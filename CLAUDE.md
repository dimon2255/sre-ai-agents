# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build --workspace          # Build all crates
cargo test --workspace           # Run all tests (46 integration + unit)
cargo test -p sre-store          # Test a single crate
cargo test -p sre-azure -- kql   # Run tests matching "kql" in sre-azure
cargo clippy --workspace         # Lint
cargo fmt --all -- --check       # Check formatting
cargo run -- investigate --resource my-app --severity Sev1   # Manual investigation
cargo run -- serve               # Start webhook server (port 8080)
```

Set `RUST_LOG` to control tracing output (default: `info,sre_orchestrator=debug,sre_agents=debug`).

## Architecture

This is an orchestrator + sub-agents platform for automated SRE incident investigation against Azure Monitor / Log Analytics / Application Insights.

### Workspace Crates

- **`sre-common`** — Shared types: `AgentMessage` envelope, `AlertInfo`, `Finding`, `InvestigationReport`, `PlatformConfig`, error hierarchy. All inter-crate communication types live here.
- **`sre-azure`** — Azure API layer: `TokenProvider` trait for auth, `AzureQueryClient` for KQL execution via REST API, `AzureAlertsClient` for Common Alert Schema parsing, parameterized KQL templates in `kql_templates.rs`.
- **`sre-agents`** — `SreAgent` trait (async inbox/outbox pattern) + implementations: `TriageAgent`, `LogsAgent`, `MetricsAgent`. Each agent is a long-lived tokio task processing messages from an `mpsc::Receiver` and sending results via `mpsc::Sender`.
- **`sre-store`** — Persistence: `WorkflowStore` + `EventLog` traits with `SqliteStore` implementation. Auto-creates DB file on first run. Tables: `workflows`, `event_log`.
- **`sre-orchestrator`** — Investigation engine: `Orchestrator` drives the workflow state machine, `AgentRouter` routes messages by agent type, `FindingAggregator` merges results, `InvestigationWorkflow` tracks state transitions.
- **Root crate** (`src/main.rs`, `src/webhook.rs`) — CLI entrypoint (clap), bootstrap wiring, webhook handler skeleton.

### Key Patterns

**Agent communication**: Agents use typed `AgentMessage` envelopes with `AgentPayload` variants, routed through `tokio::sync::mpsc` channels. The orchestrator holds the single inbox receiver; each agent gets its own sender/receiver pair via `AgentHandle`.

**Investigation workflow state machine**: `Created → Triaging → Investigating → Synthesizing → Resolved|Escalated|Failed`. Transitions validated by `WorkflowState::can_transition_to()`. Escalation triggers when aggregated confidence is below threshold.

**KQL safety (ADR-005)**: All KQL queries are generated from parameterized templates in `sre-azure/src/kql_templates.rs` — no freeform KQL construction. Resource names are sanitized, timespans clamped to 1–1440 minutes.

**Auth abstraction**: `TokenProvider` trait in `sre-azure/src/auth.rs` enables testing without Azure. `MockTokenProvider` used in dev; `AzureCredentialProvider` wraps `DefaultAzureCredential` for production.

### Configuration

`config/default.toml` — loaded by the `config` crate with `SRE__` env var overrides (e.g., `SRE__AZURE__SUBSCRIPTION_ID`). Config struct: `sre_common::config::PlatformConfig`.

### Phase Roadmap

Phase 1 (current): Read-only investigation — all agents are read-only, no remediation actions. Phase 2: Gated remediation with approval gates. Phase 3: LLM reasoning and auto-remediation. ADRs in `docs/adr/` document key decisions.
