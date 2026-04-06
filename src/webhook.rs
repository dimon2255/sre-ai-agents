use axum::{http::StatusCode, Json};
use tracing::{info, warn};

use sre_azure::alerts_client::AzureAlertsClient;
use sre_azure::models::AzureCommonAlert;

/// Webhook handler for Azure Alert action group callbacks.
///
/// Azure sends a POST with the Common Alert Schema when an alert fires.
/// We parse it, convert to our AlertInfo type, and queue an investigation.
///
/// Expected webhook URL: POST /api/v1/alerts/azure
pub async fn handle_azure_alert(
    Json(payload): Json<AzureCommonAlert>,
) -> Result<StatusCode, StatusCode> {
    info!(
        alert_rule = %payload.data.essentials.alert_rule,
        severity = %payload.data.essentials.severity,
        signal_type = %payload.data.essentials.signal_type,
        "Received Azure alert webhook"
    );

    // Parse the Common Alert Schema into our AlertInfo type
    let alert_info = match AzureAlertsClient::parse_common_alert(&payload) {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "Failed to parse alert payload");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    info!(
        alert_id = %alert_info.alert_id,
        resource = %alert_info.affected_resource,
        severity = %alert_info.severity,
        "Alert parsed — queuing investigation"
    );

    // TODO: Send alert_info to orchestrator via channel
    // For now, acknowledge receipt
    Ok(StatusCode::ACCEPTED)
}

/// Health check endpoint
pub async fn health() -> StatusCode {
    StatusCode::OK
}
