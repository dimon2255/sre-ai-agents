# SRE AI Agents Platform — Progress Tracker

**Project**: Site Reliability Engineering Multi-Agent Platform
**Language**: Rust
**Pattern**: Orchestrator + Specialized Sub-Agents
**Repository**: https://github.com/dimon2255/sre-ai-agents
**Started**: 2026-04-05

---

## Phase 1 — Azure MVP: Read-Only Investigation

**Status**: Complete (2026-04-11)
**Duration**: 2026-04-05 to 2026-04-11
**Goal**: Prove the orchestrator + agent pattern works against real Azure data

### Scope

| Component | Status | Notes |
|-----------|--------|-------|
| Project skeleton (Cargo workspace) | Done | Workspace with 5 crates + root lib/bin |
| Orchestrator (workflow state machine, routing, fan-out) | Done | `crates/orchestrator/` — per-investigation channels (ADR-006), concurrent investigations via Arc + semaphore |
| TriageAgent (alert parsing, severity classification) | Done | `crates/agents/src/triage.rs` |
| LogsAgent (KQL against Log Analytics) | Done | `crates/agents/src/logs.rs` |
| MetricsAgent (KQL against App Insights) | Done | `crates/agents/src/metrics.rs` |
| AzureQueryClient (shared KQL executor) | Done | `crates/azure/src/query_client.rs` — base URL injection for testing |
| AzureAlertsClient (alert ingestion) | Done | `crates/azure/src/alerts_client.rs` |
| KQL Templates (parameterized queries) | Done | `crates/azure/src/kql_templates.rs` |
| Common Alert Schema webhook handler | Done | `src/webhook.rs` — Axum server with 3-tier health probes + orchestrator channel |
| CLI interface (manual investigation trigger) | Done | `src/main.rs` |
| SQLite state store | Done | `crates/store/` — auto-creates DB, full audit trail |
| Config-driven auth mode | Done | `azure.auth_mode = "mock" \| "azure"` in config |
| OpenTelemetry instrumentation | Deferred | Moved to Phase 2 |
| CI/CD pipeline (GitHub Actions) | Done | `.github/workflows/` — lint, test, build, security audit, docker smoke |
| Multi-stage Dockerfile | Done | `Dockerfile` — rust:1.86 builder + debian-slim runtime, non-root user |
| Health probes (/startupz, /healthz, /readyz) | Done | 3-tier: startup gate, heartbeat liveness (no I/O), readiness (channel + semaphore + heartbeat + SQLite) |
| Release profile optimization | Done | LTO, strip, codegen-units=1 in Cargo.toml |
| Serialization roundtrip tests | Done | All AgentPayload variants validated, InboundMessage boundary documented for Phase 2 NATS |
| ADR-007: CI/CD + ACA deployment | Done | `docs/adr/007-cicd-containerization-aca-deployment.md` |
| Request body size limit | Done | 64KB DefaultBodyLimit on all routes |
| Graceful shutdown drain | Done | Semaphore-based drain with 25s deadline (ACA SIGTERM margin) |
| Concurrent investigation support | Done | Arc-wrapped orchestrator, spawned tasks, independent heartbeat tick |

### Code Review Fixes

**Round 1 (2026-04-06)**

| Issue | Severity | Status | Fix |
|-------|----------|--------|-----|
| Shared inbox race condition | Blocker | Fixed | Per-investigation reply channels (ADR-006) |
| Dead mock-auth fallback path | Blocker | Fixed | Config-driven `auth_mode` field in `AzureConfig` |
| Query client tests bypass real client | Blocker | Fixed | Base URL injection + wiremock tests exercise actual `AzureQueryClient` |
| Duration cast i64→u64 without bounds | Suggestion | Fixed | `.max(0)` guard in `workflow.rs` |
| Agent bootstrap code repetition | Suggestion | Fixed | `spawn_agent()` helper in `main.rs` |

**Round 2 (2026-04-11)**

