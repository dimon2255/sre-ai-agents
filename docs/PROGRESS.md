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

**Status**: In Progress
**Duration**: 10-14 weeks (three sub-phases)
**Goal**: Production-grade platform with human-in-the-loop remediation

### Phase 2a — Staging Deploy & Observability (3-4 weeks)

**Status**: In Progress (started 2026-04-12)

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 1 | Staging deployment (Terraform + ACA) | Done | `infra/staging/`, release workflow, OIDC auth, deployed to `aca-sre-agents-demo` |
| 2 | OpenTelemetry instrumentation | Done | 15 spans across critical path, OTLP HTTP exporter, config-driven (`SRE__OBSERVABILITY__ENABLED`) |
| 3 | PostgreSQL migration (replace SQLite) | Done | Hard cut-over shipped 2026-04-19: `sqlite` feature removed, `PgStore` replaces `SqliteStore` across orchestrator/webhook/tests. Integration tests use `testcontainers-modules` (postgres:16-alpine pinned). Smoke test in `docker.yml` runs against a Postgres sidecar on a shared docker network. Staging DB provisioned via `azurerm_postgresql_flexible_server_database.app`; app role created out-of-band via `infra/staging/create-app-user.sql`. Password auth in ACA secret; MI follow-up tied to ADR-011. **Schema modernization (TEXT→UUID/JSONB/TIMESTAMPTZ, f64 precision fix, composite index) scheduled as a follow-up corrective migration — see ADR-012.** |
| 4 | TraceAgent (distributed traces) | Not Started | App Insights trace correlation |
| 5 | InfraAgent (Activity Log, Resource Health) | Not Started | Azure Activity Log + Resource Health API |
| 6 | Webhook authentication (ADR-008) | Done | ADR-008 Accepted 2026-04-21. HMAC-SHA256 middleware (`src/middleware/hmac.rs`) wired on `/api/v1/alerts/azure` only, `{timestamp}.{body}` signed payload with 5-min skew window, constant-time compare via `subtle::ConstantTimeEq`. Config: `WebhookConfig.hmac_secret` (required in staging) + `hmac_skew_seconds` (default 300). 7 integration + 5 unit tests green, clippy `-D warnings` clean. Terraform wired: new `webhook_hmac_secret` var with ≥32-char validation, ACA secret `webhook-hmac-secret`, env `SRE__WEBHOOK__HMAC_SECRET`. Release workflow reads `secrets.SRE_WEBHOOK_HMAC_SECRET` — operator must add this to the GitHub `staging` environment before the next release run. AAD bearer deferred to Phase 2b. |
| 7 | Remediation safety model (ADR-009 draft) | Draft | ADR-009 in review (2026-04-22) — `docs/adr/009-remediation-safety-model.md`. Threat model (STRIDE + MITRE ATT&CK), RiskLevel enum (ReadOnly/LowRisk/HighRisk/Destructive), auto-approve policy, guardrails, rollback contract, audit schema additions, forward-ref to ADR-011 for identity separation. Implementation is Phase 2c. |
| 8 | Load test baseline | Not Started | Target: 50 concurrent investigations, P99 < 5s |

#### Phase 2a Exit Criteria

- [x] Terraform deploys ACA app to staging via GitHub Actions release workflow
- [x] OpenTelemetry traces flow across orchestrator and all agents
- [x] PostgreSQL stores full audit history (replaces SQLite)
- [ ] TraceAgent correlates distributed traces and identifies dependency failures
- [ ] InfraAgent detects recent deployments/config changes correlated with incidents
- [x] Webhook endpoint authenticated (ADR-008 implemented, 2026-04-21)
- [ ] Load test baseline established (50 concurrent investigations without degradation)

### Phase 2b — Scaling Infrastructure (3-4 weeks)

**Status**: Not Started

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 1 | NATS JetStream integration (ADR-010) | Not Started | Event bus replacing mpsc channels |
| 2 | Horizontal scaling (multi-replica) | Not Started | Requires NATS for work distribution |
| 3 | Notification Engine (Slack, Teams, PagerDuty) | Not Started | Rides on NATS subjects |
| 4 | InboundMessage serialization migration | Not Started | Replace `mpsc::Sender` with NATS reply subject |
| 5 | gRPC API (external integration surface) | Not Started | tonic-based API for external systems |

