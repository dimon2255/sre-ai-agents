//! ARM write client (Phase 2c, ADR-009) — the only component that issues
//! *mutations* against Azure Resource Manager. Everything upstream (agent,
//! gate) is read-only or advisory; this is where a remediation actually
//! touches Azure.
//!
//! Two implementations behind the `ArmWriteClient` trait, mirroring the
//! `TokenProvider` pattern: `MockArmClient` (deterministic, records calls, used
//! in dev/CI and tests) and `AzureArmClient` (real ARM REST via a bearer
//! token). ADR-011 will move the real client onto a *separate* managed identity
//! from the read path.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tracing::{instrument, warn};

use sre_common::errors::AzureError;
use sre_common::remediation::{ArmWriteRequest, ResourceId};

use crate::auth::TokenProvider;

/// Result of an ARM write.
#[derive(Debug, Clone)]
pub struct ArmWriteResponse {
    pub status: u16,
    /// Resources the mutation actually affected. The executor checks this
    /// against the proposal's declared `scope_resources` (post-mortem
    /// blast-radius check, ADR-009 follow-ups).
    pub affected_resources: Vec<ResourceId>,
    pub body: serde_json::Value,
}

/// Issues mutations against Azure Resource Manager.
#[async_trait]
pub trait ArmWriteClient: Send + Sync {
    async fn execute(&self, req: &ArmWriteRequest) -> Result<ArmWriteResponse, AzureError>;
}

const ARM_BASE: &str = "https://management.azure.com";
const ARM_SCOPE: &str = "https://management.azure.com/.default";

/// Real ARM REST client. Issues `req.method` to `{base}{resource_id}?api-version=...`
/// with a bearer token for the management scope.
pub struct AzureArmClient {
    http: reqwest::Client,
    credential: Arc<dyn TokenProvider>,
    base_url: String,
}

impl AzureArmClient {
    pub fn new(credential: Arc<dyn TokenProvider>) -> Self {
        Self {
            http: sre_common::http::client_builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build HTTP client"),
            credential,
            base_url: ARM_BASE.to_string(),
        }
    }

    /// Override the ARM base URL (for testing against a local mock server).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl ArmWriteClient for AzureArmClient {
    #[instrument(skip(self, req), fields(method = %req.method, resource = %req.resource_id))]
    async fn execute(&self, req: &ArmWriteRequest) -> Result<ArmWriteResponse, AzureError> {
        let token = self.credential.get_token(ARM_SCOPE).await?;
        let url = format!(
            "{}{}?api-version={}",
            self.base_url, req.resource_id, req.api_version
        );
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| AzureError::Http(format!("invalid HTTP method {}: {e}", req.method)))?;

        let mut builder = self.http.request(method, &url).bearer_auth(token);
        let body_is_empty =
            req.body.is_null() || req.body.as_object().map(|o| o.is_empty()).unwrap_or(false);
        if !body_is_empty {
            builder = builder.json(&req.body);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| AzureError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or_else(|_| json!({}));

        if !(200..300).contains(&status) {
            warn!(status, "ARM write returned non-success status");
            return Err(AzureError::ApiError {
                status,
                message: body.to_string(),
            });
        }

        // ARM does not return an explicit affected-resources list for most
        // actions; the target resource is the affected resource.
        Ok(ArmWriteResponse {
            status,
            affected_resources: vec![ResourceId(req.resource_id.clone())],
            body,
        })
    }
}

/// Mock ARM client: records every request and returns a configurable response.
/// `MockArmMode` drives the post-mortem-check and failure-path tests.
#[derive(Clone)]
pub struct MockArmClient {
    calls: Arc<Mutex<Vec<ArmWriteRequest>>>,
    mode: MockArmMode,
}

#[derive(Clone)]
pub enum MockArmMode {
    /// 200 OK, affected = the target resource only (in-scope).
    Success,
    /// 200 OK, but affected also includes these extra resources (over-scope —
    /// trips the executor's blast-radius check).
    OverScope(Vec<ResourceId>),
    /// Returns an ARM error with this message.
    Failure(String),
}

impl Default for MockArmClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockArmClient {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            mode: MockArmMode::Success,
        }
    }

    pub fn with_mode(mode: MockArmMode) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            mode,
        }
    }

    /// The requests this client received, in order.
    pub fn calls(&self) -> Vec<ArmWriteRequest> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl ArmWriteClient for MockArmClient {
    async fn execute(&self, req: &ArmWriteRequest) -> Result<ArmWriteResponse, AzureError> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.clone());
        match &self.mode {
            MockArmMode::Success => Ok(ArmWriteResponse {
                status: 200,
                affected_resources: vec![ResourceId(req.resource_id.clone())],
                body: json!({ "ok": true }),
            }),
            MockArmMode::OverScope(extra) => {
                let mut affected = vec![ResourceId(req.resource_id.clone())];
                affected.extend(extra.clone());
                Ok(ArmWriteResponse {
                    status: 200,
                    affected_resources: affected,
                    body: json!({ "ok": true }),
                })
            }
            MockArmMode::Failure(msg) => Err(AzureError::ApiError {
                status: 500,
                message: msg.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ArmWriteRequest {
        ArmWriteRequest {
            method: "POST".to_string(),
            resource_id: "/subscriptions/s/.../containerApps/web/restart".to_string(),
            api_version: "2024-03-01".to_string(),
            body: json!({}),
        }
    }

    #[tokio::test]
    async fn mock_success_records_call_and_returns_target_as_affected() {
        let client = MockArmClient::new();
        let resp = client.execute(&req()).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.affected_resources.len(), 1);
        assert_eq!(client.calls().len(), 1);
    }

    #[tokio::test]
    async fn mock_over_scope_includes_extra_resources() {
        let client = MockArmClient::with_mode(MockArmMode::OverScope(vec![ResourceId(
            "/subscriptions/s/.../containerApps/other".to_string(),
        )]));
        let resp = client.execute(&req()).await.unwrap();
        assert_eq!(resp.affected_resources.len(), 2);
    }

    #[tokio::test]
    async fn mock_failure_returns_error() {
        let client = MockArmClient::with_mode(MockArmMode::Failure("boom".to_string()));
        assert!(client.execute(&req()).await.is_err());
    }
}
