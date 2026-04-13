use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, Subcommand};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::TracerProvider;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

use sre_agents::{AgentHandle, LogsAgent, MetricsAgent, SreAgent, TriageAgent};
use sre_azure::auth::AzureCredentialProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::{ObservabilityConfig, PlatformConfig};
use sre_common::messages::*;
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::SqliteStore;

use crate::webhook::AppState;

mod webhook;

#[derive(Parser)]
#[command(name = "sre-agents")]
#[command(about = "SRE AI Agents Platform — Automated Incident Investigation")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Configuration file path
    #[arg(short, long, default_value = "config/default.toml")]
    config: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the webhook server to receive Azure alerts
    Serve,

    /// Manually investigate a simulated alert
    Investigate {
        /// Resource name to investigate
        #[arg(short, long)]
        resource: String,

        /// Alert severity (Sev0-Sev4)
        #[arg(short, long, default_value = "Sev2")]
        severity: String,

        /// Signal type (Metric, Log, Activity Log)
        #[arg(long, default_value = "Metric")]
        signal_type: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load configuration before tracing so OTel layer can be conditionally added
    let config = load_config(&cli.config)?;

    // Build the subscriber stack
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sre_orchestrator=debug,sre_agents=debug"));

    let fmt_layer = fmt::layer().with_target(true).with_thread_ids(false);

    let _tracer_provider = if config.observability.enabled {
        let provider = init_tracer_provider(&config.observability)?;
        let tracer = provider.tracer(config.observability.service_name.clone());
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        Registry::default()
            .with(env_filter)
            .with(fmt_layer)
            .with(otel_layer)
            .init();

        info!(
            endpoint = %config.observability.otlp_endpoint,
            service = %config.observability.service_name,
            "OpenTelemetry tracing enabled"
        );
        Some(provider)
    } else {
        Registry::default().with(env_filter).with(fmt_layer).init();
        None
    };

    let result = match cli.command {
        Commands::Serve => run_server(config).await,
        Commands::Investigate {
            resource,
            severity,
            signal_type,
        } => run_investigation(config, resource, severity, signal_type).await,
    };

    // Flush pending OTel spans before exit
    if let Some(provider) = _tracer_provider {
        if let Err(e) = provider.shutdown() {
            warn!(error = %e, "Failed to flush OpenTelemetry spans");
        }
    }

    result
}

/// Load platform configuration from TOML file with env var overrides
fn load_config(path: &str) -> anyhow::Result<PlatformConfig> {
    let builder = config::Config::builder()
        .add_source(config::File::with_name(path.trim_end_matches(".toml")).required(false))
        .add_source(config::Environment::with_prefix("SRE").separator("__"));

    let settings = builder.build()?;
    let config: PlatformConfig = settings.try_deserialize()?;

    info!("Configuration loaded from {path}");
    Ok(config)
}

/// Initialize an OpenTelemetry tracer provider with OTLP HTTP exporter
fn init_tracer_provider(obs_config: &ObservabilityConfig) -> anyhow::Result<TracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::runtime::Tokio;
    use opentelemetry_sdk::trace::Config;
    use opentelemetry_sdk::Resource;

    let provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .http()
                .with_endpoint(&obs_config.otlp_endpoint),
        )
        .with_trace_config(Config::default().with_resource(Resource::new([
            KeyValue::new("service.name", obs_config.service_name.clone()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
        ])))
        .install_batch(Tokio)?;

    Ok(provider)
}

/// Spawn a single agent and register its handle with the router.
fn spawn_agent(
    agent: impl SreAgent + 'static,
    router: &mut AgentRouter,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let (agent_tx, agent_rx) = mpsc::channel::<InboundMessage>(64);
    let handle = AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: agent_tx,
        capabilities: agent.capabilities(),
    };
    router.register(handle);

    handles.push(tokio::spawn(async move {
        if let Err(e) = agent.run(agent_rx).await {
            error!(error = %e, "Agent exited with error");
        }
    }));
}

