# ADR-020: Multi-Workspace Investigation

## Status

Accepted (2026-06-20). Final increment of the Phase 3 intelligence layer — fan-out
attribution across multiple Log Analytics workspaces and App Insights apps.

## Context

Large Azure estates do not put all telemetry in one place: different teams, regions,
or environments own separate Log Analytics workspaces and Application Insights
applications. The Phase 3 architecture diagram shows the Logs/Metrics/Trace agents
fanning out to **Workspace 1/2 + App Insights 1/2**, and the Phase 3 exit criteria
require "Multi-workspace works across 3+ Log Analytics workspaces."

The platform was already most of the way there: `azure.workspace_ids` and
`azure.app_insights_ids` are `Vec<String>`, the `LogsAgent` loops over every
workspace, and the `MetricsAgent` loops over every app — each already records the
workspace/app id on its `QueryRecord`. What was missing was **attribution at the
finding level**: when the same error spike could come from any of three
workspaces, an operator (or the reasoning engine) couldn't tell *which* workspace
surfaced a given finding, and the agent summary didn't reflect the fan-out.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. A reusable `Finding::with_source_scope` that tags each finding with its workspace/app id as evidence; agents stamp it in the fan-out loop; summaries report scope count** (chosen) | No change to the `Finding` shape (no new field → no ripple across every construction site or the persisted schema). Attribution rides the existing `evidence` mechanism. One small builder, applied at each push site. Tested. | Attribution lives in evidence rather than a first-class column; querying "all findings from workspace X" means scanning evidence. |
| B. Add a `source_scope: Option<String>` field to `Finding` | First-class, queryable. | Ripples through every `Finding { … }` literal in every agent + every test, and changes the serialized report schema (a migration concern). Disproportionate for attribution metadata. |
| C. Leave it at `QueryRecord` provenance (already present) | No code change. | Findings stay unattributed; doesn't meet the "works across 3+" bar in a *verifiable* way; the reasoning engine can't ground on which workspace. |
| D. One agent instance per workspace | Strong isolation. | Multiplies agent tasks and routing for no real gain; the loop already fans out within one agent; complicates the orchestrator. |

## Decision

Adopt **Option A**.

### 1. `Finding::with_source_scope` (sre-common)

A builder that appends the source scope (a Log Analytics workspace id or an App
Insights app id) as an `Evidence` entry labeled `source_scope`, plus a
`source_scopes()` reader. The `Finding` struct, its serialization, and the
persisted report schema are unchanged.

### 2. Agents stamp provenance in the fan-out loop

`LogsAgent` (over `workspace_ids`) and `MetricsAgent` (over `app_insights_ids`)
already iterate every configured scope; each finding pushed inside the loop —
including the query-failure findings — is now tagged `.with_source_scope(id)`.
Their result summaries report the fan-out (`"… across N Log Analytics workspaces"`
/ `"… across N App Insights applications"`).

### 3. Verified

A test drives `LogsAgent` with three workspaces and asserts every workspace is
represented in the findings' `source_scopes()` and that the summary reports the
multi-workspace scope — proving fan-out **and** attribution.

## Consequences

### Positive

- Findings from a multi-workspace investigation are **attributable** to their
  source workspace/app, for both human operators and the reasoning engine's RCA.
- Zero schema churn: `Finding` is unchanged; attribution rides existing evidence.
- The fan-out itself was already there; this closes it into a *verifiable*
  feature with a test and operator-visible summaries.
- Works for any number of workspaces/apps — configuration-driven, no code change
  to add a fourth.

### Negative

- Attribution is evidence-embedded, not a first-class queryable column; a future
  analytics need ("group all findings by workspace") would want Option B.
- Trace/Infra agents are scoped to their configured app/workspace list the same
  way but were not the focus here; extending richer cross-workspace correlation
  (e.g. a single trace spanning two App Insights apps) is a follow-up.
- Per-workspace queries run sequentially within an agent; very large workspace
  lists could parallelize the fan-out (a performance follow-up, not correctness).

### Follow-ups

- Parallelize the per-workspace fan-out (bounded concurrency) for large estates.
- Cross-workspace trace correlation (one OperationId across multiple App Insights
  apps).
- If analytics demand it, promote `source_scope` to a first-class `Finding` field
  with a report-schema migration.

## References

- [ADR-005: KQL Template-Driven Queries](005-kql-template-driven-queries.md) — the
  per-workspace queries are template-driven and resource-sanitized.
- [ADR-015: LLM Reasoning Engine](015-llm-reasoning-engine.md) — attributed
  findings give the RCA the workspace context to reason over.
- `docs/diagrams/phase3-architecture.md` — the multi-workspace fan-out this
  completes (Log Analytics Workspace 1/2, App Insights 1/2).
