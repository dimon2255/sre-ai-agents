//! A complete, runnable custom agent built on the SRE Agent SDK.
//!
//! Run with: `cargo run -p sre-agent-sdk --example custom_agent`
//!
//! It wires the same channels the orchestrator would, dispatches one analysis
//! task to a custom `DiskSpaceAgent`, and prints the finding the agent replies
//! with — the full request/response shape in ~100 lines.

use sre_agent_sdk::prelude::*;
use uuid::Uuid;

/// A toy custom agent that flags high disk usage. A real agent would query a
/// data source (Prometheus, a cloud API, an SSH probe); here we synthesize a
/// finding so the example stays self-contained.
struct DiskSpaceAgent {
    id: AgentId,
}

impl DiskSpaceAgent {
    fn new() -> Self {
        Self {
            id: AgentId("disk-space-agent".to_string()),
        }
    }
}

#[async_trait]
impl SreAgent for DiskSpaceAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Custom("disk-space".to_string())
    }

    fn capabilities(&self) -> Vec<String> {
        vec!["disk.utilization".to_string()]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        while let Some(inbound) = inbox.recv().await {
            let msg = &inbound.message;
            match &msg.payload {
                AgentPayload::AnalysisTask(task) => {
                    // ...real analysis would go here, keyed on task.params...
                    let finding = Finding {
                        category: FindingCategory::ResourceExhaustion,
                        severity: FindingSeverity::High,
                        title: "Disk usage at 92%".to_string(),
                        description: format!(
                            "{} data volume is nearly full",
                            task.params.resource_name
                        ),
                        confidence: 0.9,
                        observed_at: None,
                        source_agent: self.id.0.clone(),
                        evidence: vec![],
                        suggestions: vec!["Expand the volume or prune old data".to_string()],
                    };
                    let result = AnalysisResult {
                        findings: vec![finding],
                        summary: format!("Analyzed disk for {}", task.params.resource_name),
                        queries_executed: vec![],
                    };
                    let response = AgentMessage::new(
                        msg.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::AnalysisResult(result),
                        0.9,
                    )
                    .with_parent(msg.id);
                    if inbound.reply_tx.send(response).await.is_err() {
                        break; // orchestrator hung up
                    }
                }
                AgentPayload::HealthCheck => {
                    let response = AgentMessage::new(
                        msg.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::HealthStatus(AgentHealth {
                            agent_id: self.id.clone(),
                            status: AgentStatus::Ready,
                            active_tasks: 0,
                            uptime_secs: 0,
                        }),
                        1.0,
                    );
                    let _ = inbound.reply_tx.send(response).await;
                }
                _ => {} // ignore payloads this agent doesn't handle
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let agent = DiskSpaceAgent::new();
    println!("Starting {} ({})", agent.id().0, agent.agent_type());

    // Inbox: orchestrator → agent. Reply: agent → orchestrator (per investigation).
    let (inbox_tx, inbox_rx) = mpsc::channel::<InboundMessage>(8);
    let (reply_tx, mut reply_rx) = mpsc::channel::<AgentMessage>(8);

    let handle = tokio::spawn(async move { agent.run(inbox_rx).await });

    // Dispatch one analysis task, just as the orchestrator would.
    let task = AgentMessage::new(
        Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("disk-space-agent".to_string())),
        AgentPayload::AnalysisTask(AnalysisTask {
            task_type: AnalysisTaskType::ResourceUtilization,
            params: InvestigationParams {
                resource_name: "payments-db".to_string(),
                resource_group: None,
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }),
        1.0,
    );
    inbox_tx
        .send(InboundMessage {
            message: task,
            reply_tx,
        })
        .await
        .expect("agent inbox closed");

    // Receive the agent's finding.
    if let Some(reply) = reply_rx.recv().await {
        if let AgentPayload::AnalysisResult(result) = reply.payload {
            for f in &result.findings {
                println!(
                    "  finding: [{:?}/{:?}] {} — {}",
                    f.severity, f.category, f.title, f.description
                );
            }
        }
    }

    drop(inbox_tx); // closing the inbox ends the agent's run loop
    let _ = handle.await;
    println!("Done.");
}
