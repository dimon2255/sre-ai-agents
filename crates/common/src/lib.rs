pub mod config;
pub mod errors;
pub mod findings;
pub mod messages;

// Re-export commonly used types
pub use config::PlatformConfig;
pub use errors::PlatformError;
pub use findings::{Finding, FindingCategory, FindingSeverity, InvestigationReport};
pub use messages::{
    AgentId, AgentMessage, AgentPayload, AgentStatus, AgentType, AlertInfo, AlertSeverity,
    InvestigationContext, MessageTarget,
};
