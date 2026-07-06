# ADR-022: Fail-Closed Production Startup Guards

**Status:** Accepted (2026-07-05)

## Context

A pre-go-live production-readiness audit (7 specialist review passes: AppSec,
security architecture, SRE reliability, CI/CD & IaC, code correctness, database,
and a consolidated threat model) found that the platform is **secure- and
safe-by-default** — every high-risk subsystem (remediation, ChatOps, gRPC, NATS,
LLM reasoning, notifications) defaults to `enabled=false`, KQL is
template-sanitized (ADR-005 holds), and all SQL is parameterized. The danger was
not the defaults but the **failure modes when a required control is misconfigured
at deploy time**, where the platform would silently run degraded instead of
refusing to start:

1. **Webhook auth fails open.** If `webhook.hmac_secret` is unset, the alert
   route mounted unauthenticated with only a `warn!` log. The webhook is the
   primary ingress on a public (`external_enabled = true`) ACA endpoint.
2. **Mock Azure auth in production.** `azure.auth_mode = "mock"` returns a fake
   token; every KQL query is meaningless and (with remediation) ARM writes route
   to a mock — a silent no-op that still reports "healthy".
3. **Remediation against real Azure is unsafe today.** The write path reuses the
   read managed identity (ADR-011 identity separation not implemented) and the
   approval gate's rate-limit / mutual-exclusion state is per-replica (ADR-021
   durable coordination not implemented). A single `SRE__REMEDIATION__ENABLED=true`
   flip enabled all of this with no compensating guard.

The existing `enforce_nats_credentials_policy` already demonstrated the correct
pattern: refuse to boot outside a dev environment when a required credential is
absent. It was applied only to NATS.

## Decision

Add three fail-closed startup guards in `run_server`, evaluated **before any
resource is touched**, mirroring the NATS pattern. All are keyed on the canonical
`observability.environment` signal (already wired through Terraform as
`SRE__OBSERVABILITY__ENVIRONMENT`) via a shared `is_dev_environment()` helper:

- `enforce_webhook_auth_policy` — bail if `hmac_secret` is empty/unset outside dev.
- `enforce_auth_mode_policy` — bail if `auth_mode == "mock"` outside dev.
- `enforce_remediation_safety_policy` — bail if `remediation.enabled` against real
  Azure (`auth_mode != "mock"`) until ADR-011 + ADR-021 land. Mock ARM (no real
  mutation) is permitted for dev/test.

The default `observability.environment` was changed from `"staging"` to
`"development"` so a bare `cargo run` and the Docker smoke test (which set nothing)
remain exempt, while real deployments — which set the environment explicitly via
Terraform — get fail-closed behavior. Dev-class environments are
`{dev, development, local, test}`.

## Consequences

- **Positive:** A forgotten HMAC secret, a leftover `auth_mode = "mock"`, or an
  ill-advised remediation enablement now produce a fast, loud boot failure with an
  actionable message, instead of a running-but-degraded (or unauthenticated)
  service. The guards are unit-tested (`src/main.rs` `mod tests`).
- **Neutral:** Production/staging deployments **must** set
  `SRE__OBSERVABILITY__ENVIRONMENT` explicitly (staging Terraform already does; a
  prod stack must set `production`). An environment left unset is treated as dev.
- **Follow-up (not resolved here):** These guards make the unsafe states
  *unreachable by accident*; they do not implement the missing capabilities. Real
  managed-identity auth (replace the `AZURE_ACCESS_TOKEN` placeholder in
  `crates/azure/src/auth.rs` with `DefaultAzureCredential`), ADR-011 identity
  separation, and ADR-021 durable remediation coordination remain required before
  the corresponding subsystems can be enabled in production.
