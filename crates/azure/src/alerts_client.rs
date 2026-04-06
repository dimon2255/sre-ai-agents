use std::sync::Arc;

use tracing::{debug, instrument, warn};

use sre_common::errors::AzureError;
use sre_common::messages::{AlertInfo, AlertSeverity};

use crate::auth::TokenProvider;
use crate::models::{AlertFilter, AlertResource, AlertsListResponse, AzureCommonAlert};

/// Client for the Azure Monitor Alerts Management API
pub struct AzureAlertsClient {
    http: reqwest::Client,
    credential: Arc<dyn TokenProvider>,
    subscription_id: String,
}

impl AzureAlertsClient {
    pub fn new(credential: Arc<dyn TokenProvider>, subscription_id: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client"),
            credential,
            subscription_id,
        }
    }

    /// Get fired alerts from Azure Monitor
    #[instrument(skip(self))]
    pub async fn get_fired_alerts(
        &self,
        filter: &AlertFilter,
    ) -> Result<Vec<AlertResource>, AzureError> {
        let token = self
            .credential
            .get_token("https://management.azure.com/.default")
            .await?;

        let url = format!(
            "https://management.azure.com/subscriptions/{}/providers/\
             Microsoft.AlertsManagement/alerts",
            self.subscription_id
        );

        let mut query_params = vec![("api-version", "2023-07-12-preview".to_string())];

        if let Some(ref severity) = filter.severity {
            query_params.push(("severity", severity.clone()));
        }
        if let Some(ref condition) = filter.monitor_condition {
            query_params.push(("monitorCondition", condition.clone()));
        }
        if let Some(ref time_range) = filter.time_range {
            query_params.push(("timeRange", time_range.clone()));
        }
        if let Some(ref resource_type) = filter.target_resource_type {
            query_params.push(("targetResourceType", resource_type.clone()));
        }

        let response = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&query_params)
            .send()
            .await
            .map_err(|e| AzureError::Http(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            warn!(status = status.as_u16(), "Failed to fetch alerts");
            return Err(AzureError::ApiError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        let body: AlertsListResponse = response
            .json()
            .await
            .map_err(|e| AzureError::ParseError(e.to_string()))?;

        debug!(count = body.value.len(), "Fetched fired alerts");
        Ok(body.value)
    }

    /// Parse an incoming Azure Common Alert Schema webhook payload into our AlertInfo type
    pub fn parse_common_alert(payload: &AzureCommonAlert) -> Result<AlertInfo, AzureError> {
        let essentials = &payload.data.essentials;

        let severity = parse_severity(&essentials.severity)?;
        let fired_at = chrono::DateTime::parse_from_rfc3339(&essentials.fired_date_time)
            .map_err(|e| AzureError::ParseError(format!("Invalid fired_date_time: {e}")))?
            .with_timezone(&chrono::Utc);

        // Extract affected resource from alert target IDs or alert_id
        let affected_resource = essentials
            .alert_target_i_ds
            .first()
            .cloned()
            .unwrap_or_else(|| essentials.alert_id.clone());

        Ok(AlertInfo {
            alert_id: essentials.alert_id.clone(),
            alert_rule: essentials.alert_rule.clone(),
            severity,
            signal_type: essentials.signal_type.clone(),
            monitor_condition: essentials.monitor_condition.clone(),
            affected_resource,
            fired_at,
            description: essentials.description.clone(),
            raw_context: Some(payload.data.alert_context.clone()),
        })
    }
}

/// Parse Azure severity string (e.g., "Sev0", "Sev1") into our enum
fn parse_severity(s: &str) -> Result<AlertSeverity, AzureError> {
    match s {
        "Sev0" => Ok(AlertSeverity::Sev0),
        "Sev1" => Ok(AlertSeverity::Sev1),
        "Sev2" => Ok(AlertSeverity::Sev2),
        "Sev3" => Ok(AlertSeverity::Sev3),
        "Sev4" => Ok(AlertSeverity::Sev4),
        other => Err(AzureError::ParseError(format!(
            "Unknown severity: {other}"
        ))),
    }
}
