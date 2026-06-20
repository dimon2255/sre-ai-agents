use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, Subcommand};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

use sre_agents::{
    AgentHandle, InfraAgent, LogsAgent, MetricsAgent, SreAgent, TraceAgent, TriageAgent,
};
use sre_azure::auth::AzureCredentialProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::{ObservabilityConfig, PlatformConfig};
use sre_common::messages::*;
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::PgStore;

use crate::middleware::hmac::{self as hmac_auth, HmacAuth};
use crate::webhook::AppState;
use tokio_util::sync::CancellationToken;

mod ingest;
mod middleware;
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
        config.observability.validate_endpoint()?;
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

        probe_otel_exporter(&provider);
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

/// Initialize an OpenTelemetry tracer provider with the OTLP gRPC exporter.
///
/// The ACA env-managed OpenTelemetry agent on `acaenv-sre-agents-staging`
/// auto-injects `OTEL_EXPORTER_OTLP_ENDPOINT` into each pod (currently
/// `http://k8se-otel.k8se-apps.svc.cluster.local:4317`). When the app
/// config leaves `otlp_endpoint` empty (the default), we let the OTel SDK
/// read that env var. If the app config sets a non-empty value, it takes
/// precedence (useful for local dev pointing at a custom collector).
///
/// Do NOT hardcode `http://localhost:4317` — that points at nothing in an
/// ACA pod and the export will silently fail with `Connection refused`.
fn init_tracer_provider(obs_config: &ObservabilityConfig) -> anyhow::Result<SdkTracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;

    let mut attrs = vec![
        KeyValue::new("service.name", obs_config.service_name.clone()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
        KeyValue::new("deployment.environment", obs_config.environment.clone()),
    ];
    // ACA's env-managed OTel agent emits the literal placeholder
    // `$(CONTAINER_APP_REPLICA_NAME)` for service.instance.id otherwise.
    if let Ok(replica) = std::env::var("CONTAINER_APP_REPLICA_NAME") {
        if !replica.is_empty() {
            attrs.push(KeyValue::new("service.instance.id", replica));
        }
    }
    let resource = Resource::builder().with_attributes(attrs).build();

    let mut builder = SpanExporter::builder().with_tonic();
    let endpoint = obs_config.otlp_endpoint.trim_end_matches('/');
    if !endpoint.is_empty() {
        builder = builder.with_endpoint(endpoint);
    }
    let grpc_exporter = builder.build()?;

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(grpc_exporter)
        .build();

    Ok(provider)
}

