use serde::Deserialize;

/// Top-level platform configuration
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformConfig {
    pub azure: AzureConfig,
    pub orchestrator: OrchestratorConfig,
    pub agents: AgentsConfig,
    pub webhook: WebhookConfig,
    pub store: StoreConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AzureConfig {
    /// Azure subscription ID
    pub subscription_id: Option<String>,
    /// Log Analytics workspace IDs to query
    #[serde(default)]
    pub workspace_ids: Vec<String>,
    /// Application Insights app IDs
    #[serde(default)]
    pub app_insights_ids: Vec<String>,
    /// Resource group names to monitor
    #[serde(default)]
    pub resource_groups: Vec<String>,
    /// Auth mode: "azure" (DefaultAzureCredential) or "mock" (dev token)
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,
}

fn default_auth_mode() -> String {
    "azure".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    /// Max seconds to wait for any single agent response
    pub agent_timeout_secs: u64,
    /// Max concurrent investigations
    pub max_concurrent_investigations: u32,
    /// Max agents running in parallel per investigation
    pub max_parallel_agents_per_investigation: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentsConfig {
    pub triage: AgentConfig,
    pub logs: LogsAgentConfig,
    pub metrics: MetricsAgentConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogsAgentConfig {
    pub enabled: bool,
    /// Default KQL timespan in minutes
    pub default_timespan_minutes: u32,
    /// Max rows returned per query
    pub max_rows: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetricsAgentConfig {
    pub enabled: bool,
    /// Default KQL timespan in minutes
    pub default_timespan_minutes: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    pub listen_addr: String,
    pub listen_port: u16,
    /// The platform's own Azure resource ID. Alerts targeting this resource
    /// are acknowledged but not investigated, breaking the OTLP feedback loop.
    #[serde(default)]
    pub self_resource_id: Option<String>,
    /// Alert channel capacity (queued alerts before returning 503)
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
}

fn default_channel_capacity() -> usize {
    64
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    pub database_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_otlp_endpoint")]
    pub otlp_endpoint: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4318".to_string()
}

fn default_service_name() -> String {
    "sre-agents-platform".to_string()
}

impl ObservabilityConfig {
    /// Validate that the OTLP endpoint is a safe, internal destination.
    /// Prevents accidental exfiltration of span data (resource names, alert
    /// details, severity levels) to external endpoints.
    pub fn validate_endpoint(&self) -> anyhow::Result<()> {
        let url = url::Url::parse(&self.otlp_endpoint).map_err(|e| {
            anyhow::anyhow!("Invalid OTLP endpoint URL '{}': {e}", self.otlp_endpoint)
        })?;

        let host = url.host_str().unwrap_or("");
        let is_safe = host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host.ends_with(".internal")
            || host.ends_with(".local")
            || host.ends_with(".svc.cluster.local");

        if !is_safe {
            anyhow::bail!(
                "OTLP endpoint host '{host}' is not a recognised internal address. \
                 Allowed: localhost, 127.0.0.1, ::1, *.internal, *.local, *.svc.cluster.local"
            );
        }

        Ok(())
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: default_otlp_endpoint(),
            service_name: default_service_name(),
        }
    }
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            azure: AzureConfig {
                subscription_id: None,
                workspace_ids: vec![],
                app_insights_ids: vec![],
                resource_groups: vec![],
                auth_mode: "mock".to_string(),
            },
            orchestrator: OrchestratorConfig {
                agent_timeout_secs: 30,
                max_concurrent_investigations: 10,
                max_parallel_agents_per_investigation: 5,
            },
            agents: AgentsConfig {
                triage: AgentConfig { enabled: true },
                logs: LogsAgentConfig {
                    enabled: true,
                    default_timespan_minutes: 30,
                    max_rows: 1000,
                },
                metrics: MetricsAgentConfig {
                    enabled: true,
                    default_timespan_minutes: 30,
                },
            },
            webhook: WebhookConfig {
                listen_addr: "0.0.0.0".to_string(),
                listen_port: 8080,
                self_resource_id: None,
                channel_capacity: 64,
            },
            store: StoreConfig {
                database_url: "sqlite://data/sre-agents.db".to_string(),
            },
            observability: ObservabilityConfig::default(),
        }
    }
}
