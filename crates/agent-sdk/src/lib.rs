//! # SRE Agent SDK
//!
//! Build **custom investigation agents** for the SRE AI Agents platform.
//!
//! The platform is an orchestrator + specialized sub-agents. Each agent is a
//! long-lived async task that receives [`InboundMessage`]s on an inbox channel,
//! does domain-specific analysis, and replies with [`Finding`]s through the
//! per-investigation reply channel bundled with each message (ADR-006). This crate
//! is the **stable, lightweight surface** a third party implements to add their own
//! agent — it depends only on `sre-common`, not on the platform's Azure layer,
//! store, or orchestrator.
//!
//! Implement [`SreAgent`] and you have an agent the orchestrator can drive.
//!
//! ## Quick start
//!
//! Add the SDK and bring the [`prelude`] into scope:
//!
//! ```no_run
//! use sre_agent_sdk::prelude::*;
//!
//! struct DiskSpaceAgent {
//!     id: AgentId,
//! }
//!
//! #[async_trait]
//! impl SreAgent for DiskSpaceAgent {
//!     fn id(&self) -> &AgentId {
//!         &self.id
//!     }
//!
//!     fn agent_type(&self) -> AgentType {
//!         // Custom agents identify with `AgentType::Custom(kind)`.
//!         AgentType::Custom("disk-space".to_string())
//!     }
//!
//!     fn capabilities(&self) -> Vec<String> {
//!         vec!["disk.utilization".to_string()]
//!     }
//!
//!     async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
//!         // Drain the inbox until the orchestrator drops the sender.
//!         while let Some(inbound) = inbox.recv().await {
//!             let msg = &inbound.message;
//!             if let AgentPayload::AnalysisTask(task) = &msg.payload {
//!                 // ...do real analysis for `task.params.resource_name`...
//!                 let finding = Finding {
//!                     category: FindingCategory::ResourceExhaustion,
//!                     severity: FindingSeverity::High,
//!                     title: "Disk usage 92% on data volume".to_string(),
//!                     description: format!("{} is nearly full", task.params.resource_name),
//!                     confidence: 0.9,
//!                     observed_at: None,
//!                     source_agent: self.id.0.clone(),
//!                     evidence: vec![],
//!                     suggestions: vec!["Expand the volume or prune old data".to_string()],
//!                 };
//!                 let result = AnalysisResult {
//!                     findings: vec![finding],
//!                     summary: "Disk analysis complete".to_string(),
//!                     queries_executed: vec![],
//!                 };
//!                 // Reply on the per-investigation channel, not a shared outbox.
//!                 let response = AgentMessage::new(
//!                     msg.correlation_id,
//!                     self.id.clone(),
//!                     MessageTarget::Orchestrator,
//!                     AgentPayload::AnalysisResult(result),
//!                     0.9,
//!                 )
//!                 .with_parent(msg.id);
//!                 let _ = inbound.reply_tx.send(response).await;
//!             }
//!         }
//!         Ok(())
//!     }
//! }
//! ```
//!
//! See `examples/custom_agent.rs` for a complete, runnable version that wires the
//! channels and drives the agent end-to-end
//! (`cargo run -p sre-agent-sdk --example custom_agent`).
//!
//! ## The contract
//!
//! - **One task, one reply.** For each [`AgentPayload::AnalysisTask`] you receive,
//!   send exactly one reply ([`AgentPayload::AnalysisResult`]) back on
//!   `inbound.reply_tx`, tagged `.with_parent(msg.id)` so the orchestrator can
//!   correlate it.
//! - **Run until the inbox closes.** `run` should loop until `inbox.recv()` returns
//!   `None`; that is the orchestrator's shutdown signal.
//! - **Answer health checks.** Reply to [`AgentPayload::HealthCheck`] with an
//!   [`AgentHealth`] so the orchestrator can probe liveness.
//! - **Stay read-only** unless you are explicitly building a remediation agent —
//!   investigation agents observe, they don't mutate (ADR-004).
//!
//! ## Registering an agent
//!
//! The orchestrator talks to a running agent through an [`AgentHandle`] (an
//! `AgentId`, an `AgentType`, an inbox `Sender`, and the agent's capabilities).
//! The host application spawns `agent.run(inbox)` as a task and registers the
//! handle with its router. Custom agents (`AgentType::Custom`) are dispatched when
//! triage recommends them or via broadcast — wiring triage to recommend a new
//! custom kind is the host's extension point.

use async_trait::async_trait;
use tokio::sync::mpsc;

use sre_common::errors::AgentError;
use sre_common::messages::{AgentId, AgentType, InboundMessage};

/// Core trait for all SRE sub-agents (ADR-003, ADR-006).
///
/// Each agent is a long-lived async task that:
/// 1. Receives tasks via its inbox (`mpsc::Receiver<InboundMessage>`)
/// 2. Processes tasks using its domain expertise and tools
/// 3. Sends findings back via the per-investigation reply channel bundled with
///    each [`InboundMessage`]
#[async_trait]
pub trait SreAgent: Send + Sync {
    /// Agent's unique identifier.
    fn id(&self) -> &AgentId;

    /// Agent's type classification (used for routing).
    fn agent_type(&self) -> AgentType;

    /// Capabilities this agent provides (used for routing / discovery).
    fn capabilities(&self) -> Vec<String>;

    /// Run the agent's main loop, processing messages from `inbox`.
    ///
    /// Each [`InboundMessage`] carries its own `reply_tx` scoped to the
    /// investigation that dispatched the task (ADR-006). Agents send responses
    /// through that channel, not a shared outbox. This method should run until the
    /// inbox channel closes.
    async fn run(&self, inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError>;
}

/// Handle held by the orchestrator to communicate with a running agent.
pub struct AgentHandle {
    pub id: AgentId,
    pub agent_type: AgentType,
    pub sender: mpsc::Sender<InboundMessage>,
    pub capabilities: Vec<String>,
}

/// Everything needed to author an agent, in one glob import.
///
/// ```no_run
/// use sre_agent_sdk::prelude::*;
/// ```
pub mod prelude {
    pub use crate::{AgentHandle, SreAgent};

    pub use async_trait::async_trait;
    pub use tokio::sync::mpsc;

    pub use sre_common::errors::AgentError;
    pub use sre_common::findings::{Finding, FindingCategory, FindingSeverity};
    pub use sre_common::messages::{
        AgentHealth, AgentId, AgentMessage, AgentPayload, AgentStatus, AgentType, AnalysisResult,
        AnalysisTask, AnalysisTaskType, InboundMessage, InvestigationParams, MessageTarget,
        QueryRecord,
    };
}