/// Bootstrap all agents and return the orchestrator, agent join handles, and shared store
async fn bootstrap(
    config: &PlatformConfig,
) -> anyhow::Result<(
    Orchestrator,
    Vec<tokio::task::JoinHandle<()>>,
    Arc<SqliteStore>,
)> {
    // Initialize store
    let store = Arc::new(SqliteStore::new(&config.store.database_url).await?);
    let event_log = store.clone();

    // Initialize Azure credential based on config auth_mode
    let credential: Arc<dyn sre_azure::TokenProvider> = match config.azure.auth_mode.as_str() {
        "mock" => {
            info!("Using mock token provider (auth_mode=mock)");
            Arc::new(sre_azure::MockTokenProvider::new("dev-mock-token"))
        }
        _ => match AzureCredentialProvider::new() {
            Ok(cred) => Arc::new(cred),
            Err(e) => {
                info!("Azure credential not available ({e}), falling back to mock");
                Arc::new(sre_azure::MockTokenProvider::new("dev-token"))
            }
        },
    };

    let query_client = Arc::new(AzureQueryClient::new(credential.clone()));

    // Create and spawn agents
    let mut router = AgentRouter::new();
    let mut join_handles = Vec::new();

    if config.agents.triage.enabled {
        spawn_agent(TriageAgent::new(), &mut router, &mut join_handles);
    }

    if config.agents.logs.enabled {
        spawn_agent(
            LogsAgent::new(
                query_client.clone(),
                config.azure.workspace_ids.clone(),
                config.agents.logs.max_rows,
            ),
            &mut router,
            &mut join_handles,
        );
    }

    if config.agents.metrics.enabled {
        spawn_agent(
            MetricsAgent::new(query_client.clone(), config.azure.app_insights_ids.clone()),
            &mut router,
            &mut join_handles,
        );
    }

    let orchestrator = Orchestrator::new(config.clone(), router, store.clone(), event_log);

    Ok((orchestrator, join_handles, store))
}

