use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tracing::{debug, instrument, warn};

use sre_common::errors::AzureError;

use crate::auth::TokenProvider;
use crate::models::KqlQueryResponse;

/// Shared KQL query executor for Log Analytics and Application Insights (ADR-005).
///
/// Both services use the same KQL language and similar REST APIs,
/// differing only in endpoint and auth scope.
pub struct AzureQueryClient {
    http: reqwest::Client,
    credential: Arc<dyn TokenProvider>,
    /// Max queries per minute (Azure throttles at ~200/min)
    max_queries_per_minute: u32,
}

impl AzureQueryClient {
    pub fn new(credential: Arc<dyn TokenProvider>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("Failed to create HTTP client"),
            credential,
            max_queries_per_minute: 30, // Conservative default, well below Azure's 200/min
        }
    }

    pub fn with_rate_limit(mut self, max_per_minute: u32) -> Self {
        self.max_queries_per_minute = max_per_minute;
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
            "https://api.loganalytics.io/v1/workspaces/{workspace_id}/query"
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

        let url = format!(
            "https://api.applicationinsights.io/v1/apps/{app_id}/query"
        );

        debug!(
            app_id,
            timespan,
            query_length = kql.len(),
            "Executing App Insights query"
        );

        self.execute_kql_query(&url, &token, kql, timespan).await
    }

    /// Internal: execute a KQL query against any endpoint
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
            warn!(url, status = status.as_u16(), elapsed_ms, "KQL query failed");
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
