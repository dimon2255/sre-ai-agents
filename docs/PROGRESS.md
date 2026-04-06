# SRE AI Agents Platform — Progress Tracker

**Project**: Site Reliability Engineering Multi-Agent Platform
**Language**: Rust
**Pattern**: Orchestrator + Specialized Sub-Agents
**Repository**: https://github.com/dimon2255/sre-ai-agents
**Started**: 2026-04-05

---

## Phase 1 — Azure MVP: Read-Only Investigation

**Status**: Planning Complete — Implementation Not Started
**Duration**: 6-8 weeks
**Goal**: Prove the orchestrator + agent pattern works against real Azure data

### Scope

| Component | Status | Notes |
|-----------|--------|-------|
| Project skeleton (Cargo workspace) | Done | Workspace with 5 crates |
| Orchestrator (workflow state machine, routing, fan-out) | Not Started | `crates/orchestrator/` |
| TriageAgent (alert parsing, severity classification) | Not Started | `crates/agents/src/triage.rs` |
| LogsAgent (KQL against Log Analytics) | Not Started | `crates/agents/src/logs.rs` |
| MetricsAgent (KQL against App Insights) | Not Started | `crates/agents/src/metrics.rs` |
| AzureQueryClient (shared KQL executor) | Not Started | `crates/azure/src/query_client.rs` |
| AzureAlertsClient (alert ingestion) | Not Started | `crates/azure/src/alerts_client.rs` |
| KQL Templates (parameterized queries) | Not Started | `crates/azure/src/kql_templates.rs` |
| Common Alert Schema webhook handler | Not Started | `src/webhook.rs` |
| CLI interface (manual investigation trigger) | Not Started | `src/main.rs` |
| SQLite state store | Not Started | `crates/store/` |
| OpenTelemetry instrumentation | Not Started | Across all crates |

### Exit Criteria

- [ ] Webhook receives Azure alert -> investigation completes in < 60s
- [ ] TriageAgent correctly classifies severity and affected resource
- [ ] LogsAgent returns relevant error patterns from Log Analytics
- [ ] MetricsAgent returns latency/error-rate analysis from App Insights
- [ ] Parallel agent execution (LogsAgent + MetricsAgent run concurrently)
- [ ] Full audit trail in SQLite for every investigation
- [ ] Structured JSON findings report generated per investigation
- [ ] Agent timeout + circuit breaking works
- [ ] CI pipeline builds and tests pass

### Key Decisions

- In-process only (tokio::spawn + mpsc channels)
- KQL templates, not freeform queries (cost + safety)
- Read-only (no write operations against Azure)
- SQLite for zero-ops persistence
- No LLM reasoning (rule-based triage, template-driven analysis)

---

## Phase 2 — Production Hardening: Gated Remediation

**Status**: Not Started
**Duration**: 10-14 weeks
**Goal**: Production-grade platform with human-in-the-loop remediation

### Scope

| Component | Status | Notes |
|-----------|--------|-------|
| TraceAgent (App Insights distributed traces) | Not Started | |
| InfraAgent (Azure Activity Log, Resource Health) | Not Started | |
| RemediationAgent (proposals, risk levels, rollback plans) | Not Started | |
| Approval Gate (Slack/Teams integration) | Not Started | |
| gRPC API (external integration surface) | Not Started | |
| PostgreSQL (replace SQLite) | Not Started | |
| NATS JetStream (event bus, horizontal scaling) | Not Started | |
| Notification Engine (Slack, Teams, PagerDuty) | Not Started | |
| Remediation guardrails (rate limits, blast radius) | Not Started | |
| ARM REST API write operations (scoped Contributor) | Not Started | |

### Exit Criteria

- [ ] TraceAgent correlates distributed traces and identifies dependency failures
- [ ] InfraAgent detects recent deployments/config changes correlated with incidents
- [ ] RemediationAgent proposes actions with risk levels and rollback plans
- [ ] Approval gate works end-to-end (Slack -> approve -> execute -> verify)
- [ ] Rate limiting prevents runaway remediation
- [ ] Destructive actions require dual approval
- [ ] PostgreSQL stores full audit history
- [ ] NATS enables message replay for debugging
- [ ] gRPC API allows external systems to trigger and query investigations
- [ ] Platform survives agent crashes gracefully
- [ ] Load tested: 50 concurrent investigations without degradation

### Key Decisions (Planned)

- NATS JetStream for event bus (enables horizontal scaling + replay)
- RiskLevel enum: ReadOnly / LowRisk / HighRisk / Destructive
- Auto-approve only at confidence >= 0.95 and RiskLevel::LowRisk
- `SreDataSource` trait introduced for multi-provider abstraction

---

## Phase 3 — Intelligent Autonomy: LLM Reasoning & Learning

**Status**: Not Started
**Duration**: Ongoing
**Goal**: Reduce MTTR through intelligent correlation and trusted automated response

### Scope

| Component | Status | Notes |
|-----------|--------|-------|
| ReasoningEngine (LLM-powered RCA synthesis) | Not Started | |
| KnowledgeStore (Qdrant vector DB, past incidents) | Not Started | |
| LearningLoop (outcome tracking, confidence adjustment) | Not Started | |
| AutoRemediationEngine (pre-approved playbooks) | Not Started | |
| Agent SDK (published crate for custom agents) | Not Started | |
| Multi-workspace investigation | Not Started | |
| ChatOps interface (Slack/Teams conversational) | Not Started | |
| Progressive trust model | Not Started | |

### Exit Criteria

- [ ] LLM reasoning correctly synthesizes RCA in >80% of test incidents
- [ ] Knowledge store retrieves relevant past incidents with >85% relevance
- [ ] Auto-remediation: zero false positives over 30-day evaluation
- [ ] Learning loop improves confidence accuracy over time
- [ ] Agent SDK documented + 1 custom agent built by another team
- [ ] ChatOps handles natural language investigation queries
- [ ] Multi-workspace works across 3+ Log Analytics workspaces
- [ ] Progressive trust model correctly adjusts thresholds

---

## Architecture Decision Records

| ADR | Title | Status |
|-----|-------|--------|
| [ADR-001](adr/001-rust-orchestrator-subagent-pattern.md) | Rust + Orchestrator-SubAgent Pattern | Accepted |
| [ADR-002](adr/002-azure-first-observability-stack.md) | Azure-First Observability Stack | Accepted |
| [ADR-003](adr/003-event-driven-agent-communication.md) | Event-Driven Agent Communication | Accepted |
| [ADR-004](adr/004-read-only-phase1-safety-model.md) | Read-Only Phase 1 Safety Model | Accepted |
| [ADR-005](adr/005-kql-template-driven-queries.md) | KQL Template-Driven Queries | Accepted |

---

## Known Issues & Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Azure API throttling (200 req/min) | High | Rate limiter (governor crate), query budgets per agent |
| KQL query cost on large workspaces | Medium | Timespan limits, row caps, parameterized templates |
| Agent platform down during incident | Critical | 99.9% availability SLO, graceful degradation, circuit breaking |
| Token/credential management | High | Managed Identity in Azure, DefaultAzureCredential fallback |
