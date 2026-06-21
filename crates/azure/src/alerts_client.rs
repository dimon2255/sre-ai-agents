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
        other => Err(AzureError::ParseError(format!("Unknown severity: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::MockTokenProvider;

    fn alert_json(severity: &str, fired: &str) -> serde_json::Value {
        serde_json::json!({
            "schemaId": "azureMonitorCommonAlertSchema",
            "data": {
                "essentials": {
                    "alertId": "/subscriptions/abc/providers/Microsoft.AlertsManagement/alerts/test-1",
                    "alertRule": "High Error Rate",
                    "severity": severity,
                    "signalType": "Metric",
                    "monitorCondition": "Fired",
                    "alertTargetIDs": ["/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/payments-api"],
                    "firedDateTime": fired,
                    "description": "Error rate exceeded 5%"
                },
                "alertContext": { "conditionType": "metric" }
            }
        })
    }

    #[test]
    fn new_constructs_client() {
        let cred = Arc::new(MockTokenProvider::new("token"));
        let _client = AzureAlertsClient::new(cred, "sub-123".to_string());
    }

    #[test]
    fn parse_common_alert_extracts_fields() {
        let payload: AzureCommonAlert =
            serde_json::from_value(alert_json("Sev1", "2026-06-20T10:00:00Z")).unwrap();
        let info = AzureAlertsClient::parse_common_alert(&payload).unwrap();
        assert_eq!(info.severity, AlertSeverity::Sev1);
        assert_eq!(info.alert_rule, "High Error Rate");
        assert_eq!(info.signal_type, "Metric");
        // affected_resource comes from the first alert target id
        assert!(info.affected_resource.contains("payments-api"));
        assert!(info.raw_context.is_some());
    }

    #[test]
    fn parse_common_alert_falls_back_to_alert_id_without_targets() {
        let mut v = alert_json("Sev2", "2026-06-20T10:00:00Z");
        v["data"]["essentials"]["alertTargetIDs"] = serde_json::json!([]);
        let payload: AzureCommonAlert = serde_json::from_value(v).unwrap();
        let info = AzureAlertsClient::parse_common_alert(&payload).unwrap();
        // No targets => affected_resource falls back to the alert id
        assert!(info.affected_resource.contains("alerts/test-1"));
    }

    #[test]
    fn parse_common_alert_rejects_bad_severity() {
        let payload: AzureCommonAlert =
            serde_json::from_value(alert_json("SevX", "2026-06-20T10:00:00Z")).unwrap();
        assert!(AzureAlertsClient::parse_common_alert(&payload).is_err());
    }

    #[test]
    fn parse_common_alert_rejects_bad_timestamp() {
        let payload: AzureCommonAlert =
            serde_json::from_value(alert_json("Sev1", "not-a-timestamp")).unwrap();
        assert!(AzureAlertsClient::parse_common_alert(&payload).is_err());
    }

    #[test]
    fn parse_severity_covers_all_levels() {
        assert_eq!(parse_severity("Sev0").unwrap(), AlertSeverity::Sev0);
        assert_eq!(parse_severity("Sev1").unwrap(), AlertSeverity::Sev1);
        assert_eq!(parse_severity("Sev2").unwrap(), AlertSeverity::Sev2);
        assert_eq!(parse_severity("Sev3").unwrap(), AlertSeverity::Sev3);
        assert_eq!(parse_severity("Sev4").unwrap(), AlertSeverity::Sev4);
        assert!(parse_severity("Sev9").is_err());
    }
}