/// Run the webhook server with orchestrator loop and health probes.
async fn run_server(config: PlatformConfig) -> anyhow::Result<()> {
    let listen_addr = format!(
        "{}:{}",
        config.webhook.listen_addr, config.webhook.listen_port
    );

    let (orchestrator, _agent_handles, store) = bootstrap(&config).await?;

    // Shared state for probes and webhook handlers
    let heartbeat = Arc::new(AtomicU64::new(webhook::now_epoch_ms()));
    let investigations = Arc::new(Semaphore::new(
        config.orchestrator.max_concurrent_investigations as usize,
    ));
    let started = Arc::new(AtomicBool::new(false));

    // Alert channel: webhook handler sends, orchestrator loop receives
    let (alert_tx, mut alert_rx) = mpsc::channel::<AlertInfo>(config.webhook.channel_capacity);

    let app_state = AppState {
        alert_tx,
        store,
        heartbeat: heartbeat.clone(),
        investigations: investigations.clone(),
        started: started.clone(),
        config: config.clone(),
    };

    let app = Router::new()
        .route("/startupz", get(webhook::startupz))
        .route("/healthz", get(webhook::healthz))
        .route("/readyz", get(webhook::readyz))
        .route("/api/v1/alerts/azure", post(webhook::handle_azure_alert))
        .layer(DefaultBodyLimit::max(64 * 1024)) // 64 KB — Azure alert payloads are typically 2-5 KB
        .with_state(app_state);

    // Arc-wrap the orchestrator so investigations can run concurrently.
    // investigate() takes &self, so multiple tasks can share the Arc safely.
    let orchestrator = Arc::new(orchestrator);

    // Heartbeat tick — independent of investigations so liveness probe
    // stays healthy even during long-running Azure API calls.
    let hb = heartbeat.clone();
    tokio::spawn(async move {
        loop {
            hb.store(webhook::now_epoch_ms(), Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Orchestrator dispatch loop — spawns each investigation as a separate
    // task, gated by a semaphore for concurrency control.
    let orch_semaphore = investigations.clone();
    let max_concurrent = config.orchestrator.max_concurrent_investigations;
    tokio::spawn(async move {
        info!("Orchestrator loop started (max_concurrent={max_concurrent})");
        while let Some(alert_info) = alert_rx.recv().await {
            let alert_id = alert_info.alert_id.clone();

            // Acquire semaphore permit before spawning investigation
            let permit = match orch_semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    error!("Investigation semaphore closed");
                    break;
                }
            };

            info!(
                alert_id = %alert_id,
                active = max_concurrent as usize - orch_semaphore.available_permits(),
                "Dispatching investigation"
            );

            let orch = orchestrator.clone();
            tokio::spawn(async move {
                match orch.investigate(alert_info).await {
                    Ok(report) => {
                        info!(
                            alert_id = %alert_id,
                            findings = report.findings.len(),
                            confidence = report.overall_confidence,
                            duration_ms = report.duration_ms,
                            "Investigation complete"
                        );
                    }
                    Err(e) => {
                        error!(alert_id = %alert_id, error = %e, "Investigation failed");
                    }
                }
                drop(permit);
            });
        }
        info!("Alert channel closed — orchestrator loop exiting");
    });

    // Mark startup complete — probes will now report healthy
    started.store(true, Ordering::Relaxed);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    info!(listen_addr = %listen_addr, "Webhook server listening");
    info!("SRE Agent Platform running. Press Ctrl+C to stop.");

    // Serve with graceful shutdown on Ctrl+C.
    // When axum shuts down, it drops all AppState clones, which drops
    // every alert_tx clone. This closes the channel and the dispatch
    // loop exits naturally (recv returns None).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            info!("Shutting down — stopping new requests...");
        })
        .await?;

    // Drain in-flight investigations: wait until all semaphore permits
    // are returned (i.e., all spawned investigations have completed).
    // ACA sends SIGTERM and waits 30s — use 25s deadline to leave margin.
    let drain_max = config.orchestrator.max_concurrent_investigations;
    info!(
        max_concurrent = drain_max,
        "Draining in-flight investigations..."
    );
    match tokio::time::timeout(
        Duration::from_secs(25),
        investigations.acquire_many(drain_max),
    )
    .await
    {
        Ok(Ok(_permits)) => info!("All investigations drained cleanly"),
        Ok(Err(_)) => info!("Investigation semaphore closed during drain"),
        Err(_) => warn!("Drain deadline exceeded — some investigations may be lost"),
    }

    Ok(())
}

/// Run a manual investigation from the CLI
async fn run_investigation(
    config: PlatformConfig,
    resource: String,
    severity: String,
    signal_type: String,
) -> anyhow::Result<()> {
    let (orchestrator, _agent_handles, _store) = bootstrap(&config).await?;

    let severity = match severity.as_str() {
        "Sev0" => AlertSeverity::Sev0,
        "Sev1" => AlertSeverity::Sev1,
        "Sev2" => AlertSeverity::Sev2,
        "Sev3" => AlertSeverity::Sev3,
        "Sev4" => AlertSeverity::Sev4,
        other => {
            anyhow::bail!("Invalid severity: {other}. Use Sev0-Sev4.");
        }
    };

    let alert = AlertInfo {
        alert_id: format!("manual-{}", uuid::Uuid::new_v4()),
        alert_rule: "Manual Investigation".to_string(),
        severity,
        signal_type,
        monitor_condition: "Fired".to_string(),
        affected_resource: resource,
        fired_at: chrono::Utc::now(),
        description: Some("Manual investigation triggered via CLI".to_string()),
        raw_context: None,
    };

    info!("Starting manual investigation...");

    match orchestrator.investigate(alert).await {
        Ok(report) => {
            let json = serde_json::to_string_pretty(&report)?;
            println!("\n{}", "=".repeat(60));
            println!("INVESTIGATION REPORT");
            println!("{}", "=".repeat(60));
            println!("{json}");
            println!("{}", "=".repeat(60));
        }
        Err(e) => {
            error!(error = %e, "Investigation failed");
        }
    }

    Ok(())
}
