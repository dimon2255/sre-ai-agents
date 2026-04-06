use serde::Deserialize;

/// Top-level platform configuration
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformConfig {
    pub azure: AzureConfig,
    pub orchestrator: OrchestratorConfig,
    pub agents: AgentsConfig,
    pub webhook: WebhookConfig,
    pub store: StoreConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AzureConfig {
    /// Azure subscription ID
    pub subscription_id: Option<String>,
    /// Log Analytics workspace IDs to query
    pub workspace_ids: Vec<String>,
    /// Application Insights app IDs
    pub app_insights_ids: Vec<String>,
    /// Resource group names to monitor
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    pub database_url: String,
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
            },
            store: StoreConfig {
                database_url: "sqlite://data/sre-agents.db".to_string(),
            },
        }
    }
}
