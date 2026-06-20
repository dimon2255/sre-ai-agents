//! The agent contract now lives in the published `sre-agent-sdk` crate (ADR-019)
//! so third parties can build custom agents without depending on the platform's
//! internals (Azure layer, store, orchestrator). The first-party agents in this
//! crate are simply one consumer of that contract.
//!
//! Re-exported here so existing `sre_agents::SreAgent` / `crate::traits::SreAgent`
//! paths keep working unchanged.

pub use sre_agent_sdk::{AgentHandle, SreAgent};
