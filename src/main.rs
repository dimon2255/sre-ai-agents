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
use sre_orchestrator::{AgentRouter, Orchestrator, RemediationPipeline};
use sre_store::PgStore;

use crate::middleware::hmac::{self as hmac_auth, HmacAuth};
use crate::webhook::AppState;
use tokio_util::sync::CancellationToken;

mod grpc;
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
    // reqwest is built without a crypto provider (workspace Cargo.toml); install ring
    // before anything constructs a TLS config.
    sre_common::http::ensure_crypto_provider();
    let cli = Cli::parse();

    // Load configuration before tracing so OTel layer can be conditionally added
    let config = load_config(&cli.config)?;

    // Build the subscriber stack
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sre_orchestrator=debug,sre_agents=debug"));

    let fmt_layer = fmt::layer().with_target(true).with_thread_ids(false);

    let (_tracer_provider, _meter_provider) = if config.observability.enabled {
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

        // Metrics pipeline (ADR-013 PR-2): install a global MeterProvider so
        // the NATS ingestor's instruments export over the same OTLP endpoint.
        let meter_provider = init_meter_provider(&config.observability)?;
        opentelemetry::global::set_meter_provider(meter_provider.clone());
        info!("OpenTelemetry metrics enabled");

        (Some(provider), Some(meter_provider))
    } else {
        Registry::default().with(env_filter).with(fmt_layer).init();
        (None, None)
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

    // Flush pending OTel metrics before exit
    if let Some(provider) = _meter_provider {
        if let Err(e) = provider.shutdown() {
            warn!(error = %e, "Failed to flush OpenTelemetry metrics");
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
/// An Azure Container Apps environment with the env-managed OpenTelemetry
/// agent enabled auto-injects `OTEL_EXPORTER_OTLP_ENDPOINT` into each pod (currently
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

/// Initialize an OpenTelemetry meter provider with the OTLP gRPC exporter.
///
/// Mirrors [`init_tracer_provider`] for the metrics signal. A periodic reader
/// (default 60s push interval) exports the NATS ingestor's instruments
/// (`nats_consumer_pending`, `nats_redeliveries_total`, `nats_dlq_total`) over
/// the same OTLP endpoint as traces. Endpoint resolution follows the same rule
/// as traces: an empty config value lets the SDK read
/// `OTEL_EXPORTER_OTLP_ENDPOINT` (auto-injected by the ACA env-managed agent).
fn init_meter_provider(
    obs_config: &ObservabilityConfig,
) -> anyhow::Result<opentelemetry_sdk::metrics::SdkMeterProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{MetricExporter, WithExportConfig};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::Resource;

    let resource = Resource::builder()
        .with_attributes(vec![
            KeyValue::new("service.name", obs_config.service_name.clone()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
            KeyValue::new("deployment.environment", obs_config.environment.clone()),
        ])
        .build();

    let mut builder = MetricExporter::builder().with_tonic();
    let endpoint = obs_config.otlp_endpoint.trim_end_matches('/');
    if !endpoint.is_empty() {
        builder = builder.with_endpoint(endpoint);
    }
    let exporter = builder.build()?;

    let provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(exporter)
        .build();

    Ok(provider)
}

/// Emit one synthetic span at startup and force-flush the exporter.
///
/// Without this, silent export failures — e.g. wrong endpoint, unreachable
/// agent — only surface hours later as "no spans in App Insights."
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

    let mut orchestrator = Orchestrator::new(config.clone(), router, store.clone(), event_log);

    // Gated-remediation pipeline (Phase 2c, ADR-009). Opt-in; when enabled the
    // orchestrator runs propose -> gate -> execute after each investigation.
    if config.remediation.enabled {
        let arm: Arc<dyn sre_azure::ArmWriteClient> = match config.azure.auth_mode.as_str() {
            "mock" => {
                info!("Remediation using mock ARM write client (auth_mode=mock)");
                Arc::new(sre_azure::MockArmClient::new())
            }
            _ => Arc::new(sre_azure::AzureArmClient::new(credential.clone())),
        };
        let guardrails = sre_gates::GuardrailConfig {
            kill_switch: config.remediation.kill_switch,
            per_subscription_hourly_limit: config.remediation.per_subscription_hourly_limit,
            per_resource_type_hourly_limit: config.remediation.per_resource_type_hourly_limit,
            auto_approve_min_confidence: config.remediation.auto_approve_min_confidence,
            max_auto_approve_blast_radius: 1,
            business_hours_only: config.remediation.business_hours_only,
        };
        let gate = Arc::new(sre_gates::ApprovalGate::new(guardrails));
        let subscription_id = config
            .azure
            .subscription_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| config.remediation.subscription_id.clone());
        let mut pipeline = RemediationPipeline::new(arm, gate, subscription_id);
        // Progressive-trust auto-remediation (Phase 3, ADR-017). Opt-in on top of
        // remediation: trusted categories earn a lower auto-approve floor over
        // consecutive successes; a failure resets + cools down the category.
        if config.remediation.progressive_trust {
            pipeline = pipeline.with_trust(Arc::new(sre_gates::TrustModel::with_defaults()));
            info!("Progressive-trust auto-remediation enabled (ADR-017)");
        }
        orchestrator = orchestrator.with_remediation(pipeline);
        info!("Gated remediation pipeline enabled (ADR-009)");
    }

    // LLM reasoning engine (Phase 3, ADR-015). Opt-in; synthesizes an RCA after
    // each investigation. Real Claude when ANTHROPIC_API_KEY is set, else the
    // deterministic mock engine.
    if let Some(engine) = sre_intelligence::build_engine(&config.reasoning) {
        orchestrator = orchestrator.with_reasoning(engine);
        info!("LLM reasoning engine enabled (ADR-015)");
    }

    // Knowledge store + learning loop (Phase 3, ADR-016). Opt-in. The knowledge
    // store grounds the reasoning engine with similar past incidents (RAG); the
    // learning loop embeds each completed incident back into it. Defaults to the
    // deterministic in-memory mock so RAG works offline (provider = "qdrant" for
    // the real vector DB).
    if let Some(store) = sre_intelligence::build_knowledge_store(&config.knowledge) {
        if let Err(e) = store.ensure_ready().await {
            warn!(error = %e, "Knowledge store not ready — RAG/learning will be best-effort");
        }
        let learning = Arc::new(sre_intelligence::LearningLoop::new(store.clone()));
        orchestrator = orchestrator.with_knowledge(store).with_learning(learning);
        info!("Knowledge store + learning loop enabled (ADR-016)");
    }

    Ok((orchestrator, join_handles, store))
}

/// Run the webhook server with orchestrator loop and health probes.
async fn run_server(config: PlatformConfig) -> anyhow::Result<()> {
    // Fail-closed production safety gates. Checked before any resource is
    // touched so a misconfiguration is a fast, loud boot failure rather than a
    // silently-degraded running service. Dev environments are exempted.
    enforce_webhook_auth_policy(&config)?;
    enforce_auth_mode_policy(&config)?;
    enforce_remediation_safety_policy(&config)?;

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

    // Clone the store for the gRPC service before it is moved into AppState.
    let grpc_store = store.clone();

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

    // ChatOps slash-command endpoint (Phase 3, ADR-018). Opt-in; lets operators
    // trigger investigations and query status from Slack/Teams. Mounted only when
    // enabled; relies on ingress isolation (Slack request signing is a forward step).
    let chatops_routes = if config.chatops.enabled {
        info!("ChatOps endpoint enabled (ADR-018): POST /api/v1/chatops");
        Router::new().route("/api/v1/chatops", post(webhook::handle_chatops))
    } else {
        Router::new()
    };

    let app = Router::new()
        .route("/startupz", get(webhook::startupz))
        .route("/healthz", get(webhook::healthz))
        .route("/readyz", get(webhook::readyz))
        .merge(alert_routes)
        .merge(chatops_routes)
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

    // gRPC API server (Phase 2b). Runs on its own port, shares the same
    // orchestrator + store, and is cancelled on shutdown via the shared token.
    let grpc_join = if config.grpc.enabled {
        let addr: std::net::SocketAddr =
            format!("{}:{}", config.grpc.listen_addr, config.grpc.listen_port)
                .parse()
                .context("invalid gRPC listen address")?;
        let service = grpc::InvestigationGrpc::new(orchestrator.clone(), grpc_store).into_server();
        let grpc_cancel = cancel.clone();
        info!(grpc_addr = %addr, "gRPC server listening");
        Some(tokio::spawn(async move {
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_shutdown(addr, async move {
                    grpc_cancel.cancelled().await;
                })
                .await
            {
                error!(error = %e, "gRPC server error");
            }
        }))
    } else {
        info!("gRPC API disabled (grpc.enabled=false)");
        None
    };

    // NOTE: the liveness heartbeat is owned by the orchestrator dispatch loop
    // below — NOT a free-running timer. A free-running timer keeps the liveness
    // probe green even when the dispatch loop is dead or wedged; coupling the
    // heartbeat to the loop means such a wedge goes stale and ACA restarts us.

    // Notification engine (Phase 2b): delivers reports/escalations to
    // Slack/Teams/PagerDuty. `None` when notifications are disabled.
    let notification_engine =
        sre_notify::NotificationEngine::from_config(&config.notifications).map(Arc::new);
    if notification_engine.is_some() {
        info!("Notification engine enabled");
    }

    // Orchestrator dispatch loop — spawns each investigation as a separate
    // task, gated by a semaphore for concurrency control. The loop also owns the
    // liveness heartbeat: it stamps `heartbeat` on every alert and on a periodic
    // tick, so if this task dies or wedges the timestamp goes stale and the
    // liveness probe restarts the container.
    let orch_semaphore = investigations.clone();
    let max_concurrent = config.orchestrator.max_concurrent_investigations;
    let hb = heartbeat.clone();
    tokio::spawn(async move {
        info!("Orchestrator loop started (max_concurrent={max_concurrent})");
        let mut heartbeat_tick = tokio::time::interval(Duration::from_secs(5));
        heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Wait for the next alert while keeping the heartbeat fresh during
            // idle periods (recv is a legitimate idle wait, not a wedge).
            let alert_info = tokio::select! {
                _ = heartbeat_tick.tick() => {
                    hb.store(webhook::now_epoch_ms(), Ordering::Relaxed);
                    continue;
                }
                maybe_alert = alert_rx.recv() => {
                    match maybe_alert {
                        Some(a) => {
                            hb.store(webhook::now_epoch_ms(), Ordering::Relaxed);
                            a
                        }
                        None => break,
                    }
                }
            };

            let alert_id = alert_info.alert_id.clone();
            let notifier = notification_engine.clone();

            // Acquire a permit before spawning. Keep ticking the heartbeat while
            // waiting so a legitimately-full semaphore under sustained load is
            // not mistaken for a dead loop.
            let permit = loop {
                tokio::select! {
                    _ = heartbeat_tick.tick() => {
                        hb.store(webhook::now_epoch_ms(), Ordering::Relaxed);
                    }
                    acquired = orch_semaphore.clone().acquire_owned() => {
                        match acquired {
                            Ok(p) => break p,
                            Err(_) => {
                                error!("Investigation semaphore closed — orchestrator loop exiting");
                                return;
                            }
                        }
                    }
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
                        if let Some(engine) = notifier {
                            let delivered = engine.notify_report(&report).await;
                            if delivered > 0 {
                                info!(alert_id = %alert_id, delivered, "Notifications delivered");
                            }
                        }
                        let remediated = orch.remediate(&report).await;
                        if remediated > 0 {
                            info!(alert_id = %alert_id, remediated, "Remediation proposals processed");
                        }
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
            shutdown_signal().await;
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
    let grpc_drain = async {
        if let Some(join) = grpc_join {
            match join.await {
                Ok(()) => info!("gRPC server stopped cleanly"),
                Err(e) => warn!(error = %e, "gRPC server task panicked during drain"),
            }
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(25),
        futures_util::future::join3(nats_drain, investigations_drain, grpc_drain),
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
    if is_dev_environment(config) {
        return Ok(());
    }

    anyhow::bail!(
        "NATS is enabled in environment='{}' but nats.credentials_path is not set. \
         Anonymous NATS connections are permitted only in dev/local. Set \
         SRE__NATS__CREDENTIALS_PATH or disable NATS (SRE__NATS__ENABLED=false).",
        config.observability.environment
    )
}

/// Whether the platform is running in a development-class environment. Dev
/// environments relax the fail-closed policies (mock auth, unauthenticated
/// webhook, anonymous NATS) that must never be permitted in staging/production.
/// Real deployments set `SRE__OBSERVABILITY__ENVIRONMENT` explicitly.
fn is_dev_environment(config: &PlatformConfig) -> bool {
    matches!(
        config.observability.environment.to_lowercase().as_str(),
        "dev" | "development" | "local" | "test"
    )
}

/// Refuse to boot with an unauthenticated webhook outside dev. The webhook is
/// the primary ingress and reaches the investigate (and, when enabled, the
/// remediate) path, so a missing HMAC secret in staging/prod is a hard stop,
/// not a warning. Mirrors [`enforce_nats_credentials_policy`].
fn enforce_webhook_auth_policy(config: &PlatformConfig) -> anyhow::Result<()> {
    if is_dev_environment(config) {
        return Ok(());
    }
    let has_secret = config
        .webhook
        .hmac_secret
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if !has_secret {
        anyhow::bail!(
            "webhook.hmac_secret is required in environment='{}' but is not set. \
             Set SRE__WEBHOOK__HMAC_SECRET (>= 32 bytes) or run in a dev \
             environment. Refusing to start an unauthenticated webhook.",
            config.observability.environment
        );
    }
    Ok(())
}

/// Refuse to boot with mock Azure auth outside dev. `auth_mode="mock"` returns a
/// fake token, so every KQL query is meaningless and (with remediation enabled)
/// ARM writes route to a mock client — a silent no-op that still looks healthy.
fn enforce_auth_mode_policy(config: &PlatformConfig) -> anyhow::Result<()> {
    if is_dev_environment(config) {
        return Ok(());
    }
    if config.azure.auth_mode.eq_ignore_ascii_case("mock") {
        anyhow::bail!(
            "azure.auth_mode=\"mock\" is not permitted in environment='{}'. \
             Set SRE__AZURE__AUTH_MODE=azure. Refusing to start with fake Azure \
             credentials outside a dev environment.",
            config.observability.environment
        );
    }
    Ok(())
}

/// Refuse to enable remediation against real Azure until identity separation
/// (ADR-011) and durable multi-replica coordination (ADR-021) land. The write
/// path currently reuses the read credential and the approval gate's
/// rate-limit / mutual-exclusion state is per-replica, so auto-remediation on
/// real Azure is unsafe. Fail closed rather than trust a config comment.
fn enforce_remediation_safety_policy(config: &PlatformConfig) -> anyhow::Result<()> {
    if !config.remediation.enabled {
        return Ok(());
    }
    // Mock auth uses the mock ARM client (no real mutation) — safe for dev/test.
    if config.azure.auth_mode.eq_ignore_ascii_case("mock") {
        return Ok(());
    }
    anyhow::bail!(
        "remediation.enabled=true with auth_mode='{}' is blocked. The remediation \
         write path reuses the read credential (ADR-011 identity separation \
         not yet implemented) and the approval gate's rate limits and resource \
         locks are per-replica (ADR-021 durable coordination not yet implemented), \
         so auto-remediation against real Azure is unsafe. Keep \
         SRE__REMEDIATION__ENABLED=false until a dedicated remediation identity \
         and durable coordination are in place.",
        config.azure.auth_mode
    );
}

/// Completes when the process receives a shutdown signal: Ctrl-C (SIGINT) on
/// any platform, or SIGTERM on Unix. ACA / Kubernetes send SIGTERM before the
/// grace-period SIGKILL, so handling it is what makes the graceful drain run —
/// previously only SIGINT was handled, so the drain was dead code on ACA.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "Failed to install SIGTERM handler — SIGTERM will not drain");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT (Ctrl-C) — starting graceful shutdown"),
        _ = terminate => info!("Received SIGTERM — starting graceful shutdown"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(env: &str) -> PlatformConfig {
        let mut c = PlatformConfig::default();
        c.observability.environment = env.to_string();
        c
    }

    #[test]
    fn dev_environment_detection() {
        assert!(is_dev_environment(&cfg("development")));
        assert!(is_dev_environment(&cfg("local")));
        assert!(is_dev_environment(&cfg("TEST"))); // case-insensitive
        assert!(!is_dev_environment(&cfg("staging")));
        assert!(!is_dev_environment(&cfg("production")));
    }

    #[test]
    fn webhook_auth_fails_closed_outside_dev() {
        // Production with no HMAC secret must refuse to boot.
        let mut prod = cfg("production");
        prod.webhook.hmac_secret = None;
        assert!(enforce_webhook_auth_policy(&prod).is_err());

        // A configured secret lets it boot.
        prod.webhook.hmac_secret = Some("a-32-byte-or-longer-shared-secret!".to_string());
        assert!(enforce_webhook_auth_policy(&prod).is_ok());

        // Dev is exempt even with no secret (local ergonomics).
        let mut dev = cfg("development");
        dev.webhook.hmac_secret = None;
        assert!(enforce_webhook_auth_policy(&dev).is_ok());
    }

    #[test]
    fn mock_auth_rejected_outside_dev() {
        let mut prod = cfg("production");
        prod.azure.auth_mode = "mock".to_string();
        assert!(enforce_auth_mode_policy(&prod).is_err());

        prod.azure.auth_mode = "azure".to_string();
        assert!(enforce_auth_mode_policy(&prod).is_ok());

        // Dev may use the mock token provider.
        let mut dev = cfg("development");
        dev.azure.auth_mode = "mock".to_string();
        assert!(enforce_auth_mode_policy(&dev).is_ok());
    }

    #[test]
    fn remediation_blocked_against_real_azure() {
        let mut prod = cfg("production");
        prod.remediation.enabled = true;
        prod.azure.auth_mode = "azure".to_string();
        // ADR-011 / ADR-021 not yet implemented → must fail closed.
        assert!(enforce_remediation_safety_policy(&prod).is_err());

        // Mock ARM client performs no real mutation → safe.
        prod.azure.auth_mode = "mock".to_string();
        assert!(enforce_remediation_safety_policy(&prod).is_ok());

        // Remediation disabled is always fine, even against real Azure.
        prod.remediation.enabled = false;
        prod.azure.auth_mode = "azure".to_string();
        assert!(enforce_remediation_safety_policy(&prod).is_ok());
    }
}
