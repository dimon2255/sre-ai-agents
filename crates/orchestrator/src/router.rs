use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::{debug, instrument};

use sre_agents::AgentHandle;
use sre_common::messages::{AgentMessage, AgentType, InboundMessage};

/// Capability-based agent router (ADR-003, ADR-006).
///
/// Routes tasks to agents based on their registered capabilities.
/// Each dispatched message includes a per-investigation reply channel
/// so agents respond directly to the investigation that sent the task.
pub struct AgentRouter {
    agents: HashMap<String, AgentHandle>,
}

impl Default for AgentRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRouter {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// Register an agent handle for routing
    pub fn register(&mut self, handle: AgentHandle) {
        debug!(
            agent_id = %handle.id,
            agent_type = %handle.agent_type,
            capabilities = ?handle.capabilities,
            "Agent registered with router"
        );
        self.agents.insert(handle.id.0.clone(), handle);
    }

    /// Route a message to the appropriate agent by type, with a reply channel
    #[instrument(skip(self, message, reply_tx), fields(agent_type = %agent_type))]
    pub async fn route_to_type(
        &self,
        agent_type: &AgentType,
        message: AgentMessage,
        reply_tx: mpsc::Sender<AgentMessage>,
    ) -> Result<(), RouteError> {
        let handle = self
            .agents
            .values()
            .find(|h| &h.agent_type == agent_type)
            .ok_or_else(|| RouteError::NoAgent(agent_type.to_string()))?;

        handle
            .sender
            .send(InboundMessage { message, reply_tx })
            .await
            .map_err(|_| RouteError::ChannelClosed(handle.id.to_string()))?;

        Ok(())
    }

    /// Route a message to a specific agent by ID, with a reply channel
    pub async fn route_to_id(
        &self,
        agent_id: &str,
        message: AgentMessage,
        reply_tx: mpsc::Sender<AgentMessage>,
    ) -> Result<(), RouteError> {
        let handle = self
            .agents
            .get(agent_id)
            .ok_or_else(|| RouteError::NoAgent(agent_id.to_string()))?;

        handle
            .sender
            .send(InboundMessage { message, reply_tx })
            .await
            .map_err(|_| RouteError::ChannelClosed(agent_id.to_string()))?;

        Ok(())
    }

    /// Get all registered agent types
    pub fn registered_types(&self) -> Vec<AgentType> {
        self.agents.values().map(|h| h.agent_type.clone()).collect()
    }

    /// Check if an agent type is available
    pub fn has_agent_type(&self, agent_type: &AgentType) -> bool {
        self.agents.values().any(|h| &h.agent_type == agent_type)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("No agent available for: {0}")]
    NoAgent(String),

    #[error("Agent channel closed: {0}")]
    ChannelClosed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::messages::{AgentId, AgentPayload, MessageTarget};

    fn handle(id: &str, agent_type: AgentType) -> (AgentHandle, mpsc::Receiver<InboundMessage>) {
        let (tx, rx) = mpsc::channel(8);
        (
            AgentHandle {
                id: AgentId(id.to_string()),
                agent_type,
                sender: tx,
                capabilities: vec!["cap".to_string()],
            },
            rx,
        )
    }

    fn msg() -> AgentMessage {
        AgentMessage::new(
            uuid::Uuid::new_v4(),
            AgentId("orchestrator".to_string()),
            MessageTarget::Orchestrator,
            AgentPayload::HealthCheck,
            1.0,
        )
    }

    #[test]
    fn default_and_new_are_empty() {
        let r = AgentRouter::default();
        assert!(r.registered_types().is_empty());
        assert!(!r.has_agent_type(&AgentType::Logs));
    }

    #[tokio::test]
    async fn register_and_route_to_type() {
        let mut router = AgentRouter::new();
        let (h, mut rx) = handle("logs-agent", AgentType::Logs);
        router.register(h);

        assert!(router.has_agent_type(&AgentType::Logs));
        assert_eq!(router.registered_types(), vec![AgentType::Logs]);

        let (reply_tx, _reply_rx) = mpsc::channel(8);
        router
            .route_to_type(&AgentType::Logs, msg(), reply_tx)
            .await
            .unwrap();
        // message landed in the agent inbox
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn route_to_type_with_no_agent_errors() {
        let router = AgentRouter::new();
        let (reply_tx, _r) = mpsc::channel(8);
        let err = router
            .route_to_type(&AgentType::Metrics, msg(), reply_tx)
            .await
            .unwrap_err();
        assert!(matches!(err, RouteError::NoAgent(_)));
        assert!(err.to_string().contains("metrics"));
    }

    #[tokio::test]
    async fn route_to_id_success_and_missing() {
        let mut router = AgentRouter::new();
        let (h, mut rx) = handle("triage-agent", AgentType::Triage);
        router.register(h);

        let (reply_tx, _r) = mpsc::channel(8);
        router
            .route_to_id("triage-agent", msg(), reply_tx)
            .await
            .unwrap();
        assert!(rx.recv().await.is_some());

        let (reply_tx2, _r2) = mpsc::channel(8);
        let err = router
            .route_to_id("ghost", msg(), reply_tx2)
            .await
            .unwrap_err();
        assert!(matches!(err, RouteError::NoAgent(_)));
    }

    #[tokio::test]
    async fn route_to_closed_inbox_errors() {
        let mut router = AgentRouter::new();
        let (h, rx) = handle("logs-agent", AgentType::Logs);
        router.register(h);
        drop(rx); // close the agent inbox

        let (reply_tx, _r) = mpsc::channel(8);
        let err = router
            .route_to_type(&AgentType::Logs, msg(), reply_tx)
            .await
            .unwrap_err();
        assert!(matches!(err, RouteError::ChannelClosed(_)));
    }
}
