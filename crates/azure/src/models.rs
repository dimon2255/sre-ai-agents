use serde::{Deserialize, Serialize};

// ─── Azure Common Alert Schema (webhook payload) ───

/// Top-level Azure Common Alert Schema
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureCommonAlert {
    pub schema_id: String,
    pub data: AlertData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertData {
    pub essentials: AlertEssentials,
    /// Signal-type-specific context (metric, log, activity log, etc.)
    pub alert_context: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertEssentials {
    pub alert_id: String,
    pub alert_rule: String,
    pub severity: String,
    pub signal_type: String,
    pub monitor_condition: String,
    #[serde(default)]
    pub monitoring_service: Option<String>,
    #[serde(default)]
    pub alert_target_i_ds: Vec<String>,
    #[serde(default)]
    pub originating_alert_id: Option<String>,
    pub fired_date_time: String,
    #[serde(default)]
    pub resolved_date_time: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub essentials_version: Option<String>,
    #[serde(default)]
    pub alert_context_version: Option<String>,
}

// ─── KQL Query Response ───

/// Response from Log Analytics / App Insights query API
#[derive(Debug, Clone, Deserialize)]
pub struct KqlQueryResponse {
    pub tables: Vec<KqlTable>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KqlTable {
    pub name: String,
    pub columns: Vec<KqlColumn>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KqlColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub column_type: String,
}

impl KqlTable {
    /// Convert rows to a vector of key-value maps for easier processing
    pub fn to_records(&self) -> Vec<serde_json::Map<String, serde_json::Value>> {
        self.rows
            .iter()
            .map(|row| {
                self.columns
                    .iter()
                    .zip(row.iter())
                    .map(|(col, val)| (col.name.clone(), val.clone()))
                    .collect()
            })
            .collect()
    }

    /// Get the number of rows
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

// ─── Azure Alerts Management API Response ───

/// Response from the Azure Alerts Management API
#[derive(Debug, Clone, Deserialize)]
pub struct AlertsListResponse {
    pub value: Vec<AlertResource>,
    #[serde(rename = "nextLink")]
    pub next_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertResource {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub resource_type: String,
    pub properties: AlertProperties,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertProperties {
    pub essentials: AlertPropertiesEssentials,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertPropertiesEssentials {
    pub severity: String,
    pub signal_type: String,
    pub alert_state: String,
    pub monitor_condition: String,
    #[serde(default)]
    pub target_resource: Option<String>,
    #[serde(default)]
    pub target_resource_name: Option<String>,
    #[serde(default)]
    pub target_resource_group: Option<String>,
    #[serde(default)]
    pub target_resource_type: Option<String>,
    #[serde(default)]
    pub start_date_time: Option<String>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// ─── Query result convenience types ───

/// Parsed error spike data from KQL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorSpikeRecord {
    pub timestamp: String,
    pub exception_type: String,
    pub problem_id: String,
    pub error_count: i64,
    pub distinct_errors: i64,
}

/// Parsed latency data from KQL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyRecord {
    pub timestamp: String,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub failure_rate_pct: f64,
    pub total_requests: i64,
}

/// Parsed dependency failure data from KQL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyFailureRecord {
    pub timestamp: String,
    pub dependency_type: String,
    pub target: String,
    pub result_code: String,
    pub duration_ms: f64,
    pub operation_id: String,
}

/// Filter criteria for Azure Alerts API
#[derive(Debug, Clone, Default)]
pub struct AlertFilter {
    pub severity: Option<String>,
    pub monitor_condition: Option<String>,
    pub time_range: Option<String>,
    pub target_resource_type: Option<String>,
}