| Issue | Severity | Status | Fix |
|-------|----------|--------|-----|
| Heartbeat starvation + serial investigations | Blocker | Fixed | Decoupled heartbeat tick, Arc-wrapped orchestrator, concurrent task spawning |
| Tests re-implement production code | Suggestion | Fixed | Extracted `src/lib.rs`, tests import production `AppState` + handlers |
| Duplicate SqliteStore in run_server | Suggestion | Fixed | `bootstrap()` returns shared store; single connection pool |
| Temp dir leak in tests (std::mem::forget) | Suggestion | Fixed | Return `TempDir` in test setup tuple |
| Stale Rust version in ADR-007 | Nit | Fixed | Updated 1.75 → 1.86 |
| No request body size limit | Suggestion | Fixed | 64KB `DefaultBodyLimit` layer |
| No graceful investigation drain | Suggestion | Fixed | Semaphore acquire_many with 25s timeout on shutdown |

### Test Coverage

- **65 tests** across all crates (zero failures, zero clippy warnings)
- Store layer: 12 tests (CRUD, state transitions, event log)
- Agent layer: 11 tests (triage, logs, metrics, capabilities)
- Azure layer: 16 tests (query client via wiremock, alert parsing, KQL templates)
- Orchestrator: 9 tests (full investigation flow, state persistence, aggregation)
- Common layer: 10 tests (serialization roundtrips for all message/payload types)
- Webhook/E2E: 7 tests (health probes, alert ingestion, self-exclusion, end-to-end investigation)

### Exit Criteria

- [x] Webhook receives Azure alert -> investigation completes in < 60s
- [x] TriageAgent correctly classifies severity and affected resource
- [x] LogsAgent returns relevant error patterns from Log Analytics
- [x] MetricsAgent returns latency/error-rate analysis from App Insights
- [x] Parallel agent execution (LogsAgent + MetricsAgent run concurrently)
- [x] Full audit trail in SQLite for every investigation
- [x] Structured JSON findings report generated per investigation
- [x] Agent timeout + circuit breaking works
- [x] CI pipeline builds and tests pass
- [x] Concurrent investigations with semaphore-based concurrency control
- [x] Graceful shutdown drains in-flight investigations
- [x] Integration tests exercise production code paths (not mirrors)

### Key Decisions

- In-process only (tokio::spawn + mpsc channels)
- Per-investigation reply channels eliminate shared-inbox race (ADR-006)
- KQL templates, not freeform queries (cost + safety)
- Read-only (no write operations against Azure)
- SQLite for zero-ops persistence
- Config-driven auth mode (`mock` for dev, `azure` for production)
- No LLM reasoning (rule-based triage, template-driven analysis)
- Arc-wrapped orchestrator with `investigate(&self)` enables concurrent spawning
- Independent heartbeat tick prevents liveness probe starvation during long investigations
- Webhook auth deferred to Phase 2 (ADR-008) — Phase 1 relies on ACA ingress isolation

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

### Planned ADRs

- **ADR-008: Webhook Authentication** — Secure the `/api/v1/alerts/azure` endpoint. Options: shared secret in custom header (`X-Webhook-Secret`), Azure AD bearer token validation, or both. Phase 1 runs behind ACA ingress with no public auth; Phase 2 must add authentication before exposing to external action groups.

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
| [ADR-003](adr/003-event-driven-agent-communication.md) | Event-Driven Agent Communication | Accepted (amended by ADR-006) |
| [ADR-004](adr/004-read-only-phase1-safety-model.md) | Read-Only Phase 1 Safety Model | Accepted |
| [ADR-005](adr/005-kql-template-driven-queries.md) | KQL Template-Driven Queries | Accepted |
| [ADR-006](adr/006-per-investigation-reply-channels.md) | Per-Investigation Reply Channels | Accepted |
| [ADR-007](adr/007-cicd-containerization-aca-deployment.md) | CI/CD, Containerization, ACA Deployment | Accepted |

---

## Known Issues & Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Azure API throttling (200 req/min) | High | Rate limiter (governor crate), query budgets per agent |
| KQL query cost on large workspaces | Medium | Timespan limits, row caps, parameterized templates |
| Agent platform down during incident | Critical | 99.9% availability SLO, graceful degradation, circuit breaking |
| Token/credential management | High | Managed Identity in Azure, config-driven auth mode (mock for dev) |
| Unauthenticated webhook endpoint | Medium | Phase 1: ACA ingress only. Phase 2: ADR-008 (shared secret / Azure AD) |
