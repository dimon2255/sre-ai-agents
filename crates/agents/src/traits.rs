use async_trait::async_trait;
use tokio::sync::mpsc;

use sre_common::errors::AgentError;
use sre_common::messages::{AgentId, AgentType, InboundMessage};

/// Core trait for all SRE sub-agents (ADR-003, ADR-006).
///
/// Each agent is a long-lived async task that:
/// 1. Receives tasks via its inbox (mpsc::Receiver<InboundMessage>)
/// 2. Processes tasks using its domain expertise and tools
/// 3. Sends findings back via the per-investigation reply channel
///    bundled with each InboundMessage
#[async_trait]
pub trait SreAgent: Send + Sync {
    /// Agent's unique identifier
    fn id(&self) -> &AgentId;

    /// Agent's type classification
    fn agent_type(&self) -> AgentType;

    /// Capabilities this agent provides (used for routing)
    fn capabilities(&self) -> Vec<String>;

    /// Run the agent's main loop, processing messages from inbox.
    ///
    /// Each InboundMessage carries its own reply_tx scoped to the
    /// investigation that dispatched the task (ADR-006). Agents send
    /// responses through that channel, not a shared outbox.
    ///
    /// This method should run until the inbox channel closes.
    async fn run(
        &self,
        inbox: mpsc::Receiver<InboundMessage>,
    ) -> Result<(), AgentError>;
}

/// Handle held by the orchestrator to communicate with a running agent
pub struct AgentHandle {
    pub id: AgentId,
    pub agent_type: AgentType,
    pub sender: mpsc::Sender<InboundMessage>,
    pub capabilities: Vec<String>,
}
