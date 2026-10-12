use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tracing::{debug, instrument, warn};

use sre_common::errors::AzureError;

use crate::auth::TokenProvider;
use crate::models::KqlQueryResponse;

/// Default Azure Log Analytics API base URL
const DEFAULT_LOG_ANALYTICS_BASE: &str = "https://api.loganalytics.io";

/// Default Azure Application Insights API base URL
const DEFAULT_APP_INSIGHTS_BASE: &str = "https://api.applicationinsights.io";

/// Shared KQL query executor for Log Analytics and Application Insights (ADR-005).
///
/// Both services use the same KQL language and similar REST APIs,
/// differing only in endpoint and auth scope.
///
/// Base URLs are configurable for testing (point at wiremock).
pub struct AzureQueryClient {
    http: reqwest::Client,
    credential: Arc<dyn TokenProvider>,
    log_analytics_base_url: String,
    app_insights_base_url: String,
}

impl AzureQueryClient {
    pub fn new(credential: Arc<dyn TokenProvider>) -> Self {
        Self {
            http: sre_common::http::client_builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("Failed to create HTTP client"),
            credential,
            log_analytics_base_url: DEFAULT_LOG_ANALYTICS_BASE.to_string(),
            app_insights_base_url: DEFAULT_APP_INSIGHTS_BASE.to_string(),
        }
    }

    /// Override the Log Analytics base URL (for testing with wiremock)
    pub fn with_log_analytics_base_url(mut self, url: impl Into<String>) -> Self {
        self.log_analytics_base_url = url.into();
        self
    }

    /// Override the App Insights base URL (for testing with wiremock)
    pub fn with_app_insights_base_url(mut self, url: impl Into<String>) -> Self {
        self.app_insights_base_url = url.into();
        self
    }

    /// Query a Log Analytics workspace using KQL
    #[instrument(skip(self, kql), fields(workspace_id, timespan))]
    pub async fn query_log_analytics(
        &self,
        workspace_id: &str,
        kql: &str,
        timespan: &str,
    ) -> Result<KqlQueryResponse, AzureError> {
        let token = self
            .credential
            .get_token("https://api.loganalytics.io/.default")
            .await?;

        let url = format!(
            "{}/v1/workspaces/{workspace_id}/query",
            self.log_analytics_base_url
        );

        debug!(
            workspace_id,
            timespan,
            query_length = kql.len(),
            "Executing Log Analytics query"
        );

        self.execute_kql_query(&url, &token, kql, timespan).await
    }

    /// Query an Application Insights resource using KQL
    #[instrument(skip(self, kql), fields(app_id, timespan))]
    pub async fn query_app_insights(
        &self,
        app_id: &str,
        kql: &str,
        timespan: &str,
    ) -> Result<KqlQueryResponse, AzureError> {
        let token = self
            .credential
            .get_token("https://api.applicationinsights.io/.default")
            .await?;

        let url = format!("{}/v1/apps/{app_id}/query", self.app_insights_base_url);

        debug!(
            app_id,
            timespan,
            query_length = kql.len(),
            "Executing App Insights query"
        );

        self.execute_kql_query(&url, &token, kql, timespan).await
    }

    /// Internal: execute a KQL query against any endpoint
    #[instrument(skip(self, token, kql), fields(url))]
    async fn execute_kql_query(
        &self,
        url: &str,
        token: &str,
        kql: &str,
        timespan: &str,
    ) -> Result<KqlQueryResponse, AzureError> {
        let start = Instant::now();

        let body = json!({
            "query": kql,
            "timespan": timespan
        });

        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| AzureError::Http(e.to_string()))?;

        let status = response.status();
        let elapsed_ms = start.elapsed().as_millis();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            warn!(url, elapsed_ms, "Azure API rate limited");
            return Err(AzureError::RateLimited);
        }

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            warn!(
                url,
                status = status.as_u16(),
                elapsed_ms,
                "KQL query failed"
            );
            return Err(AzureError::ApiError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        let result: KqlQueryResponse = response
            .json()
            .await
            .map_err(|e| AzureError::ParseError(e.to_string()))?;

        let row_count: usize = result.tables.iter().map(|t| t.row_count()).sum();
        debug!(url, elapsed_ms, row_count, "KQL query completed");

        Ok(result)
    }
}
