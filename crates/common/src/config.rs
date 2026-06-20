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
    #[serde(default)]
    pub nats: NatsConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub grpc: GrpcConfig,
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
    #[serde(default = "default_trace_agent_config")]
    pub trace: TraceAgentConfig,
    #[serde(default = "default_infra_agent_config")]
    pub infra: InfraAgentConfig,
}

fn default_trace_agent_config() -> TraceAgentConfig {
    TraceAgentConfig {
        enabled: true,
        default_timespan_minutes: 30,
        max_rows: 50,
    }
}

fn default_infra_agent_config() -> InfraAgentConfig {
    InfraAgentConfig {
        enabled: true,
        default_timespan_minutes: 120,
        max_rows: 50,
    }
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
pub struct TraceAgentConfig {
    pub enabled: bool,
    /// Default KQL timespan in minutes
    pub default_timespan_minutes: u32,
    /// Max rows returned per trace-correlation query
    pub max_rows: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InfraAgentConfig {
    pub enabled: bool,
    /// Default KQL timespan in minutes (deployments often precede incidents,
    /// so this defaults wider than the log/metric agents)
    pub default_timespan_minutes: u32,
    /// Max rows returned per Activity Log query
    pub max_rows: u32,
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

/// NATS JetStream ingestion configuration (ADR-013).
///
/// When `enabled` is `false` (the default), the NATS ingestor task is not
/// spawned and the platform behaves exactly as it did before ADR-013.
/// The webhook continues to receive alerts unchanged.
#[derive(Debug, Clone, Deserialize)]
pub struct NatsConfig {
    /// When `false`, the NATS ingestor task is not spawned. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// NATS server URL. Example: `nats://nats:4222`.
    #[serde(default = "default_nats_url")]
    pub url: String,
    /// Optional path to a NATS credentials file (NKEY/JWT). When `None`, the
    /// connection is anonymous — acceptable for local dev only. Staging and
    /// production MUST set this via `SRE__NATS__CREDENTIALS_PATH`.
    #[serde(default)]
    pub credentials_path: Option<String>,
    /// JetStream stream name. Created if absent.
    #[serde(default = "default_nats_stream_name")]
    pub stream_name: String,
    /// Durable consumer name. Created if absent.
    #[serde(default = "default_nats_consumer_name")]
    pub consumer_name: String,
    /// Subject filter for the consumer. Example: `alerts.azure.>`.
    #[serde(default = "default_nats_subject_filter")]
    pub subject_filter: String,
    /// Maximum number of unacknowledged messages in-flight to this consumer.
    /// Should match or be close to the `webhook.channel_capacity` so JetStream
    /// stops delivering before the in-process channel saturates.
    #[serde(default = "default_nats_max_ack_pending")]
    pub max_ack_pending: i64,
    /// How long JetStream waits for an ack before redelivering.
    #[serde(default = "default_nats_ack_wait_secs")]
    pub ack_wait_secs: u64,
    /// Maximum redelivery attempts. After this, the message is terminal
    /// (future: routed to DLQ — see ADR-013 out-of-scope section).
    #[serde(default = "default_nats_max_deliver")]
    pub max_deliver: i64,
    /// Maximum age of messages in the stream, in hours. Default: 24.
    #[serde(default = "default_nats_max_age_hours")]
    pub max_age_hours: u64,
    /// Number of stream replicas. 1 for staging, 3 for prod.
    #[serde(default = "default_nats_num_replicas")]
    pub num_replicas: usize,
    /// Enable the dead-letter stream (ADR-013 PR-3). When `true`, poison
    /// messages (parse failures) are republished to `dlq_subject` for
    /// forensic inspection before being terminated, instead of dropped.
    #[serde(default = "default_nats_dlq_enabled")]
    pub dlq_enabled: bool,
    /// JetStream stream name for the DLQ. Created if absent.
    #[serde(default = "default_nats_dlq_stream_name")]
    pub dlq_stream_name: String,
    /// Subject filter the DLQ stream binds (wildcard).
    #[serde(default = "default_nats_dlq_subject_filter")]
    pub dlq_subject_filter: String,
    /// Concrete subject poison messages are published to.
    #[serde(default = "default_nats_dlq_subject")]
    pub dlq_subject: String,
    /// Max age of DLQ messages in hours. Defaults wider than the main
    /// stream (7 days) for forensic retention.
    #[serde(default = "default_nats_dlq_max_age_hours")]
    pub dlq_max_age_hours: u64,
}

fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}

fn default_nats_stream_name() -> String {
    "sre-alerts".to_string()
}

fn default_nats_consumer_name() -> String {
    "sre-orchestrator".to_string()
}

fn default_nats_subject_filter() -> String {
    "alerts.azure.>".to_string()
}

fn default_nats_max_ack_pending() -> i64 {
    64
}

fn default_nats_ack_wait_secs() -> u64 {
    60
}

fn default_nats_max_deliver() -> i64 {
    5
}

fn default_nats_max_age_hours() -> u64 {
    24
}

fn default_nats_num_replicas() -> usize {
    1
}

fn default_nats_dlq_enabled() -> bool {
    true
}

fn default_nats_dlq_stream_name() -> String {
    "sre-alerts-dlq".to_string()
}

fn default_nats_dlq_subject_filter() -> String {
    "alerts.dlq.azure.>".to_string()
}

fn default_nats_dlq_subject() -> String {
    "alerts.dlq.azure.poison".to_string()
}

fn default_nats_dlq_max_age_hours() -> u64 {
    168
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_nats_url(),
            credentials_path: None,
            stream_name: default_nats_stream_name(),
            consumer_name: default_nats_consumer_name(),
            subject_filter: default_nats_subject_filter(),
            max_ack_pending: default_nats_max_ack_pending(),
            ack_wait_secs: default_nats_ack_wait_secs(),
            max_deliver: default_nats_max_deliver(),
            max_age_hours: default_nats_max_age_hours(),
            num_replicas: default_nats_num_replicas(),
            dlq_enabled: default_nats_dlq_enabled(),
            dlq_stream_name: default_nats_dlq_stream_name(),
            dlq_subject_filter: default_nats_dlq_subject_filter(),
            dlq_subject: default_nats_dlq_subject(),
            dlq_max_age_hours: default_nats_dlq_max_age_hours(),
        }
    }
}

