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
| Multi-stage Dockerfile | Done | `Dockerfile` — rust:1.99-bookworm builder + debian-slim runtime, non-root user |
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
| 1 | Staging deployment (Terraform + ACA) | Done | Deployed to a staging Azure Container Apps app (`aca-sre-agents-demo`) by a Terraform stack and a release workflow that authenticates to Azure with OIDC workload identity federation. Both are environment-specific and are not published in this repository. |
| 2 | OpenTelemetry instrumentation | Done | 15 spans across critical path, OTLP HTTP exporter, config-driven (`SRE__OBSERVABILITY__ENABLED`) |
| 3 | PostgreSQL migration (replace SQLite) | Done | Hard cut-over shipped 2026-04-19: `sqlite` feature removed, `PgStore` replaces `SqliteStore` across orchestrator/webhook/tests. Integration tests use `testcontainers-modules` (postgres:16-alpine pinned). Smoke test in `docker.yml` runs against a Postgres sidecar on a shared docker network. Staging DB provisioned via `azurerm_postgresql_flexible_server_database.app`; least-privilege app role created out-of-band by a one-time SQL script kept with the (unpublished) staging Terraform. Password auth in ACA secret; MI follow-up tied to ADR-011. **Schema modernization (TEXT→UUID/JSONB/TIMESTAMPTZ, f64 precision fix, composite index) scheduled as a follow-up corrective migration — see ADR-012.** |
| 4 | TraceAgent (distributed traces) | Done | `crates/agents/src/trace.rs` — App Insights `AppRequests` ⋈ `AppDependencies` correlated by `OperationId`. Two KQL templates (`trace_failure_correlation`, `trace_latency_breakdown`): localizes which downstream hop owns end-to-end failures and where latency is actually spent. New `AgentType::Trace` + `AnalysisTaskType::TraceCorrelation`. Triage routes Trace when `operation_ids` are present or on a Metric signal. |
| 5 | InfraAgent (Activity Log, Resource Health) | Done | `crates/agents/src/infra.rs` — Azure Activity Log via the Log Analytics `AzureActivity` table (KQL templates, ADR-005). Two templates (`recent_deployments`, `resource_health`): "what changed?" (administrative writes/deploys/restarts → `DeploymentCorrelation`) and Resource Health transitions (Available→Degraded/Unavailable → new `FindingCategory::ServiceHealth`). New `AgentType::Infra` + `AnalysisTaskType::InfraChanges`. InfraAgent always runs — change correlation is a primary RCA signal. |
| 6 | Webhook authentication (ADR-008) | Done | ADR-008 Accepted 2026-04-21. HMAC-SHA256 middleware (`src/middleware/hmac.rs`) wired on `/api/v1/alerts/azure` only, `{timestamp}.{body}` signed payload with 5-min skew window, constant-time compare via `subtle::ConstantTimeEq`. Config: `WebhookConfig.hmac_secret` (required in staging) + `hmac_skew_seconds` (default 300). 7 integration + 5 unit tests green, clippy `-D warnings` clean. Terraform wired: new `webhook_hmac_secret` var with ≥32-char validation, ACA secret `webhook-hmac-secret`, env `SRE__WEBHOOK__HMAC_SECRET`. The release pipeline supplies the value from its secret store (`SRE_WEBHOOK_HMAC_SECRET`). AAD bearer deferred to Phase 2b. |
| 7 | Remediation safety model (ADR-009 draft) | Draft | ADR-009 in review (2026-04-22) — `docs/adr/009-remediation-safety-model.md`. Threat model (STRIDE + MITRE ATT&CK), RiskLevel enum (ReadOnly/LowRisk/HighRisk/Destructive), auto-approve policy, guardrails, rollback contract, audit schema additions, forward-ref to ADR-011 for identity separation. Implementation is Phase 2c. |
| 8 | Load test baseline | Not Started | Target: 50 concurrent investigations, P99 < 5s |

#### Phase 2a Exit Criteria

- [x] Terraform deploys ACA app to staging via a release workflow (environment-specific, not published here)
- [x] OpenTelemetry traces flow across orchestrator and all agents
- [x] PostgreSQL stores full audit history (replaces SQLite)
- [x] TraceAgent correlates distributed traces and identifies dependency failures
- [x] InfraAgent detects recent deployments/config changes correlated with incidents
- [x] Webhook endpoint authenticated (ADR-008 implemented, 2026-04-21)
- [ ] Load test baseline established (50 concurrent investigations without degradation)

### Phase 2b — Scaling Infrastructure (3-4 weeks)

