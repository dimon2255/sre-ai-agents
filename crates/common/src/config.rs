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
    /// HMAC-SHA256 shared secret for webhook authentication (ADR-008).
    /// When `None`, the HMAC middleware is not mounted and the webhook runs
    /// unauthenticated (developer ergonomics for local `cargo run -- serve`).
    /// Staging and production MUST set this via `SRE_WEBHOOK_SECRET`.
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Maximum allowed clock skew between client timestamp and server time,
    /// in seconds. Bounds the replay window for captured signed requests.
    #[serde(default = "default_hmac_skew_seconds")]
    pub hmac_skew_seconds: u64,
}

fn default_channel_capacity() -> usize {
    64
}

fn default_hmac_skew_seconds() -> u64 {
    300
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
    #[serde(default = "default_environment")]
    pub environment: String,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4318".to_string()
}

fn default_service_name() -> String {
    "sre-agents-platform".to_string()
}

fn default_environment() -> String {
    "staging".to_string()
}

impl ObservabilityConfig {
    /// Validate that the OTLP endpoint is a safe, internal destination.
    /// Prevents accidental exfiltration of span data (resource names, alert
    /// details, severity levels) to external endpoints.
    pub fn validate_endpoint(&self) -> anyhow::Result<()> {
        let url = url::Url::parse(&self.otlp_endpoint).map_err(|e| {
            anyhow::anyhow!("Invalid OTLP endpoint URL '{}': {e}", self.otlp_endpoint)
        })?;

        let raw_host = url.host_str().unwrap_or("").to_lowercase();
        // Strip IPv6 brackets if present (host_str returns "[::1]" for IPv6)
        let host = raw_host.trim_start_matches('[').trim_end_matches(']');
        let is_safe = host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host.ends_with(".internal")
            || host.ends_with(".local")
            || host.ends_with(".svc.cluster.local")
            || is_rfc1918(host);

        if !is_safe {
            anyhow::bail!(
                "OTLP endpoint host '{host}' is not a recognised internal address. \
                 Allowed: localhost, 127.0.0.1, ::1, *.internal, *.local, \
                 *.svc.cluster.local, RFC 1918 private IPs"
            );
        }

        Ok(())
    }
}

/// Check if a host string is an RFC 1918 private IPv4 address.
/// Covers 10.0.0.0/8, 172.16.0.0/12, and 192.168.0.0/16.
fn is_rfc1918(host: &str) -> bool {
    let octets: Vec<u8> = match host
        .split('.')
        .map(|s| s.parse::<u8>())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) if v.len() == 4 => v,
        _ => return false,
    };
    matches!(
        (octets[0], octets[1]),
        (10, _) | (172, 16..=31) | (192, 168)
    )
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: default_otlp_endpoint(),
            service_name: default_service_name(),
            environment: default_environment(),
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
                hmac_secret: None,
                hmac_skew_seconds: 300,
            },
            store: StoreConfig {
                database_url: "postgres://postgres:password@localhost:5432/sre_agents".to_string(),
            },
            observability: ObservabilityConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_endpoint(endpoint: &str) -> ObservabilityConfig {
        ObservabilityConfig {
            enabled: true,
            otlp_endpoint: endpoint.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn validate_endpoint_allows_localhost() {
        assert!(config_with_endpoint("http://localhost:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_localhost_case_insensitive() {
        assert!(config_with_endpoint("http://LOCALHOST:4318")
            .validate_endpoint()
            .is_ok());
        assert!(config_with_endpoint("http://Localhost:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_loopback_ipv4() {
        assert!(config_with_endpoint("http://127.0.0.1:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_loopback_ipv6() {
        assert!(config_with_endpoint("http://[::1]:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_internal_suffix() {
        assert!(config_with_endpoint("http://collector.internal:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_local_suffix() {
        assert!(config_with_endpoint("http://otel.local:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_k8s_svc() {
        assert!(
            config_with_endpoint("http://otel-collector.monitoring.svc.cluster.local:4318")
                .validate_endpoint()
                .is_ok()
        );
    }

    #[test]
    fn validate_endpoint_allows_rfc1918_10() {
        assert!(config_with_endpoint("http://10.0.0.5:4318")
            .validate_endpoint()
            .is_ok());
        assert!(config_with_endpoint("http://10.255.255.255:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_allows_rfc1918_172() {
        assert!(config_with_endpoint("http://172.16.0.1:4318")
            .validate_endpoint()
            .is_ok());
        assert!(config_with_endpoint("http://172.31.255.255:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_rejects_172_outside_range() {
        assert!(config_with_endpoint("http://172.15.0.1:4318")
            .validate_endpoint()
            .is_err());
        assert!(config_with_endpoint("http://172.32.0.1:4318")
            .validate_endpoint()
            .is_err());
    }

    #[test]
    fn validate_endpoint_allows_rfc1918_192_168() {
        assert!(config_with_endpoint("http://192.168.1.1:4318")
            .validate_endpoint()
            .is_ok());
    }

    #[test]
    fn validate_endpoint_rejects_external_host() {
        assert!(config_with_endpoint("http://evil.example.com:4318")
            .validate_endpoint()
            .is_err());
    }

    #[test]
    fn validate_endpoint_rejects_public_ip() {
        assert!(config_with_endpoint("http://8.8.8.8:4318")
            .validate_endpoint()
            .is_err());
    }

    #[test]
    fn validate_endpoint_rejects_malformed_url() {
        assert!(config_with_endpoint("not-a-url")
            .validate_endpoint()
            .is_err());
    }

    #[test]
    fn is_rfc1918_rejects_non_ip() {
        assert!(!is_rfc1918("localhost"));
        assert!(!is_rfc1918("hello.world"));
    }
}
