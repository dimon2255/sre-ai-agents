//! ChatOps interface (Phase 3, ADR-018) — a conversational entry point.
//!
//! The phase-3 architecture diagram lists a ChatOps bot (Slack/Teams) as a
//! first-class entry point alongside Azure Alerts and the gRPC API. This crate is
//! the **transport-agnostic core**: a pure command parser plus a handler that
//! dispatches commands to a [`ChatBackend`] and formats the reply as Slack-style
//! mrkdwn. It depends only on `sre-common`.
//!
//! The platform's trait + mock shape applies: [`ChatBackend`] abstracts the
//! "trigger an investigation / look up its status" operations so the handler is
//! fully testable offline; the webhook server implements it over the alert
//! channel + the store, and a Slack slash-command endpoint drives it.

pub mod command;
pub mod handler;

pub use command::{parse_command, parse_severity, ChatCommand};
pub use handler::{ChatBackend, ChatOpsHandler, ChatResponse, InvestigationStatus};
