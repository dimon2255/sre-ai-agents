# SRE AI Agents Platform

An AI-driven Site Reliability Engineering platform built in Rust, using an orchestrator + sub-agents pattern to automate incident investigation, alert triage, and root cause analysis against Azure Monitor, Log Analytics, and Application Insights.

## Architecture

```
Azure Alert (webhook) ──► Orchestrator ──┬── TriageAgent
       or CLI                            ├── LogsAgent ──► Log Analytics (KQL)
                                         └── MetricsAgent ──► App Insights (KQL)
                                                │
                                         Findings Report (JSON)
```

The orchestrator coordinates specialized agents that run as async Tokio tasks, communicating via typed message channels. Each agent queries Azure observability APIs using parameterized KQL templates and returns structured findings with confidence scores.

## Project Structure

```
crates/
├── common/        # Shared types: messages, findings, config, errors
├── store/         # Persistence: WorkflowStore + EventLog traits, SQLite backend
├── azure/         # Azure API clients: KQL executor, alerts, auth, KQL templates
├── agents/        # SreAgent trait + TriageAgent, LogsAgent, MetricsAgent
└── orchestrator/  # Workflow engine, agent router, finding aggregator, timeouts
```

## Quick Start

```bash
# Build
cargo build --workspace

# Run tests
cargo test --workspace

# Manual investigation (requires Azure credentials)
cargo run -- investigate --resource my-app --severity Sev1

# Start webhook server
cargo run -- serve
```

## Configuration

Copy and edit `config/default.toml`:

```toml
[azure]
subscription_id = "your-subscription-id"
workspace_ids = ["your-log-analytics-workspace-id"]
app_insights_ids = ["your-app-insights-app-id"]

[orchestrator]
agent_timeout_secs = 30
max_concurrent_investigations = 10
```

Azure authentication uses `DefaultAzureCredential` (Managed Identity in Azure, Azure CLI locally).

## Phased Roadmap

| Phase | Focus | Status |
|-------|-------|--------|
| **Phase 1** | Read-only investigation (Log Analytics + App Insights) | In Progress |
| **Phase 2** | Gated remediation, TraceAgent, InfraAgent, approval gates | Planned |
| **Phase 3** | LLM reasoning, knowledge store, auto-remediation | Planned |

See [docs/PROGRESS.md](docs/PROGRESS.md) for detailed tracking and [docs/adr/](docs/adr/) for architecture decisions.

## License

MIT
