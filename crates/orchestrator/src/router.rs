use std::collections::HashMap;

use tracing::debug;

use sre_agents::AgentHandle;
use sre_common::messages::{AgentMessage, AgentType};

/// Capability-based agent router.
///
/// Routes tasks to agents based on their registered capabilities,
/// not hard-coded type mappings. This allows adding new agents
/// without modifying routing logic.
pub struct AgentRouter {
    agents: HashMap<String, AgentHandle>,
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

    /// Route a message to the appropriate agent by type
    pub async fn route_to_type(
        &self,
        agent_type: &AgentType,
        message: AgentMessage,
    ) -> Result<(), RouteError> {
        let handle = self
            .agents
            .values()
            .find(|h| &h.agent_type == agent_type)
            .ok_or_else(|| RouteError::NoAgent(agent_type.to_string()))?;

        handle
            .sender
            .send(message)
            .await
            .map_err(|_| RouteError::ChannelClosed(handle.id.to_string()))?;

        Ok(())
    }

    /// Route a message to a specific agent by ID
    pub async fn route_to_id(
        &self,
        agent_id: &str,
        message: AgentMessage,
    ) -> Result<(), RouteError> {
        let handle = self
            .agents
            .get(agent_id)
            .ok_or_else(|| RouteError::NoAgent(agent_id.to_string()))?;

        handle
            .sender
            .send(message)
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