#### Phase 2b Exit Criteria

- [ ] NATS enables message replay for debugging
- [ ] Platform runs multi-replica behind ACA scaling
- [ ] Notification engine delivers to Slack/Teams/PagerDuty
- [ ] gRPC API allows external systems to trigger and query investigations
- [ ] Load test re-validated: 50 concurrent investigations after NATS migration

### Phase 2c — Gated Remediation (4-5 weeks)

**Status**: Not Started

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 1 | RemediationAgent (proposals, risk levels, rollback plans) | Not Started | Playbook-driven |
| 2 | Approval Gate — separate `sre-gates` crate | Not Started | Security boundary; Slack/Teams interactive messages |
| 3 | Remediation guardrails (rate limits, blast radius) | Not Started | Per ADR-009 threat model |
| 4 | ARM REST API write operations | Not Started | Scoped RBAC, not broad Contributor |
| 5 | RiskLevel enum + auto-approve policy | Not Started | ReadOnly / LowRisk / HighRisk / Destructive |
| 6 | Identity separation for remediation (ADR-011) | Not Started | Separate MI from `sre-agents-identity` |

#### Phase 2c Exit Criteria

- [ ] RemediationAgent proposes actions with risk levels and rollback plans
- [ ] Approval gate works end-to-end (Slack -> approve -> execute -> verify)
- [ ] Rate limiting prevents runaway remediation
- [ ] Destructive actions require dual approval
- [ ] Platform survives agent crashes gracefully
- [ ] Load test re-validated after remediation pipeline added

### Key Decisions

- NATS JetStream for event bus (enables horizontal scaling + replay)
- RiskLevel enum: ReadOnly / LowRisk / HighRisk / Destructive
- Auto-approve only at confidence >= 0.95 and RiskLevel::LowRisk
- `SreDataSource` trait introduced for multi-provider abstraction
- Approval gate as separate crate (`sre-gates`) — trust boundary separation
- `sre-agents-identity` acceptable for Phase 2a/2b; scoped identity required before Phase 2c (ADR-011)
- Single OIDC federated credential per GitHub environment
- Postgres migration is a hard cut-over: `sqlite` feature dropped from `crates/store/Cargo.toml`, compiler enforces removal of every SQLite call site. No runtime backend selection, no feature-flag hedge
- Postgres auth in Phase 2a uses password-in-ACA-secret; Managed Identity token auth deferred to ADR-011 follow-up
- Store tests run against real Postgres via `testcontainers-rs` (both local and CI) — no SQLite in-memory shortcut

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
| [ADR-008](adr/008-webhook-authentication.md) | Webhook Authentication (HMAC-SHA256) | Accepted (2026-04-21) |
| [ADR-009](adr/009-remediation-safety-model.md) | Remediation Safety Model | Draft (2026-04-22) |
| ADR-010 | NATS JetStream Design | Planned (Phase 2b) |
| ADR-011 | Identity Separation for Remediation | Planned (Phase 2c) |

---

## Known Issues & Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Azure API throttling (200 req/min) | High | Rate limiter (governor crate), query budgets per agent |
| KQL query cost on large workspaces | Medium | Timespan limits, row caps, parameterized templates |
| Agent platform down during incident | Critical | 99.9% availability SLO, graceful degradation, circuit breaking |
| Token/credential management | High | Managed Identity in Azure, config-driven auth mode (mock for dev) |
| Unauthenticated webhook endpoint | Medium | Phase 1: ACA ingress only. Phase 2: ADR-008 (shared secret / Azure AD) |
| Ephemeral audit trail on staging | ~~High~~ Resolved | **Resolved 2026-04-19**: Postgres cut-over shipped (Phase 2a task #3). App now writes to `sre_agents` DB on `psql-sre-agents-staging`; `SRE__STORE__DATABASE_URL` composed from `SRE_AGENTS_DB_PASSWORD` GitHub secret. Audit history survives container restarts. |
