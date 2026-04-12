use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use tokio::sync::{mpsc, Semaphore};
use tracing::{info, warn};

use sre_azure::alerts_client::AzureAlertsClient;
use sre_azure::models::AzureCommonAlert;
use sre_common::config::PlatformConfig;
use sre_common::messages::AlertInfo;
use sre_store::SqliteStore;

/// Shared application state for axum handlers and background tasks.
///
/// All fields are cheaply clonable (Arc-wrapped or already Clone).
/// Probe handlers read from atomic state without locks or async I/O.
#[derive(Clone)]
pub struct AppState {
    /// Sender side of the alert channel. Webhook handler sends `AlertInfo`
    /// here; the orchestrator loop receives on the other end.
    pub alert_tx: mpsc::Sender<AlertInfo>,
    /// Store reference for readiness probe (SQLite connectivity check).
    pub store: Arc<SqliteStore>,
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
}

/// Webhook handler for Azure Alert action group callbacks.
///
/// Azure sends a POST with the Common Alert Schema when an alert fires.
/// We parse it, check for self-referential alerts, and enqueue for
/// investigation. Returns 202 Accepted immediately — investigation
/// runs asynchronously in the orchestrator loop.
///
/// Expected webhook URL: POST /api/v1/alerts/azure
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

    let alert_info = match AzureAlertsClient::parse_common_alert(&payload) {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "Failed to parse alert payload");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // Self-exclusion filter: break the OTLP feedback loop (ADR-007).
    // If the alert targets the platform's own resource, acknowledge but don't investigate.
    if let Some(ref self_id) = state.config.webhook.self_resource_id {
        if alert_info.affected_resource == *self_id {
            warn!(
                alert_id = %alert_info.alert_id,
                resource = %alert_info.affected_resource,
                "Ignoring self-referential alert — breaks OTLP feedback loop"
            );
            return Ok(StatusCode::OK);
        }
    }

    info!(
        alert_id = %alert_info.alert_id,
        resource = %alert_info.affected_resource,
        severity = %alert_info.severity,
        "Alert parsed — queuing investigation"
    );

    // try_send: never block the HTTP handler. If the channel is full,
    // return 503 so Azure Action Groups retry with backoff.
    match state.alert_tx.try_send(alert_info) {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("Alert channel full — returning 503 for Azure retry");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
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

    // Check SQLite connectivity
    if state.store.ping().await.is_err() {
        reasons.push("database unreachable");
    }

    if reasons.is_empty() {
        (StatusCode::OK, Json(serde_json::json!({"ready": true}))).into_response()
    } else {
        warn!(?reasons, "Readiness check failed");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ready": false, "reasons": reasons})),
        )
            .into_response()
    }
}

pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
