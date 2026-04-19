# ADR-012: Schema Type Modernization (Two-Phase Postgres Migration)

## Status

Accepted (2026-04-19). Schema follow-up scheduled for next sprint.

## Context

Phase 2a replaced the SQLite `WorkflowStore` implementation with a Postgres-backed `PgStore` (see PROGRESS.md Phase 2a task #3). The goal was a hard cut-over: remove the `sqlite` feature from `crates/store/Cargo.toml`, let the compiler enforce the removal of every SQLite call site, and avoid a dual-backend abstraction.

The initial migration (`crates/store/migrations/20260418000001_init.sql`) is functionally correct but is a **straight port of the SQLite schema** — every column is `TEXT` or `REAL`, which works against Postgres but leaves a number of Postgres-native benefits on the table and introduces one concrete correctness risk.

### Issues Identified During Review

| # | Column(s) | Current | Issue |
|---|-----------|---------|-------|
| 1 | `event_log.confidence` | `REAL NOT NULL` | 🚨 **Precision bug.** Postgres `REAL` is 4-byte float. Binding an `f64` from `AgentMessage::confidence` silently truncates to `f32` on INSERT. Should be `DOUBLE PRECISION`. Impact is bounded because confidence values live in `[0.0, 1.0]`, but it is still a correctness regression vs. SQLite (which held the full f64). |
| 2 | All UUID columns (`workflows.id`, `event_log.id`, `event_log.correlation_id`, `event_log.parent_message_id`) | `TEXT` | No type enforcement at the DB level — any string can be inserted. ~2.25× storage overhead vs. native `UUID` (36 bytes of hex string + length prefix vs. 16 bytes). Loses `gen_random_uuid()` as a server-side default. |
| 3 | All timestamp columns (`workflows.created_at`, `workflows.updated_at`, `event_log.timestamp`) | `TEXT` (RFC3339) | Sort order depends on every writer normalizing the offset to `+00:00`. Cannot use `TIMESTAMPTZ` operators (`EXTRACT`, `AT TIME ZONE`, range queries). |
| 4 | JSON blobs (`workflows.report_json`, `event_log.payload_json`) | `TEXT` | No structural indexing (GIN), no containment queries (`@>`), no validation that the column contains valid JSON. Swapping to `JSONB` is near-zero effort with high payoff. |
| 5 | Enum columns (`workflows.state`, `workflows.severity`) | `TEXT` holding JSON-encoded strings | Code serializes the enum via `serde_json::to_string(&variant)` before binding, so the column contains quoted strings (`"resolved"`, not `resolved`). The filter in `get_active_workflows` — `WHERE state NOT IN ('"resolved"', '"escalated"', '"failed"')` — is correct but fragile: any change to the serde representation silently breaks it. |
| 6 | `event_log(correlation_id)` index | Single-column | `get_by_correlation_id` runs `WHERE correlation_id = $1 ORDER BY timestamp ASC`. A composite `(correlation_id, timestamp)` serves the query with an index-ordered scan instead of index-scan-plus-sort. |

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Two-phase migration** — ship the SQLite-shaped schema in Phase 2a, then land a corrective `ALTER` migration in a follow-up PR | Small blast radius for the cut-over PR; exercises the normal migration-on-migration workflow; corrective migration is trivial against the fresh staging DB (no prod data yet); builds the muscle for schema evolution that will be needed forever after | Two PRs instead of one; window of time on staging with sub-optimal schema |
| B. Rewrite the init migration in place | Single clean initial schema; no leftover SQLite-port artefacts | Any branch/CI run that already executed the current init will have `_sqlx_migrations` recording a version-hash that no longer matches, causing sqlx to refuse to start on those branches. Creates a footgun every contributor can trip on. |
| C. Squash the cut-over and schema modernization into a single PR | Cleanest end state | Inflates the cut-over PR with ~6 additional schema concerns + a new round of DB review + retest. Violates "small, reversible changes" principle. |

## Decision

**Option A.** Phase 2a ships the current migration (`20260418000001_init.sql`) unchanged. The corrective migration (`20260420000001_fix_schema_types.sql`) lands in an immediate follow-up PR and applies the six changes above via `ALTER TABLE` on the (still fresh) staging database.

### Follow-Up Migration Scope

The follow-up migration (`20260420000001_fix_schema_types.sql`) will:

1. `ALTER TABLE event_log ALTER COLUMN confidence TYPE DOUBLE PRECISION` — fixes the precision regression.
2. `ALTER TABLE workflows ALTER COLUMN id TYPE UUID USING id::uuid` and same for `event_log.id`, `correlation_id`, `parent_message_id`.
3. `ALTER TABLE workflows ALTER COLUMN created_at TYPE TIMESTAMPTZ USING created_at::timestamptz` and same for `updated_at` and `event_log.timestamp`.
4. `ALTER TABLE workflows ALTER COLUMN report_json TYPE JSONB USING report_json::jsonb` and same for `event_log.payload_json`.
5. Either (a) stop JSON-encoding the enum before binding and store plain strings (`resolved`, `escalated`, `failed`), or (b) introduce a Postgres `ENUM` type per enum (`CREATE TYPE workflow_state AS ENUM (...)`). Decision between (a) and (b) deferred to the follow-up PR; (a) is lower-ceremony and preferred unless we need the constraint.
6. `CREATE INDEX idx_event_log_correlation_timestamp ON event_log (correlation_id, timestamp)` and `DROP INDEX idx_event_log_correlation`.

Rust code changes in the follow-up PR:

- `PgStore` row-mapping types (`WorkflowRow`, `EventRow`) switch from `String` fields to `Uuid`, `DateTime<Utc>`, `serde_json::Value`.
- The `get_active_workflows` filter drops the quoted-JSON literals.
- Bind-site `serde_json::to_string(&enum_value)` calls swap for `.to_string()` (with a `Display` impl or `#[serde(rename_all = "lowercase")]` on the enum).

### Out of Scope for Both PRs

- Foreign key from `event_log.parent_message_id` → `event_log.id`. Out-of-order inserts and the lack of a meaningful cascade rule make the FK more hassle than value. Document the invariant, skip the FK.
- `BEFORE UPDATE` trigger on `updated_at`. Application code already sets the column on every update; a trigger would shadow that logic and complicate testing.
- `CHECK` constraints on severity. Handled by the Rust enum at the type system level; redundant at the DB.

## Consequences

### What becomes easier

- **The cut-over PR stays reviewable.** Reviewers can focus on the SQLite→Postgres boundary change (driver, migrations plumbing, testcontainers, Terraform, CI) without also adjudicating six schema concerns.
- **Correct types after the follow-up.** `UUID`, `TIMESTAMPTZ`, `JSONB`, `DOUBLE PRECISION` all enforce shape at the DB, not just in Rust.
- **JSONB unlocks query-inside-blob workflows** for future observability/debugging tools (e.g., "give me every investigation where a finding of category `ErrorSpike` appeared").
- **Composite index removes the sort step** from `get_by_correlation_id`, relevant as event volume per correlation grows.
- **Fixes the silent f64→f32 truncation** in `confidence`.

### What becomes harder

- **Schema migrations from now on are ALTERs, not edits to the init file.** This is the correct long-term posture but imposes discipline: no one can edit `20260418000001_init.sql` retroactively.
- **The staging DB has a short window (hours to days, between the two PRs) running on the suboptimal schema.** Acceptable because staging has no meaningful data to protect and no external consumers.
- **Row-mapping code in `PgStore` changes shape** in the follow-up PR. Tests already cover the round-trip, so regressions surface at CI time.

### Rollback Plan

If the follow-up migration fails against staging:

1. sqlx `migrate` holds an advisory lock and runs each migration in a transaction — partial application is not possible.
2. If the migration hash on disk diverges from `_sqlx_migrations`, sqlx refuses to start (fail-loud).
3. Manual rollback: `psql` against staging, `DROP TABLE workflows, event_log; DELETE FROM _sqlx_migrations WHERE version >= 20260420000001`, then rerun the init. Loses staging data but no persistent state matters yet.

Post-follow-up this rollback becomes more expensive. Anything beyond ADR-012's follow-up migration should plan its own rollback explicitly.