**Status**: In Progress (started 2026-05-18)

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 1 | NATS JetStream ingestion (ADR-013) — PR-1/2/3 | Done | Parallel ingestion path: webhook stays, NATS adds durable/replayable ingest. Boundary held at `mpsc::Sender<AlertInfo>` — orchestrator and agents untouched. **PR-1 (merged #18):** NatsConfig, `parse_and_enqueue` shared helper, `src/ingest/nats.rs` with backpressure (no-ack-until-enqueued) + `CancellationToken` graceful shutdown, `AlertDedupCache` (FIFO, in-memory), testcontainers smoke test. **PR-2 (done):** OTel metrics `nats_consumer_pending` / `nats_redeliveries_total` / `nats_dlq_total` via a global `SdkMeterProvider` (OTLP, gated by `observability.enabled`), read from per-message JetStream `Info` (zero extra round-trips) — `src/ingest/metrics.rs`. **PR-3 (done):** DLQ stream `sre-alerts-dlq` (subject `alerts.dlq.azure.poison`); poison payloads republished with `Sre-Dlq-Reason`/`Sre-Original-Subject` headers before terminate; integration test asserts poison→DLQ. |
| 2 | Horizontal scaling (multi-replica) | Done | ADR-014. The platform is already multi-replica-safe: stateless replicas, shared NATS durable consumer distributes work, the `workflows.alert_id` UNIQUE constraint is the authoritative cross-replica dedup gate, per-replica drain on SIGTERM (25s within ACA's 30s grace). Infra change: explicit ACA `http_scale_rule` (concurrent_requests=20), `max_replicas` 3→5; prod should set `min_replicas=2`. NATS-pending KEDA scaler (using the ADR-013 PR-2 `nats_consumer_pending` metric) is the documented forward step. |
| 3 | Notification Engine (Slack, Teams, PagerDuty) | Done | New `sre-notify` crate. `Notifier` trait + `MockNotifier` (dev/CI) + real Slack (mrkdwn), Teams (MessageCard), PagerDuty (Events API v2) bindings. `NotificationEngine` decides via a severity threshold + escalation policy and fans out to every configured provider concurrently (best-effort — one broken destination never blocks the others). Wired into the orchestrator dispatch loop; config-gated (`[notifications]`, provider secrets via env). |
| 4 | InboundMessage serialization migration | Deferred | Not needed for ingestion-only NATS. Required only when **agent replies** cross NATS (Phase 3 concern). ADR-013 explicitly keeps `InboundMessage.reply_tx` as `mpsc::Sender` for now. |
| 5 | gRPC API (external integration surface) | Done | tonic 0.14 server (`proto/sre.proto`, `build.rs` via `tonic-prost-build`, `src/grpc.rs`). `InvestigationService`: `TriggerInvestigation` (synchronous — runs the orchestrator, returns the full report) + `GetInvestigation` (store lookup by workflow UUID). Config-gated (`[grpc]`, own port 50051), graceful shutdown via the shared cancel token. CI jobs + Dockerfile install `protoc`; Dockerfile copies `build.rs` + `proto/`. |

#### Phase 2b Exit Criteria

- [ ] NATS enables message replay for debugging
- [x] Platform runs multi-replica behind ACA scaling (ADR-014)
- [x] Notification engine delivers to Slack/Teams/PagerDuty
- [x] gRPC API allows external systems to trigger and query investigations
- [ ] Load test re-validated: 50 concurrent investigations after NATS migration

### Phase 2c — Gated Remediation (4-5 weeks)

**Status**: Functionally complete (2026-06-20) — mock-capable end-to-end (propose → gate → execute → post-mortem verify → rollback → audit). Forward steps: Slack interactive approval UI, the remediation MI Terraform (ADR-011), and the `workflow_state` ENUM migration (ADR-009 §7).

| # | Component | Status | Notes |
|---|-----------|--------|-------|
| 1 | RemediationAgent (proposals, risk levels, rollback plans) | Done (propose-only) | `crates/agents/src/remediation.rs` — playbook maps findings → `RemediationProposal`s: ErrorSpike→RestartWorkload (LowRisk), ResourceExhaustion/LatencyDegradation→ScaleCompute (LowRisk), DeploymentCorrelation→revert revision (HighRisk). Each proposal carries scope_resources, ArmWriteRequest, RollbackStrategy, rationale, evidence. Dedups by category (highest confidence wins). No execution — proposes only; the gate + executor (next PR) decide and run. `AgentType::Remediation` + `AgentPayload::RemediationRequest`/`RemediationProposed`. |
| 2 | Approval Gate — separate `sre-gates` crate | Done | New `sre-gates` crate (pure policy, `sre-common` only). `ApprovalGate::evaluate` implements the ADR-009 §3 auto-approve policy: ReadOnly→auto; LowRisk→auto iff confidence ≥ 0.95 & blast-radius ≤ 1 & guardrails OK, else 1 approval; HighRisk→1 approval; Destructive→2 **distinct** approvals. Re-derives risk via `enforced_risk()` so a mis-tagged destructive action can't sneak through (tested). `approve()` tallies distinct approvers (dual-approval); `acquire_locks()`/`release()` for the executor. Slack-interactive approval UI is the forward step. |
| 3 | Remediation guardrails (rate limits, blast radius) | Done | ADR-009 §4: global kill switch, per-subscription + per-resource-type hourly rate limits (sliding window, **injected clock** for deterministic tests), blast-radius cap (1 for auto), per-resource mutual-exclusion locks, optional business-hours window. Auto-approve **atomically reserves** rate budget + locks to avoid TOCTOU races. 16 unit tests cover the full decision matrix. |
| 4 | ARM REST API write operations | Done | `sre-azure::arm_write`: `ArmWriteClient` trait + `MockArmClient` (records calls; Success/OverScope/Failure modes) + `AzureArmClient` (real ARM REST via `TokenProvider`, management scope). `RemediationExecutor` (orchestrator) runs gate → execute → **post-mortem blast-radius check** (affected ⊆ declared scope, else rollback + escalate) → insert-only audit. Wired into the dispatch loop, opt-in via `[remediation]`. |
| 5 | RiskLevel enum + auto-approve policy | In Progress | `sre-common::remediation` (ADR-009 §1-2): `RiskLevel` (ReadOnly/LowRisk/HighRisk/Destructive, ordered), `RemediationCategory` (with `is_always_destructive`/`is_blocklisted`), `RollbackStrategy`, `ArmWriteRequest`, `ResourceId`, `RemediationProposal` with `enforced_risk()` (always-destructive categories + no-rollback ⇒ Destructive, non-overridable). The auto-approve **policy** that consumes these lands in `sre-gates` (next PR). |
| 6 | Identity separation for remediation (ADR-011) | Done (paper + seam) | ADR-011 Accepted. `AzureArmClient` takes its **own** `TokenProvider`, independent of the read client — a one-line wiring change to a separate remediation MI once provisioned. The Terraform MI + scoped per-resource-type RBAC roles are the documented infra forward step; remediation stays disabled by default until then. |

#### Phase 2c Exit Criteria

- [ ] RemediationAgent proposes actions with risk levels and rollback plans
- [x] Approval gate works end-to-end (auto-approve path: gate -> execute -> post-mortem verify -> rollback-on-overscope; Slack human-approval UI is the forward step)
- [x] Rate limiting prevents runaway remediation (sre-gates guardrails)
- [x] Destructive actions require dual approval (sre-gates, enforced via `enforced_risk()`)
- [ ] Platform survives agent crashes gracefully
- [ ] Load test re-validated after remediation pipeline added

### Key Decisions

- NATS JetStream for event bus (enables horizontal scaling + replay)
- RiskLevel enum: ReadOnly / LowRisk / HighRisk / Destructive
- Auto-approve only at confidence >= 0.95 and RiskLevel::LowRisk
- `SreDataSource` trait introduced for multi-provider abstraction
- Approval gate as separate crate (`sre-gates`) — trust boundary separation
- A single read-scoped managed identity (`sre-agents-identity`) is acceptable for Phase 2a/2b; a separate scoped identity is required before Phase 2c (ADR-011)
- Deployments authenticate to Azure with OIDC workload identity federation, one federated credential per deployment environment, so CI holds no long-lived Azure secret
- Postgres migration is a hard cut-over: `sqlite` feature dropped from `crates/store/Cargo.toml`, compiler enforces removal of every SQLite call site. No runtime backend selection, no feature-flag hedge
- Postgres auth in Phase 2a uses password-in-ACA-secret; Managed Identity token auth deferred to ADR-011 follow-up
- Store tests run against real Postgres via `testcontainers-rs` (both local and CI) — no SQLite in-memory shortcut
- InfraAgent reads the Azure Activity Log through the Log Analytics `AzureActivity` table (KQL templates, ADR-005) rather than the ARM Activity Log REST API — keeps the read path uniform, template-safe, and reusing the existing `AzureQueryClient`
- Investigation agent roster is now Triage + Logs + Metrics + Trace + Infra; the orchestrator's `has_agent_type` skip means a recommended agent that isn't registered is silently dropped, so triage can recommend liberally without breaking minimal deployments

---

## Phase 3 — Intelligent Autonomy: LLM Reasoning & Learning

**Status**: Code-complete (2026-06-20) — all eight components implemented, compiling, and tested (mock-capable end-to-end). The remaining exit criteria are **evaluation-gated** (accuracy/relevance thresholds, a 30-day false-positive window, adoption by another team) and require real-incident data over time, not more code.
**Duration**: 2026-06-20 (implementation)
**Goal**: Reduce MTTR through intelligent correlation and trusted automated response

### Scope

| Component | Status | Notes |
|-----------|--------|-------|
| ReasoningEngine (LLM-powered RCA synthesis) | Done | New `sre-intelligence` crate (ADR-015). `ReasoningEngine` trait + `MockReasoningEngine` (deterministic, rule-based; CI/dev) + `ClaudeReasoningEngine` (real Anthropic Messages API via raw HTTP — no Rust SDK — default `claude-opus-4-8`). Selected by config like `auth_mode`: Claude when `ANTHROPIC_API_KEY` set, else mock. Orchestrator enriches the report summary with the synthesized RCA (root cause, confidence, factors, actions, narrative) — best-effort, opt-in (`[reasoning]`), never blocks an investigation. `related_incidents` seam ready for RAG. |
| KnowledgeStore (Qdrant vector DB, past incidents) | Done | `sre-intelligence` (ADR-016). `KnowledgeStore` trait + `MockKnowledgeStore` (in-memory cosine, deterministic; CI/dev default) + `QdrantKnowledgeStore` (raw-HTTP REST binding; pure body builders + response parser unit-tested). `Embedder` trait + `HashEmbedder` (FNV-1a feature-hashing, offline lexical embedding; semantic embedder is a drop-in). Selected by config like `auth_mode`. **Fills the ADR-015 `related_incidents` RAG seam**: orchestrator retrieves `top_k` similar past incidents before reasoning and feeds their grounding lines to the engine. Best-effort, opt-in (`[knowledge]`). |
| LearningLoop (outcome tracking, confidence adjustment) | Done | `sre-intelligence::learning` (ADR-016). `OutcomeSignals::classify()` implements the diagram's decision tree as a pure fn (no-remediation→Inconclusive; resolved-in-window→Success; refired-in-hour→Failure; else Success). `LearningLoop::record_outcome` stamps the outcome, updates a per-category `ConfidenceTracker` (the success-rate signal the progressive-trust model will consume), and embeds the incident into the knowledge store. Clock-free (callers stamp timestamps) for deterministic tests. Wired into `investigate()` (read-only path → Inconclusive); real post-remediation outcomes land with auto-remediation. |
| AutoRemediationEngine (pre-approved playbooks) | Done | ADR-017. The Phase 2c executor gains an optional `TrustModel`: trusted remediation *categories* (the "pre-approved playbooks") auto-execute below the fixed 0.95 floor. Auto-remediation never bypasses the gate — it only lowers the LowRisk confidence bar (within `[0.85, 0.95]`); risk routing, kill switch, blast-radius, rate limits, locks, and the HighRisk/Destructive human requirement are unchanged. Opt-in via `remediation.progressive_trust`. |
| Progressive trust model | Done | `sre-gates::trust::TrustModel` (ADR-017). Per-category floor ratchets 0.95→0.93→0.90→0.87→0.85 at 10/25/50/100 consecutive successes; one failure resets + cools down (15/30/60 min by tier), suspending auto-remediation (gate routes to human approval) until it expires. Pure policy, injected clock (deterministic). The executor feeds each execution outcome back (`Executed`→success; over-scope `RolledBack`/ARM `Failed`→failure). 9 unit tests + 4 executor-integration tests. |
| Agent SDK (published crate for custom agents) | Done | ADR-019. New lightweight `sre-agent-sdk` crate (sre-common only): owns the `SreAgent` trait + `AgentHandle` (moved from `sre-agents`, which now re-exports them — zero churn), a `prelude` (trait + message/finding types + `async_trait`/`mpsc` in one glob), crate docs of the contract, a README guide, and a **runnable** `examples/custom_agent.rs` (compile-checked in CI, `cargo run --example custom_agent`). New `AgentType::Custom(String)` gives third-party agents an identity. 2 doctests + the example; the 19 first-party agent tests pass unchanged. |
| Multi-workspace investigation | Done | ADR-020. The Logs/Metrics agents already fan out over `azure.workspace_ids` / `app_insights_ids`; this closes it into a verifiable feature: reusable `Finding::with_source_scope` tags each finding with its source workspace/app id (via existing evidence — no `Finding` schema change), agents stamp it in the fan-out loop (incl. failure findings), and summaries report the scope count ("… across N Log Analytics workspaces"). A fan-out test drives 3 workspaces and asserts all 3 are attributed in the findings' `source_scopes()`. |
| ChatOps interface (Slack/Teams conversational) | Done | ADR-018. New transport-agnostic `sre-chatops` crate: pure `parse_command` grammar (`investigate <resource> [severity]`, `status <id>`, `help`; verb/severity aliases) + `ChatOpsHandler` over a `ChatBackend` trait → Slack-style mrkdwn replies. Webhook server binds it: `POST /api/v1/chatops` (Slack slash command), `WebhookChatBackend` triggers via the same alert channel as the webhook and reads status from the store. New `WorkflowStore::get_workflow_by_alert_id` lets the ChatOps tracking ref round-trip `investigate`→`status`. Config-gated (`[chatops]`, default off); Slack request-signing is the documented forward step. 13 crate unit tests + 1 store test. |

### Exit Criteria

**Implemented + verified in tests:**

- [x] Multi-workspace works across 3+ Log Analytics workspaces (ADR-020; fan-out test over 3 workspaces with per-finding attribution)
- [x] Progressive trust model correctly adjusts thresholds (ADR-017; 9 unit + 4 executor-integration tests across the 0.95→0.85 tiers + cooldown)
- [x] Agent SDK documented + example custom agent built (ADR-019; `sre-agent-sdk` crate, README, runnable `custom_agent` example) — *adoption by another team is the only remaining, external, part*
- [x] ChatOps handles investigation queries (ADR-018; `investigate`/`status` slash-command grammar, 13 tests) — *free-form natural-language parsing is a noted follow-up*

**Implemented, but evaluation-gated** (need real-incident data / time, not more code):

- [ ] LLM reasoning correctly synthesizes RCA in >80% of test incidents — *engine implemented (ADR-015); accuracy needs a labeled incident corpus*
- [ ] Knowledge store retrieves relevant past incidents with >85% relevance — *retrieval implemented (ADR-016); relevance needs a graded eval set*
- [ ] Auto-remediation: zero false positives over 30-day evaluation — *pipeline + trust gating implemented (ADR-009/017); the 30-day window is by definition future-dated*
- [ ] Learning loop improves confidence accuracy over time — *outcome tracking + `ConfidenceTracker` implemented (ADR-016); "over time" needs longitudinal data*

---

## Test Coverage

**Status**: 80% target met (2026-06-20) — **85.24% workspace line coverage** (measured by `cargo llvm-cov`, all files included); re-measured 2026-10-12 at **82.89%** (the README badge).

### Measuring

```bash
cargo install cargo-llvm-cov --locked          # one-time
cargo llvm-cov --workspace --summary-only       # full table
# Docker unavailable? The Postgres/NATS testcontainers tests fail at runtime;
# add --ignore-run-fail so the report is still emitted from the tests that ran.
cargo llvm-cov --workspace --ignore-run-fail --summary-only
```

### Coverage by area (line %)

| Area | Before | After | How |
|------|--------|-------|-----|
| `agents/{metrics,logs,trace,infra}` | 13–23% | 98–99% | In-file `#[cfg(test)]` over the pure `analyze_*` functions (JSON records → `Finding`) |
| `orchestrator/engine` | 18% | 64% | `tests/engine_inmemory_tests.rs` drives full `investigate()` against an in-memory `WorkflowStore`/`EventLog` — **no Docker** |
| `orchestrator/{workflow,router,aggregator}` | 43–74% | 94–99% | State-machine + routing + dedup unit tests |
| `intelligence/reasoning` | 0% | 100% | `build_engine` provider selection (mock/claude/unknown/disabled) |
| `azure/{auth,alerts_client}` | 36–57% | 88–94% | Token providers + Common Alert Schema parsing |
| `notify/engine` | 73% | 97% | `from_config` provider wiring via the `MockNotifier` |
| `ingest/nats` | 0% | 78% | State enum, `build_options`, cancel-path backoff (server-independent logic) |

### Not unit-testable in this environment

- `src/main.rs` (0%) — CLI bootstrap / clap wiring (binary entrypoint).
- `store/postgres.rs`, the bulk of `ingest/nats.rs` `run()/handle_message()`, `webhook.rs` handler, `crates/*/tests/{postgres_tests,investigation_tests,nats_ingest,webhook_tests}.rs` — require a live Postgres/NATS via `testcontainers` (Docker). These pass in CI where Docker is present; they are skipped locally without it.

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
| [ADR-009](adr/009-remediation-safety-model.md) | Remediation Safety Model | Accepted (2026-06-20) |
| ADR-010 | NATS JetStream Design | Superseded by ADR-013 |
| [ADR-011](adr/011-identity-separation-remediation.md) | Identity Separation for Remediation | Accepted (2026-06-20) |
| [ADR-012](adr/012-schema-type-modernization.md) | Schema Type Modernization | Accepted (2026-04-19) |
| [ADR-013](adr/013-nats-jetstream-ingest.md) | NATS JetStream as Parallel Ingestion Path | Accepted (2026-05-18; PR-2/3 2026-06-20) |
| [ADR-014](adr/014-horizontal-scaling-model.md) | Horizontal Scaling Model (Multi-Replica) | Accepted (2026-06-20) |
| [ADR-015](adr/015-llm-reasoning-engine.md) | LLM Reasoning Engine | Accepted (2026-06-20) |
| [ADR-016](adr/016-knowledge-store-learning-loop.md) | Knowledge Store + Learning Loop | Accepted (2026-06-20) |
| [ADR-017](adr/017-progressive-trust-auto-remediation.md) | Progressive Trust + Auto-Remediation | Accepted (2026-06-20) |
| [ADR-018](adr/018-chatops-interface.md) | ChatOps Interface | Accepted (2026-06-20) |
| [ADR-019](adr/019-agent-sdk.md) | Agent SDK | Accepted (2026-06-20) |
| [ADR-020](adr/020-multi-workspace-investigation.md) | Multi-Workspace Investigation | Accepted (2026-06-20) |

---

## Known Issues & Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Azure API throttling (200 req/min) | High | Rate limiter (governor crate), query budgets per agent |
| KQL query cost on large workspaces | Medium | Timespan limits, row caps, parameterized templates |
| Agent platform down during incident | Critical | 99.9% availability SLO, graceful degradation, circuit breaking |
| Token/credential management | High | Managed Identity in Azure, config-driven auth mode (mock for dev) |
| Unauthenticated webhook endpoint | Medium | Phase 1: ACA ingress only. Phase 2: ADR-008 (shared secret / Azure AD) |
| Ephemeral audit trail on staging | ~~High~~ Resolved | **Resolved 2026-04-19**: Postgres cut-over shipped (Phase 2a task #3). App now writes to a dedicated `sre_agents` database on the staging PostgreSQL Flexible Server; `SRE__STORE__DATABASE_URL` is composed at deploy time from a pipeline secret. Audit history survives container restarts. |

---

## Production-Readiness Hardening (2026-07-05)

**Trigger**: pre-go-live audit — seven specialist review passes (AppSec, security
architecture, SRE reliability, CI/CD & IaC, code correctness, database, threat
model). Verdict: **secure- and safe-by-default** (all high-risk subsystems default
off; KQL is template-sanitized per ADR-005; SQL is fully parameterized), but
several silent-degradation and reliability defects required fixing first. Branch:
`harden/prod-readiness`. See [ADR-022](adr/022-production-hardening-guards.md).

### Fixed

| # | Severity | Issue | Fix |
|---|----------|-------|-----|
| 1 | Blocker | Agents `break` out of `run()` on a closed per-investigation reply channel → one slow (>30s) Azure query permanently kills the agent; if TriageAgent dies every investigation fails. | `break`→`continue` in all 6 agents. |
| 2 | Blocker | Graceful drain listened for SIGINT only; ACA sends SIGTERM → drain was dead code, every deploy/scale-in dropped in-flight + queued work. | `shutdown_signal()` selects over Ctrl-C and SIGTERM. |
| 3 | Blocker | Failed-query findings had `confidence: 1.0` and `Failed` investigations weren't escalated → an Azure outage reported "Resolved" at full confidence and paged no one. | Failure findings `confidence: 0.0`; `Failed` state now escalates + pages with a failure reason. |
| 4 | Blocker | Webhook fails open (unauthenticated) when `hmac_secret` unset; mock auth / remediation enablable in prod with no guard. | Fail-closed startup guards (ADR-022) for HMAC secret, `auth_mode=mock`, and remediation-vs-real-Azure. |
| 5 | High | HMAC middleware buffered the body with `usize::MAX` before auth → pre-auth memory-exhaustion DoS on the public endpoint. | Cap `to_bytes` at 64 KiB. |
| 6 | High | Liveness heartbeat was a free-running timer → a dead/wedged dispatch loop stayed "healthy" forever. | Heartbeat owned by the dispatch loop (alert + interval tick); a dead loop goes stale → ACA restarts. |
| 7 | High | Notifier HTTP clients had no timeout and run while holding the investigation permit (semaphore wedge); Slack/Teams secret leaked into logs via `reqwest::Error` URL. | 10s timeout on all 3 notifiers; `without_url()` strips the secret-bearing URL. |
| 8 | High | DB pool: 5 conns (< max_concurrent 10), no statement/idle/acquire timeouts → a failover-stuck query wedged all slots incl. the readiness ping. | Pool 16 conns + `statement_timeout`/`idle_in_transaction`/`acquire_timeout`/`max_lifetime`. |
| 9 | High | CI/CD (release workflow, not published here): `${{ github.event.release.tag_name }}` interpolated into `run:` blocks holding Azure OIDC creds (shell injection); no `permissions:` blocks; TF secrets on argv. | Untrusted inputs via env + charset validation; `permissions: contents: read`; secrets via `TF_VAR_*`; Dependabot added. |
| 10 | Medium | NaN/out-of-range confidence bypassed the auto-approve floor and trust cooldown (`NaN < x` is false). | Gate rejects non-finite / out-of-[0,1] confidence → routes to human. |
| 11 | Medium | Azure "Resolved" notifications ran full investigations; stuck-state + report-write failures swallowed silently. | Ignore `monitorCondition=Resolved`; persist `Failed` on the unexpected-payload path; log terminal-state + report-write failures. |

Verification: `cargo build` / `clippy -D warnings` / `fmt --check` clean; ~263 unit
tests + Docker-free integration suites pass (one test updated for the corrected
`Failed ⇒ escalated` behavior). Docker-gated Postgres/NATS testcontainer suites
not runnable in this environment.

### Go-live gate — REQUIRED before enabling (not fixed here; larger efforts)

- **Real Azure auth** — `crates/azure/src/auth.rs` `AzureCredentialProvider` is a
  placeholder reading a static `AZURE_ACCESS_TOKEN`; wire `DefaultAzureCredential`
  (Managed Identity) with token refresh. **Hard blocker for any real go-live.**
- **Remediation** — requires ADR-011 dedicated identity + ADR-021 durable
  coordination. Guarded off; do not enable.
- **ChatOps / gRPC** — unauthenticated on a public port; add Slack signature
  verification / gRPC auth + semaphore routing before enabling.
- **Infra** — no prod stack (staging-only), `min_replicas = 1` (no HA), shared
  `sre-agents-identity`, secrets in TF state (→ Key Vault); prefer IP-restricted /
  internal ingress; add image scanning + SHA-pinned actions.
- **Data** — `event_log`/`workflows` grow unbounded (add retention); redact Azure
  exception text before external LLM egress if reasoning is enabled.

---

## Dependency & CI Maintenance (2026-08-31)

Cleared the six accumulated Dependabot PRs (#54–#59) and fixed two CI failures
that had main red. Landed as [#60](https://github.com/dimon2255/sre-ai-agents/pull/60)
(consolidation) and [#61](https://github.com/dimon2255/sre-ai-agents/pull/61)
(grouping policy).

### Fixed

| # | Severity | Issue | Fix |
|---|----------|-------|-----|
| 1 | High | `Security Audit` red on main since 2026-08-24 on a **real** advisory: RUSTSEC-2026-0258, `h2` 0.4.15 "unbounded empty DATA frames". The workflow was reporting correctly — the finding was genuine. | Lockfile refresh → `h2` 0.4.19. Also cleared the yanked `chacha20` 0.10.1 → 0.10.2. `cargo audit` now exits 0 with no allowed warnings. |
| 2 | High | `CI / Test` intermittently red since 2026-08-03 (main, and PRs #56/#58/#59). `ContainerAsync::get_host_port_ipv4` inspects Docker live and returns `PortNotExposed` when the host port binding has not been published yet; `start()` waits only for the image readiness condition, not port publication. The two race. | All six call sites poll for a bounded 5s window. Reproduced pre-fix (12/13 passed, 1 failed on identical code); 13/13 across three consecutive runs after. |
| 3 | Medium | Six Dependabot PRs serialized against a shared `Cargo.lock`, each carrying a *partial* resolution. Two regressed a transitive dep: #55 downgraded `windows-sys` 0.61.2 → 0.52.0, #59 downgraded `getrandom` 0.4.3 → 0.3.4. | Consolidated into one `cargo update`, then grouped Dependabot so it consolidates itself: one PR per ecosystem per week. |

### Dependency policy (`.github/dependabot.yml`)

Cargo **minor/patch** bumps batch into one PR — semver-compatible, so the group
resolves as a single lockfile update. **Major** bumps stay individual: they break
APIs and need per-crate review a batched PR would hide.

Versions landed: `async-nats` 0.48→0.50 (manifest edit), `clap` 4.6.6,
`thiserror` 2.0.20, `async-trait` 0.1.92, `uuid` 1.26.0, Docker base `rust` 1.98.

### Known constraints

- **`reqwest` moved to 0.13 on 2026-10-12** once the OpenTelemetry crates reached 0.33
  and the graph carried only one `reqwest`. 0.13 defaults to the `aws-lc-rs` crypto
  provider (a cmake + NASM build), so it is compiled with `rustls-no-provider` and
  `sre_common::http::client_builder` installs `ring` before any client is built;
  `reqwest::Client::builder()` must not be called directly.
- **Dependabot's cargo sweep** failed 2026-08-17 and 2026-08-24, and did not run
  2026-08-31. Logs have expired (HTTP 410), so the cause is **unconfirmed**. A
  forced run on 2026-08-31 succeeded and the lockfile is now fully refreshed.
  Watch the next scheduled sweep.
- `cargo clippy --workspace --all-targets` reports 3 pre-existing
  `field_reassign_with_default` lints in `sre-notify` test code. CI does not pass
  `--all-targets`, so this is not gating.

### Test suite status

The Docker-gated Postgres/NATS testcontainer suites **do** run given a working
Docker daemon, superseding the "not runnable in this environment" note under
Production-Readiness Hardening (2026-07-05). Full suite on this branch:
**316 passed, 0 failed** (`cargo test --workspace`), with `fmt --check` and
`clippy --workspace -D warnings` clean.

---

## Golden-Set Evaluation (2026-10-11)

**Status**: Harness, golden set and CI gate complete. LLM tier run three times against
the real API on 2026-10-11 (Max-plan promotional API credits). Branch: `features/golden-set-evals`. See
[ADR-023](adr/023-golden-set-evaluation.md), the
[harness diagram](diagrams/eval-harness.md) and the
[baseline report](evals/golden-set-report.md).

**Goal**: measure whether an investigation reaches the right answer (root cause,
escalation, remediation), not only whether each component behaves as coded.

| Item | Status | Notes |
|------|--------|-------|
| `crates/evals` harness (fixture KQL server, real orchestrator + 5 agents) | Done | Only the Azure query endpoint is replaced. Template routing guarded by a unit test. |
| Scoring + scorecard (Markdown + JSON) | Done | Order-independent: top-tier ties scored as misses. Identical across 3 runs. |
| 14-scenario golden set | Done | Hand-authored fixtures, one labeler. Contrastive pairs. |
| CI ratchet gate (`baseline.json`) | Done | Verified to fail on an injected regression. Runs under `cargo test --workspace`. |
| Opt-in LLM-judge tier (`--engine claude`) | Done (3 runs) | Real API, 84 calls, 0 failures. Claude RCA judged correct 10 of 14 (71%) in every run, same 10 each time. `--summarize-runs` combines runs. |

### Baseline (rule-based pipeline)

Root cause top-1 4 of 14 (29%); root cause found at any rank 9 of 12 (75%);
detection precision 22 of 24 (92%), recall 22 of 25 (88%); escalation 13 of 14
(1 missed page); first-choice remediation 7 of 14 (50%), 4 unwarranted actions.

LLM tier (three runs, `claude-opus-4-8` RCA judged by `claude-opus-5-5`): 10 of
14 (71%) judged correct in each run, pooled 30 of 42. It fixed 7 of the 10 rule-based root-cause misses and got
one rule-based pass wrong (GS-12). Remediation and paging are unaffected because
they still come from the rule-based findings.

### Defects found by the evaluation (not fixed in this change)

| # | Severity | Issue | Scenarios |
|---|----------|-------|-----------|
| 1 | High | Findings ranked by severity alone, so symptoms (error spike, latency) outrank causes (deploy, dependency, platform). | GS-01, 02, 03, 05, 08, 10 |
| 2 | High | Traffic collapse with healthy telemetry resolves without a page: no volume check, and a Low finding lifts confidence over the 0.5 threshold. | GS-14 |
| 3 | High | Rollback filed under `RestartWorkload`; `propose()` keeps one per category, so a restart (0.90) silently discards the rollback (0.80). | GS-01, 08 |
| 4 | Medium | `resource_utilization` KQL template is never queried; resource exhaustion is never detected. | GS-04, 09 |
| 5 | Medium | Playbook proposes restart/scale for causes they cannot fix (dependency, platform, bad input, slow partner). | GS-02, 03, 05, 11 |
| 6 | Medium | Failure breakdown grades by absolute count, not rate: 6 of 3,100 failures becomes a Medium error spike. | GS-05, 06 |
| 7 | Low | Reasoning engine receives the alert id as trigger, not the alert rule or description. | GS-14 (LLM tier) |

### Decisions

- Labels describe the incident, not the code; written before the first run.
  Equivalent categories within one causal family use explicit `a|b` labels.
- A finding counts as a detection only at Medium severity or above, so status
  readouts (the Low latency summary emitted on every run) are not claims.
- The LLM RCA defaults to the platform's configured `reasoning.model`
  (`claude-opus-4-8`) so the eval measures what production would run; the judge
  defaults to `claude-opus-5-5` with structured output and server-side
  refusal fallback.

### Verification

- `cargo test --workspace`: 330 passed, 0 failed (Docker suites included).
  `cargo test -p sre-evals`: 16 unit tests plus the ratchet gate.
- `fmt --check` and `clippy --workspace -D warnings` clean; `clippy -p sre-evals
  --all-targets -D warnings` clean.
- Fresh-context review: APPROVE. Its suggestions were applied before commit: runs
  where a tasked agent never replied are now rejected instead of scored; the
  per-category table credits the category actually emitted; LLM-tier API errors
  are excluded from the RCA denominator and counted separately; `--only` no longer
  reports skipped scenarios as regressions; fixture markers no longer use bare
  table names, and a test fails if a KQL template is added without a fixture key.

### Known issues

- Fixtures are hand-authored, not captured from production.
- Labels have one author; needs a second-reviewer pass.
- Fourteen scenarios is too small for a held-out split.

---

## Dependency & CI Maintenance (2026-10-11)

`Security Audit` was red on main from 2026-09-21 and failed on every PR,
including the open Dependabot PRs (#64 cargo, replaced by #67 during this work; #65 Docker). Branch:
`bugfixes/rustls-advisory-dependabot-consolidation`.

### Fixed

| # | Severity | Issue | Fix |
|---|----------|-------|-----|
| 1 | High | A **real** advisory: RUSTSEC-2026-0285, `rustls` 0.23.43 "TLS 1.3 handshake messages incorrectly accepted across encryption level boundaries" (published 2026-09-14). The workflow was reporting correctly. | One `cargo update` lifts `rustls` to 0.23.45. `cargo audit` with CI's flags fails on main's lockfile and exits 0 on this branch, with no warnings. |
| 2 | Low | Two Dependabot PRs open against a shared `Cargo.lock` / `Dockerfile`. | Consolidated: the same `cargo update` carries all five bumps from #67 (`tokio`, `uuid`, `thiserror`, `config`, `clap`; `uuid` one minor newer), which superseded #64; the Dockerfile takes #65's `rust` 1.98 to 1.99 builder image unchanged. |

Same pattern as the 2026-08-31 consolidation (#60): one lockfile resolution,
the superseded Dependabot PRs closed with a pointer once this merges.
