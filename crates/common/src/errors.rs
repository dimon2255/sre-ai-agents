use thiserror::Error;

/// Top-level platform errors
#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("Azure API error: {0}")]
    Azure(#[from] AzureError),

    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),

    #[error("Store error: {0}")]
    Store(#[from] StoreError),

    #[error("Orchestrator error: {0}")]
    Orchestrator(#[from] OrchestratorError),

    #[error("Configuration error: {0}")]
    Config(String),
}

/// Errors from Azure API interactions
#[derive(Debug, Error)]
pub enum AzureError {
    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("KQL query failed: {0}")]
    QueryFailed(String),

    #[error("Azure API request failed: {status} {message}")]
    ApiError { status: u16, message: String },

    #[error("Rate limited by Azure API")]
    RateLimited,

    #[error("Query timeout after {0} seconds")]
    QueryTimeout(u64),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Failed to parse Azure response: {0}")]
    ParseError(String),
}

/// Errors from agent operations
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Agent {agent_id} timed out after {timeout_secs}s")]
    Timeout { agent_id: String, timeout_secs: u64 },

    #[error("Agent {agent_id} failed: {reason}")]
    ExecutionFailed { agent_id: String, reason: String },

    #[error("Agent {agent_id} channel closed")]
    ChannelClosed { agent_id: String },

    #[error("Agent {agent_id} received invalid task: {reason}")]
    InvalidTask { agent_id: String, reason: String },
}

/// Errors from the persistence layer
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Workflow {0} not found")]
    WorkflowNotFound(String),

    #[error("Migration failed: {0}")]
    MigrationFailed(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Errors from orchestrator operations
#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("Invalid workflow state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    #[error("No agent available for capability: {0}")]
    NoAgentAvailable(String),

    #[error("Investigation {0} already in progress")]
    DuplicateInvestigation(String),

    #[error("Max concurrent investigations ({0}) reached")]
    CapacityExhausted(u32),
}
