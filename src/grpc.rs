//! gRPC API (Phase 2b) — an external integration surface for triggering and
//! querying investigations, built on tonic.
//!
//! The seam is the same as every other entry point: a trigger ultimately
//! produces an `AlertInfo` and runs through the orchestrator. `GetInvestigation`
//! reads the durable workflow store. Disabled by default; enabled via
//! `[grpc] enabled = true`.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use tracing::{error, info};

use sre_common::messages::{AlertInfo, AlertSeverity};
use sre_orchestrator::Orchestrator;
use sre_store::{PgStore, WorkflowStore};

pub mod pb {
    tonic::include_proto!("sre.v1");
}

use pb::investigation_service_server::{InvestigationService, InvestigationServiceServer};
use pb::{
    GetInvestigationRequest, GetInvestigationResponse, TriggerInvestigationRequest,
    TriggerInvestigationResponse,
};

/// gRPC service implementation. Holds the orchestrator (to run investigations)
/// and the store (to query past ones).
pub struct InvestigationGrpc {
    orchestrator: Arc<Orchestrator>,
    store: Arc<PgStore>,
}

impl InvestigationGrpc {
    pub fn new(orchestrator: Arc<Orchestrator>, store: Arc<PgStore>) -> Self {
        Self {
            orchestrator,
            store,
        }
    }

    /// Wrap into the tonic server service, ready for `Server::add_service`.
    pub fn into_server(self) -> InvestigationServiceServer<Self> {
        InvestigationServiceServer::new(self)
    }
}

/// Build an `AlertInfo` from a trigger request. Pure and unit-testable —
/// keeps the request-validation/mapping logic out of the async transport path.
fn alert_from_trigger(req: &TriggerInvestigationRequest) -> Result<AlertInfo, String> {
    if req.resource.trim().is_empty() {
        return Err("resource must not be empty".to_string());
    }
    let severity: AlertSeverity = if req.severity.is_empty() {
        AlertSeverity::Sev3
    } else {
        req.severity
            .parse()
            .map_err(|e| format!("invalid severity: {e}"))?
    };
    let signal_type = if req.signal_type.is_empty() {
        "Metric".to_string()
    } else {
        req.signal_type.clone()
    };
    let alert_rule = if req.alert_rule.is_empty() {
        "gRPC trigger".to_string()
    } else {
        req.alert_rule.clone()
    };
    Ok(AlertInfo {
        alert_id: format!("grpc-{}", uuid::Uuid::new_v4()),
        alert_rule,
        severity,
        signal_type,
        monitor_condition: "Fired".to_string(),
        affected_resource: req.resource.clone(),
        fired_at: chrono::Utc::now(),
        description: if req.description.is_empty() {
            None
        } else {
            Some(req.description.clone())
        },
        raw_context: None,
    })
}

#[tonic::async_trait]
impl InvestigationService for InvestigationGrpc {
    async fn trigger_investigation(
        &self,
        request: Request<TriggerInvestigationRequest>,
    ) -> Result<Response<TriggerInvestigationResponse>, Status> {
        let req = request.into_inner();
        let alert = alert_from_trigger(&req).map_err(Status::invalid_argument)?;
        let alert_id = alert.alert_id.clone();
        info!(
            alert_id = %alert_id,
            resource = %alert.affected_resource,
            "gRPC TriggerInvestigation"
        );

        // Synchronous run: the caller gets the full report back. NOTE: this
        // bypasses the dispatch-loop concurrency semaphore — acceptable for
        // the Phase 2b external surface; production gates this behind auth +
        // rate limits (and could enqueue onto the same channel instead).
        match self.orchestrator.investigate(alert).await {
            Ok(report) => {
                let state = if report.escalated {
                    "escalated"
                } else {
                    "resolved"
                };
                let message = format!(
                    "investigation complete with {} findings",
                    report.findings.len()
                );
                let report_json = serde_json::to_string(&report).unwrap_or_default();
                Ok(Response::new(TriggerInvestigationResponse {
                    accepted: true,
                    investigation_id: report.investigation_id,
                    state: state.to_string(),
                    report_json,
                    message,
                }))
            }
            Err(e) => {
                error!(alert_id = %alert_id, error = %e, "gRPC investigation failed");
                Err(Status::internal(format!("investigation failed: {e}")))
            }
        }
    }

    async fn get_investigation(
        &self,
        request: Request<GetInvestigationRequest>,
    ) -> Result<Response<GetInvestigationResponse>, Status> {
        let req = request.into_inner();
        let id = uuid::Uuid::parse_str(&req.investigation_id)
            .map_err(|_| Status::invalid_argument("investigation_id must be a UUID"))?;

        match self.store.get_workflow(&id).await {
            Ok(Some(record)) => Ok(Response::new(GetInvestigationResponse {
                found: true,
                investigation_id: record.id.to_string(),
                state: record.state.to_string(),
                report_json: record.report_json.unwrap_or_default(),
            })),
            Ok(None) => Ok(Response::new(GetInvestigationResponse {
                found: false,
                investigation_id: req.investigation_id,
                state: String::new(),
                report_json: String::new(),
            })),
            Err(e) => Err(Status::internal(format!("store error: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_from_trigger_applies_defaults() {
        let req = TriggerInvestigationRequest {
            resource: "payments-api".to_string(),
            severity: String::new(),
            signal_type: String::new(),
            description: String::new(),
            alert_rule: String::new(),
        };
        let alert = alert_from_trigger(&req).unwrap();
        assert_eq!(alert.affected_resource, "payments-api");
        assert_eq!(alert.severity, AlertSeverity::Sev3);
        assert_eq!(alert.signal_type, "Metric");
        assert_eq!(alert.alert_rule, "gRPC trigger");
        assert!(alert.alert_id.starts_with("grpc-"));
    }

    #[test]
    fn alert_from_trigger_parses_severity_and_fields() {
        let req = TriggerInvestigationRequest {
            resource: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Web/sites/web"
                .to_string(),
            severity: "Sev1".to_string(),
            signal_type: "Log".to_string(),
            description: "high error rate".to_string(),
            alert_rule: "Error Rate".to_string(),
        };
        let alert = alert_from_trigger(&req).unwrap();
        assert_eq!(alert.severity, AlertSeverity::Sev1);
        assert_eq!(alert.signal_type, "Log");
        assert_eq!(alert.description.as_deref(), Some("high error rate"));
        assert_eq!(alert.alert_rule, "Error Rate");
    }

    #[test]
    fn alert_from_trigger_rejects_empty_resource() {
        let req = TriggerInvestigationRequest {
            resource: "   ".to_string(),
            ..Default::default()
        };
        assert!(alert_from_trigger(&req).is_err());
    }

    #[test]
    fn alert_from_trigger_rejects_bad_severity() {
        let req = TriggerInvestigationRequest {
            resource: "x".to_string(),
            severity: "SevNope".to_string(),
            ..Default::default()
        };
        assert!(alert_from_trigger(&req).is_err());
    }
}
