use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Form, Json};
use chrono::Utc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

use sre_azure::models::AzureCommonAlert;
use sre_chatops::{ChatBackend, ChatOpsHandler, InvestigationStatus};
use sre_common::config::PlatformConfig;
use sre_common::findings::InvestigationReport;
use sre_common::messages::{AlertInfo, AlertSeverity};
use sre_store::models::WorkflowRecord;
use sre_store::traits::WorkflowStore;
use sre_store::PgStore;

use crate::ingest::nats::NatsIngestorState;
use crate::ingest::{parse_and_enqueue, IngestError, IngestOutcome};

/// Shared application state for axum handlers and background tasks.
///
/// All fields are cheaply clonable (Arc-wrapped or already Clone).
/// Probe handlers read from atomic state without locks or async I/O.
#[derive(Clone)]
pub struct AppState {
    /// Sender side of the alert channel. Webhook handler sends `AlertInfo`
    /// here; the orchestrator loop receives on the other end.
    pub alert_tx: mpsc::Sender<AlertInfo>,
    /// Store reference for readiness probe (Postgres connectivity check).
    pub store: Arc<PgStore>,
    /// Epoch milliseconds of the orchestrator loop's last iteration.
    /// Written by the orchestrator loop, read by liveness/readiness probes.
    pub heartbeat: Arc<AtomicU64>,
    /// Counting semaphore limiting concurrent investigations.
    /// Permits = `config.orchestrator.max_concurrent_investigations`.
    pub investigations: Arc<Semaphore>,
    /// Flipped to `true` once after bootstrap completes.
    /// Read by the startup probe.
    pub started: Arc<AtomicBool>,
    /// Platform configuration (for self_resource_id check).
    pub config: PlatformConfig,
    /// Current NATS ingestor state (ADR-013). Reported by `/readyz` for
    /// operator visibility but never fails the probe — the webhook is the
    /// safety net, so a NATS outage must not stop traffic.
    pub nats_status: Arc<AtomicU8>,
}

/// Webhook handler for Azure Alert action group callbacks.
///
/// Azure sends a POST with the Common Alert Schema when an alert fires.
/// We parse it, check for self-referential alerts, and enqueue for
/// investigation. Returns 202 Accepted immediately — investigation
/// runs asynchronously in the orchestrator loop.
///
/// Expected webhook URL: POST /api/v1/alerts/azure
#[tracing::instrument(
    name = "handle_azure_alert",
    skip(state, payload),
    fields(
        alert_rule = %payload.data.essentials.alert_rule,
        severity = %payload.data.essentials.severity,
        signal_type = %payload.data.essentials.signal_type,
        affected_resource = tracing::field::Empty,
        alert_id = tracing::field::Empty,
    )
)]
pub async fn handle_azure_alert(
    State(state): State<AppState>,
    Json(payload): Json<AzureCommonAlert>,
) -> Result<StatusCode, StatusCode> {
    info!(
        alert_rule = %payload.data.essentials.alert_rule,
        severity = %payload.data.essentials.severity,
        signal_type = %payload.data.essentials.signal_type,
        "Received Azure alert webhook"
    );

    let span = tracing::Span::current();
    if let Some(target) = payload.data.essentials.alert_target_i_ds.first() {
        span.record("affected_resource", tracing::field::display(target));
    }
    span.record(
        "alert_id",
        tracing::field::display(&payload.data.essentials.alert_id),
    );

    match parse_and_enqueue(&payload, &state.alert_tx, &state.config) {
        Ok(IngestOutcome::Accepted) => Ok(StatusCode::ACCEPTED),
        Ok(IngestOutcome::Ignored) => Ok(StatusCode::OK),
        Err(IngestError::Parse(reason)) => {
            warn!(error = %reason, "Failed to parse alert payload");
            Err(StatusCode::BAD_REQUEST)
        }
        Err(IngestError::ChannelFull) => {
            warn!("Alert channel full — returning 503 for Azure retry");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(IngestError::ChannelClosed) => {
            warn!("Alert channel closed — orchestrator loop has stopped");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

// ─── Health Probes ───

/// Startup probe — has the platform finished bootstrapping?
/// ACA behaviour on failure: keeps waiting, no traffic routed, no kill.
pub async fn startupz(State(state): State<AppState>) -> StatusCode {
    if state.started.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Liveness probe — is the orchestrator loop fundamentally alive?
/// ACA behaviour on failure: **kills and restarts** the container.
///
/// Checks only the heartbeat timestamp. No I/O, no DB, no channel probe —
/// a liveness check that does I/O can false-positive during transient
/// storage issues, causing unnecessary restarts.
pub async fn healthz(State(state): State<AppState>) -> StatusCode {
    let last_heartbeat = state.heartbeat.load(Ordering::Relaxed);
    let now = now_epoch_ms();

    if now.saturating_sub(last_heartbeat) < 30_000 {
        StatusCode::OK
    } else {
        warn!(
            last_heartbeat,
            now, "Liveness check failed — orchestrator heartbeat stale"
        );
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Readiness probe — can the service accept new alerts right now?
/// ACA behaviour on failure: **stops routing traffic**, container stays alive.
/// Returns JSON body with reasons on failure so operators can diagnose.
///
/// NATS ingestor state (ADR-013) is reported in the body for visibility
/// but never affects the probe outcome — the webhook is the safety net.
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let mut reasons: Vec<&str> = Vec::new();

    // Check channel capacity
    if state.alert_tx.capacity() == 0 {
        reasons.push("alert channel full");
    }

    // Check investigation concurrency
    if state.investigations.available_permits() == 0 {
        reasons.push("max investigations reached");
    }

    // Check orchestrator heartbeat (tighter than liveness — 10s vs 30s)
    let last_heartbeat = state.heartbeat.load(Ordering::Relaxed);
    let now = now_epoch_ms();
    if now.saturating_sub(last_heartbeat) > 10_000 {
        reasons.push("orchestrator heartbeat stale");
    }

    // Check Postgres connectivity
    if state.store.ping().await.is_err() {
        reasons.push("database unreachable");
    }

    let nats_state = NatsIngestorState::from_u8(state.nats_status.load(Ordering::Relaxed));

    if reasons.is_empty() {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "ready": true,
                "nats": nats_state.as_str(),
            })),
        )
            .into_response()
    } else {
        warn!(?reasons, nats = %nats_state.as_str(), "Readiness check failed");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ready": false,
                "reasons": reasons,
                "nats": nats_state.as_str(),
            })),
        )
            .into_response()
    }
}