/// Emit one synthetic span at startup and force-flush the exporter.
///
/// Same startup check as the `probeExporter` pattern common in Go OTel
/// services. Without this, silent export
/// failures — e.g. wrong endpoint, unreachable agent — only surface
/// hours later as "no spans in App Insights."
fn probe_otel_exporter(provider: &SdkTracerProvider) {
    use opentelemetry::trace::{Tracer, TracerProvider as _};

    let tracer = provider.tracer("telemetry.bootstrap");
    tracer.in_span("otel.bootstrap.probe", |_| {});

    match provider.force_flush() {
        Ok(()) => info!("OpenTelemetry boot probe flushed successfully"),
        Err(e) => warn!(
            error = %e,
            "OpenTelemetry boot probe flush failed — spans may be silently dropped"
        ),
    }
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
) -> anyhow::Result<(Orchestrator, Vec<tokio::task::JoinHandle<()>>, Arc<PgStore>)> {
    // Initialize store
    let store = Arc::new(PgStore::new(&config.store.database_url).await?);
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

    if config.agents.trace.enabled {
        spawn_agent(
            TraceAgent::new(
                query_client.clone(),
                config.azure.app_insights_ids.clone(),
                config.agents.trace.max_rows,
            ),
            &mut router,
            &mut join_handles,
        );
    }

    if config.agents.infra.enabled {
        spawn_agent(
            InfraAgent::new(
                query_client.clone(),
                config.azure.workspace_ids.clone(),
                config.agents.infra.max_rows,
            ),
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

    // Clone retained for the NATS ingestor (ADR-013) — must capture before
    // `alert_tx` is moved into `AppState` for axum's `.with_state(...)`.
    let nats_alert_tx = alert_tx.clone();

    // Shared NATS ingestor state for `/readyz` reporting (ADR-013).
    // Disabled by default; the spawn branch promotes it to Connecting.
    let nats_status = Arc::new(AtomicU8::new(
        ingest::nats::NatsIngestorState::Disabled as u8,
    ));

    let app_state = AppState {
        alert_tx,
        store,
        heartbeat: heartbeat.clone(),
        investigations: investigations.clone(),
        started: started.clone(),
        config: config.clone(),
        nats_status: nats_status.clone(),
    };

    // HMAC auth is applied ONLY to the alert route (ADR-008). Health probes
    // stay unauthenticated so ACA can hit them. If no secret is configured,
    // the middleware is not mounted and the webhook runs unauthenticated —
    // a loud warning is logged. Staging and production must set the secret.
    let alert_routes = {
        let r = Router::new().route("/api/v1/alerts/azure", post(webhook::handle_azure_alert));
        match config.webhook.hmac_secret.as_ref() {
            Some(secret) if !secret.is_empty() => {
                let auth = HmacAuth::new(
                    secret.clone().into_bytes(),
                    config.webhook.hmac_skew_seconds,
                );
                r.layer(axum::middleware::from_fn_with_state(
                    auth,
                    hmac_auth::verify,
                ))
            }
            _ => {
                warn!(
                    "SRE_WEBHOOK_SECRET is not set — webhook authentication is DISABLED. \
                     Do not run this configuration in staging or production."
                );
                r
            }
        }
    };

    let app = Router::new()
        .route("/startupz", get(webhook::startupz))
        .route("/healthz", get(webhook::healthz))
        .route("/readyz", get(webhook::readyz))
        .merge(alert_routes)
        .layer(DefaultBodyLimit::max(64 * 1024)) // 64 KB — Azure alert payloads are typically 2-5 KB
        .with_state(app_state);

    // Arc-wrap the orchestrator so investigations can run concurrently.
    // investigate() takes &self, so multiple tasks can share the Arc safely.
    let orchestrator = Arc::new(orchestrator);

    // Shared cancellation token (ADR-013).
    // - tripped by Ctrl+C in axum's with_graceful_shutdown closure
    // - watched by the NATS ingestor for graceful drain
    let cancel = CancellationToken::new();

    // Spawn the NATS JetStream ingestor when enabled (ADR-013).
    // Both ingestion paths terminate at the same `alert_tx` channel; the
    // webhook continues to receive alerts even if NATS is disabled or down.
    //
    // Two startup gates run before the task is spawned:
    //   1. Credentials policy (#11) — refuse to start if running outside a
    //      dev environment without a credentials file. Anonymous connections
    //      are local-dev only.
    //   2. Preflight (#10) — bounded connection test, so a typo in the URL
    //      or an unreachable server fails fast instead of hanging quietly
    //      in the background while the rest of the platform looks healthy.
    let nats_join = if config.nats.enabled {
        enforce_nats_credentials_policy(&config)?;
        ingest::nats::preflight(&config.nats)
            .await
            .context("NATS preflight failed — refusing to start ingestor")?;

        let nats_cfg = config.nats.clone();
        let platform_cfg = config.clone();
        let nats_cancel = cancel.clone();
        let nats_status_task = nats_status.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = ingest::nats::run(
                nats_cfg,
                platform_cfg,
                nats_alert_tx,
                nats_cancel,
                nats_status_task,
            )
            .await
            {
                error!(error = %e, "NATS ingestor exited with error — webhook path remains active");
            }
        }))
    } else {
        // Drop the unused clone so the channel sender count stays accurate.
        drop(nats_alert_tx);
        info!("NATS ingestion disabled (nats.enabled=false) — webhook is the only ingestion path");
        None
    };

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
    //
    // Before axum drains, we trip the shared cancellation token (ADR-013)
    // so the NATS ingestor stops fetching new messages and drops its
    // alert_tx clone. Without this, the dispatch loop would stay alive
    // as long as the NATS task held the channel open.
    let shutdown_cancel = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Shutdown signal received — tripping cancellation token");
            shutdown_cancel.cancel();
        })
        .await?;

    // Drain NATS ingestor and in-flight investigations concurrently under a
    // single shared deadline. ACA gives us 30s after SIGTERM before it
    // SIGKILLs the container; budgeting them serially (10s + 25s) would
    // overrun the grace window. Both drains are independent — NATS is
    // already stopping fetching, and investigations finish on their own
    // tasks — so running them in parallel is strictly faster.
    let drain_max = config.orchestrator.max_concurrent_investigations;
    info!(
        max_concurrent = drain_max,
        "Draining NATS ingestor and in-flight investigations..."
    );

    let nats_drain = async {
        if let Some(join) = nats_join {
            match join.await {
                Ok(()) => info!("NATS ingestor stopped cleanly"),
                Err(e) => warn!(error = %e, "NATS ingestor task panicked during drain"),
            }
        }
    };
    let investigations_drain = async {
        match investigations.acquire_many(drain_max).await {
            Ok(_permits) => info!("All investigations drained cleanly"),
            Err(_) => info!("Investigation semaphore closed during drain"),
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(25),
        futures_util::future::join(nats_drain, investigations_drain),
    )
    .await
    {
        Ok(_) => info!("Shutdown drains completed within deadline"),
        Err(_) => warn!(
            "Shutdown drain deadline exceeded — NATS ingestor or in-flight \
             investigations may not have completed cleanly"
        ),
    }

    Ok(())
}

/// Refuse to start when running outside dev with NATS enabled and no
/// credentials configured (#11). Anonymous connections are a local-dev
/// affordance only — staging and production must use NKEY/JWT.
fn enforce_nats_credentials_policy(config: &PlatformConfig) -> anyhow::Result<()> {
    if !config.nats.enabled {
        return Ok(());
    }
    if config.nats.credentials_path.is_some() {
        return Ok(());
    }

    // `observability.environment` is the canonical deployment-environment
    // signal already wired through Terraform (`SRE__OBSERVABILITY__ENVIRONMENT`).
    // Reuse it here rather than introducing a parallel notion of "env".
    let env = config.observability.environment.to_lowercase();
    let dev_envs = ["dev", "development", "local", "test"];
    if dev_envs.iter().any(|d| env == *d) {
        return Ok(());
    }

    anyhow::bail!(
        "NATS is enabled in environment='{}' but nats.credentials_path is not set. \
         Anonymous NATS connections are permitted only in dev/local. Set \
         SRE__NATS__CREDENTIALS_PATH or disable NATS (SRE__NATS__ENABLED=false).",
        config.observability.environment
    )
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