/// Notification engine configuration. Delivers investigation reports and
/// escalations to chat/paging destinations. Each provider is enabled only
/// when its secret/URL is present; when `enabled` is true but no provider is
/// configured, a logging mock notifier is used (dev affordance).
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    /// Master switch. When `false`, no notification engine is built.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum report severity to notify on: `critical|high|medium|low|info`.
    #[serde(default = "default_notify_min_severity")]
    pub min_severity: String,
    /// Always notify when an investigation escalates, regardless of severity.
    #[serde(default = "default_true")]
    pub notify_on_escalation: bool,
    /// Slack incoming-webhook destination (secret via env).
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    /// Microsoft Teams incoming-webhook destination (secret via env).
    #[serde(default)]
    pub teams: Option<TeamsConfig>,
    /// PagerDuty Events API v2 destination (routing key via env).
    #[serde(default)]
    pub pagerduty: Option<PagerDutyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlackConfig {
    pub webhook_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamsConfig {
    pub webhook_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PagerDutyConfig {
    pub routing_key: String,
}

fn default_notify_min_severity() -> String {
    "high".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_severity: default_notify_min_severity(),
            notify_on_escalation: true,
            slack: None,
            teams: None,
            pagerduty: None,
        }
    }
}

/// gRPC API server configuration (Phase 2b). Disabled by default; runs on a
/// separate port from the webhook when enabled.
#[derive(Debug, Clone, Deserialize)]
pub struct GrpcConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_grpc_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_grpc_listen_port")]
    pub listen_port: u16,
}

fn default_grpc_listen_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_grpc_listen_port() -> u16 {
    50051
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_grpc_listen_addr(),
            listen_port: default_grpc_listen_port(),
        }
    }
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
    // Empty by default: let the OTel SDK read `OTEL_EXPORTER_OTLP_ENDPOINT`
    // from the environment.
    //
    // - In ACA staging/prod (acaenv-sre-agents-staging), the env-managed OTel
    //   agent auto-injects the env var pointing at its cluster-local
    //   service (e.g., `http://k8se-otel.k8se-apps.svc.cluster.local:4317`).
    //   Hardcoding `http://localhost:4317` here would override that and
    //   silently break export (nothing listens on localhost in an ACA pod).
    // - For local dev with no env var set, the OTel SDK falls back to its
    //   own default (`http://localhost:4317`), so a developer running a
    //   local collector on the standard port still gets traces.
    String::new()
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
    ///
    /// An empty endpoint is allowed — it signals "let the OTel SDK read
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` from the environment" (auto-injected
    /// by the ACA env-managed agent in staging/prod).
    pub fn validate_endpoint(&self) -> anyhow::Result<()> {
        if self.otlp_endpoint.is_empty() {
            return Ok(());
        }
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
                trace: default_trace_agent_config(),
                infra: default_infra_agent_config(),
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
            nats: NatsConfig::default(),
            notifications: NotificationConfig::default(),
            grpc: GrpcConfig::default(),
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
