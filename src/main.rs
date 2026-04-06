use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

use sre_agents::{AgentHandle, LogsAgent, MetricsAgent, SreAgent, TriageAgent};
use sre_azure::auth::AzureCredentialProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::PlatformConfig;
use sre_common::messages::*;
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::SqliteStore;

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
    // Initialize tracing
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sre_orchestrator=debug,sre_agents=debug")),
        )
        .with_target(true)
        .with_thread_ids(false)
        .init();

    let cli = Cli::parse();

    // Load configuration
    let config = load_config(&cli.config)?;

    match cli.command {
        Commands::Serve => run_server(config).await,
        Commands::Investigate {
            resource,
            severity,
            signal_type,
        } => run_investigation(config, resource, severity, signal_type).await,
    }
}

/// Load platform configuration from TOML file with env var overrides
fn load_config(path: &str) -> anyhow::Result<PlatformConfig> {
    let builder = config::Config::builder()
        .add_source(config::File::with_name(path).required(false))
        .add_source(config::Environment::with_prefix("SRE").separator("__"));

    let settings = builder.build()?;
    let config: PlatformConfig = settings.try_deserialize().unwrap_or_default();

    info!("Configuration loaded from {path}");
    Ok(config)
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

/// Bootstrap all agents and return the orchestrator + agent join handles
async fn bootstrap(
    config: &PlatformConfig,
) -> anyhow::Result<(Orchestrator, Vec<tokio::task::JoinHandle<()>>)> {
    // Initialize store
    let store = Arc::new(SqliteStore::new(&config.store.database_url).await?);
    let event_log = store.clone();

    // Initialize Azure credential based on config auth_mode
    let credential: Arc<dyn sre_azure::TokenProvider> =
        match config.azure.auth_mode.as_str() {
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
            MetricsAgent::new(
                query_client.clone(),
                config.azure.app_insights_ids.clone(),
            ),
            &mut router,
            &mut join_handles,
        );
    }

    let orchestrator = Orchestrator::new(config.clone(), router, store, event_log);

    Ok((orchestrator, join_handles))
}

/// Run the webhook server
async fn run_server(config: PlatformConfig) -> anyhow::Result<()> {
    let listen_addr = format!("{}:{}", config.webhook.listen_addr, config.webhook.listen_port);

    let (_orchestrator, _agent_handles) = bootstrap(&config).await?;

    info!(listen_addr = %listen_addr, "Starting webhook server");

    // TODO: Wire up axum server with webhook::handle_azure_alert
    // For now, just keep the process alive
    info!("SRE Agent Platform running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;

    info!("Shutting down...");
    Ok(())
}

/// Run a manual investigation from the CLI
async fn run_investigation(
    config: PlatformConfig,
    resource: String,
    severity: String,
    signal_type: String,
) -> anyhow::Result<()> {
    let (mut orchestrator, _agent_handles) = bootstrap(&config).await?;

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
