use async_trait::async_trait;
use tokio::sync::mpsc;

use sre_common::errors::AgentError;
use sre_common::messages::{AgentId, AgentMessage, AgentType};

/// Core trait for all SRE sub-agents.
///
/// Each agent is a long-lived async task that:
/// 1. Receives tasks via its inbox (mpsc::Receiver)
/// 2. Processes tasks using its domain expertise and tools
/// 3. Sends findings back to the orchestrator via its outbox (mpsc::Sender)
#[async_trait]
pub trait SreAgent: Send + Sync {
    /// Agent's unique identifier
    fn id(&self) -> &AgentId;

    /// Agent's type classification
    fn agent_type(&self) -> AgentType;

    /// Capabilities this agent provides (used for routing)
    fn capabilities(&self) -> Vec<String>;

    /// Run the agent's main loop, processing messages from inbox
    /// and sending results to outbox.
    ///
    /// This method should run until the inbox channel closes.
    async fn run(
        &self,
        inbox: mpsc::Receiver<AgentMessage>,
        outbox: mpsc::Sender<AgentMessage>,
    ) -> Result<(), AgentError>;
}

/// Handle held by the orchestrator to communicate with a running agent
pub struct AgentHandle {
    pub id: AgentId,
    pub agent_type: AgentType,
    pub sender: mpsc::Sender<AgentMessage>,
    pub capabilities: Vec<String>,
}
