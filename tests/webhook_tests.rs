use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{mpsc, Semaphore};

use sre_agents::traits::SreAgent;
use sre_agents::{AgentHandle, LogsAgent, MetricsAgent, TriageAgent};
use sre_azure::auth::MockTokenProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::PlatformConfig;
use sre_common::messages::*;
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::SqliteStore;

/// Mirrors src/webhook.rs AppState
#[derive(Clone)]
#[allow(dead_code)]
struct TestAppState {
    alert_tx: mpsc::Sender<AlertInfo>,
    store: Arc<SqliteStore>,
    heartbeat: Arc<AtomicU64>,
    investigations: Arc<Semaphore>,
    started: Arc<AtomicBool>,
    config: PlatformConfig,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn startupz(
    axum::extract::State(state): axum::extract::State<TestAppState>,
) -> axum::http::StatusCode {
    if state.started.load(Ordering::Relaxed) {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn healthz(
    axum::extract::State(state): axum::extract::State<TestAppState>,
) -> axum::http::StatusCode {
    let last = state.heartbeat.load(Ordering::Relaxed);
    let now = now_epoch_ms();
    if now.saturating_sub(last) < 30_000 {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<TestAppState>,
) -> axum::http::StatusCode {
    match state.store.ping().await {
        Ok(()) => axum::http::StatusCode::OK,
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn handle_alert(
    axum::extract::State(state): axum::extract::State<TestAppState>,
    axum::Json(payload): axum::Json<sre_azure::models::AzureCommonAlert>,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    let alert_info = sre_azure::alerts_client::AzureAlertsClient::parse_common_alert(&payload)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

    // Self-exclusion filter
    if let Some(ref self_id) = state.config.webhook.self_resource_id {
        if alert_info.affected_resource == *self_id {
            return Ok(axum::http::StatusCode::OK);
        }
    }

    match state.alert_tx.try_send(alert_info) {
        Ok(()) => Ok(axum::http::StatusCode::ACCEPTED),
        Err(mpsc::error::TrySendError::Full(_)) => Err(axum::http::StatusCode::SERVICE_UNAVAILABLE),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            Err(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

fn sample_alert_payload() -> serde_json::Value {
    serde_json::json!({
        "schemaId": "azureMonitorCommonAlertSchema",
        "data": {
            "essentials": {
                "alertId": "/subscriptions/abc/providers/Microsoft.AlertsManagement/alerts/e2e-test",
                "alertRule": "High Error Rate",
                "severity": "Sev1",
                "signalType": "Metric",
                "monitorCondition": "Fired",
                "alertTargetIDs": [
                    "/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/payments-api"
                ],
                "firedDateTime": "2026-04-06T02:00:00.000Z",
                "description": "Error rate exceeded 5%"
            },
            "alertContext": {
                "conditionType": "SingleResourceMultipleMetricCriteria",
                "condition": {
                    "allOf": [{
                        "metricName": "Http5xx",
                        "metricValue": 150.0,
                        "threshold": 10.0
                    }]
                }
            }
        }
    })
}

async fn setup_test_server() -> (String, Arc<SqliteStore>) {
    setup_test_server_with_config(PlatformConfig::default()).await
}

async fn setup_test_server_with_config(config: PlatformConfig) -> (String, Arc<SqliteStore>) {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let url = format!("sqlite://{}", db_path.display());

    let store = Arc::new(SqliteStore::new(&url).await.unwrap());
    let event_log = store.clone();

    let credential: Arc<dyn sre_azure::TokenProvider> =
        Arc::new(MockTokenProvider::new("test-token"));
    let query_client = Arc::new(AzureQueryClient::new(credential));

    let mut router = AgentRouter::new();

    // Register all agents
    let (tx, rx) = mpsc::channel(64);
    let agent = TriageAgent::new();
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    let (tx, rx) = mpsc::channel(64);
    let agent = LogsAgent::new(query_client.clone(), vec![], 1000);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    let (tx, rx) = mpsc::channel(64);
    let agent = MetricsAgent::new(query_client.clone(), vec![]);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    let mut orchestrator = Orchestrator::new(config.clone(), router, store.clone(), event_log);

    // Channel for webhook → orchestrator
    let (alert_tx, mut alert_rx) = mpsc::channel::<AlertInfo>(config.webhook.channel_capacity);

    let heartbeat = Arc::new(AtomicU64::new(now_epoch_ms()));
    let investigations = Arc::new(Semaphore::new(
        config.orchestrator.max_concurrent_investigations as usize,
    ));
    let started = Arc::new(AtomicBool::new(true));

    let app_state = TestAppState {
        alert_tx,
        store: store.clone(),
        heartbeat: heartbeat.clone(),
        investigations,
        started,
        config,
    };

    let app = Router::new()
        .route("/startupz", get(startupz))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/alerts/azure", post(handle_alert))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    // Spawn orchestrator loop with heartbeat
    let orch_heartbeat = heartbeat.clone();
    tokio::spawn(async move {
        loop {
            orch_heartbeat.store(now_epoch_ms(), Ordering::Relaxed);
            tokio::select! {
                alert = alert_rx.recv() => {
                    match alert {
                        Some(alert_info) => { let _ = orchestrator.investigate(alert_info).await; }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Keep tempdir alive
    std::mem::forget(dir);

    (base_url, store)
}

// ─── Health Probe Tests ───

#[tokio::test]
async fn test_healthz_returns_200() {
    let (base_url, _store) = setup_test_server().await;

    let resp = reqwest::get(format!("{base_url}/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_readyz_returns_200_when_db_healthy() {
    let (base_url, _store) = setup_test_server().await;

    let resp = reqwest::get(format!("{base_url}/readyz")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_startupz_returns_200() {
    let (base_url, _store) = setup_test_server().await;

    let resp = reqwest::get(format!("{base_url}/startupz")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

// ─── Webhook Tests ───

#[tokio::test]
async fn test_webhook_accepts_valid_alert() {
    let (base_url, _store) = setup_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .json(&sample_alert_payload())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn test_webhook_rejects_invalid_payload() {
    let (base_url, _store) = setup_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .json(&serde_json::json!({"garbage": true}))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn test_self_exclusion_filter() {
    let mut config = PlatformConfig::default();
    config.webhook.self_resource_id = Some(
        "/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/payments-api"
            .to_string(),
    );

    let (base_url, _store) = setup_test_server_with_config(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .json(&sample_alert_payload())
        .send()
        .await
        .unwrap();

    // Self-referential alert acknowledged with 200, NOT queued (202)
    assert_eq!(resp.status(), 200);
}

// ─── End-to-End Test ───

#[tokio::test]
async fn test_webhook_e2e_alert_to_investigation() {
    use sre_store::WorkflowStore;

    let (base_url, store) = setup_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .json(&sample_alert_payload())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 202, "Webhook should accept valid alert");

    // Wait for the orchestrator to process the alert asynchronously.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // After investigation completes, workflow reaches terminal state.
    // get_active_workflows returns only non-terminal, so it should be empty.
    let active = store.get_active_workflows().await.unwrap();
    assert!(
        active.is_empty(),
        "All investigations should have completed (found {} active)",
        active.len()
    );
}