// ─── ChatOps (Phase 3, ADR-018) ───

/// Slack slash-command form payload. Slack POSTs `application/x-www-form-urlencoded`
/// with the user's text in the `text` field (the `/command` itself is separate).
#[derive(Debug, serde::Deserialize)]
pub struct ChatOpsForm {
    #[serde(default)]
    pub text: String,
}

/// [`ChatBackend`] over the platform: triggers via the alert channel (same path
/// as the webhook — fire-and-forget into the orchestrator loop) and reads status
/// from the store. Built per request from [`AppState`].
struct WebhookChatBackend {
    alert_tx: mpsc::Sender<AlertInfo>,
    store: Arc<PgStore>,
}

#[async_trait]
impl ChatBackend for WebhookChatBackend {
    async fn trigger_investigation(
        &self,
        resource: &str,
        severity: AlertSeverity,
    ) -> Result<String, String> {
        let alert_id = format!("chatops-{}", Uuid::new_v4());
        let alert = AlertInfo {
            alert_id: alert_id.clone(),
            alert_rule: format!("ChatOps investigate: {resource}"),
            severity,
            signal_type: "ChatOps".to_string(),
            monitor_condition: "Fired".to_string(),
            affected_resource: resource.to_string(),
            fired_at: Utc::now(),
            description: Some("Investigation triggered via ChatOps".to_string()),
            raw_context: None,
        };
        self.alert_tx.try_send(alert).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                "alert queue is full — try again shortly".to_string()
            }
            mpsc::error::TrySendError::Closed(_) => "the platform is shutting down".to_string(),
        })?;
        Ok(alert_id)
    }

    async fn get_status(
        &self,
        investigation_id: &str,
    ) -> Result<Option<InvestigationStatus>, String> {
        // Accept either a workflow UUID (from notifications/logs) or a ChatOps
        // tracking reference (the alert id returned by `trigger`).
        let record = if let Ok(uuid) = Uuid::parse_str(investigation_id) {
            self.store.get_workflow(&uuid).await
        } else {
            self.store.get_workflow_by_alert_id(investigation_id).await
        }
        .map_err(|e| e.to_string())?;
        Ok(record.map(record_to_status))
    }
}

/// Map a stored workflow into the ChatOps status view, pulling the summary +
/// confidence out of the persisted report when the investigation has finished.
fn record_to_status(record: WorkflowRecord) -> InvestigationStatus {
    let report: Option<InvestigationReport> = record
        .report_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    InvestigationStatus {
        id: record.id.to_string(),
        state: record.state.to_string(),
        affected_resource: record.affected_resource,
        severity: record.severity.as_label().to_string(),
        summary: report.as_ref().map(|r| r.summary.clone()),
        confidence: report.as_ref().map(|r| r.overall_confidence),
        escalated: report.as_ref().map(|r| r.escalated).unwrap_or(false),
    }
}

/// ChatOps slash-command endpoint: `POST /api/v1/chatops`. Parses the command,
/// dispatches it (trigger an investigation / query status), and returns a Slack
/// slash-command JSON response.
pub async fn handle_chatops(
    State(state): State<AppState>,
    Form(form): Form<ChatOpsForm>,
) -> impl IntoResponse {
    let backend = Arc::new(WebhookChatBackend {
        alert_tx: state.alert_tx.clone(),
        store: state.store.clone(),
    });
    let response = ChatOpsHandler::new(backend).handle(&form.text).await;
    Json(response)
}

pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
