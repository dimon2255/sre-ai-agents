# ADR-008: Webhook Authentication via HMAC-SHA256 Shared Secret

## Status

Accepted (2026-04-21)

## Context

Phase 1 intentionally deferred webhook authentication — the `POST /api/v1/alerts/azure` endpoint accepts any Common Alert Schema payload that reaches it, relying entirely on Azure Container Apps ingress isolation (no public FQDN, internal ingress only) as a coarse network boundary. This was acceptable for a local-and-staging MVP but is insufficient for production.

Phase 2a pushes the platform toward external exposure: staging is deployed on ACA with the webhook reachable from Azure Monitor action groups, and Phase 2b plans to extend the ingest surface with a gRPC API and NATS-driven event pipeline. Before any of that lands, the webhook endpoint needs a request-authenticity mechanism that:

1. Rejects forged alerts that waste investigation cycles (Azure API calls against Log Analytics / App Insights have real cost).
2. Survives a compromised internal network — "behind ACA ingress" must not be the only control.
3. Is implementable against Azure Monitor action groups, which do **not** natively support HMAC request signing.
4. Does not require an Azure AD tenant round-trip on every alert (latency and dependency risk for an incident-response platform).

### Threat Model (webhook endpoint)

| # | Threat | STRIDE | Likelihood | Impact |
|---|--------|--------|------------|--------|
| 1 | Forged alert triggers investigation → KQL query cost + agent cycles | Spoofing | Medium | Financial / DoS |
| 2 | Replayed valid alert → duplicate investigations, storage bloat in `event_log` | Tampering | Low | Operational |
| 3 | Leak of shared secret via logs, config dump, or compromised CI | Information disclosure | Medium | Full bypass |
| 4 | Timing side-channel on signature verification | Information disclosure | Low | Secret recovery (theoretical) |
| 5 | Denial-of-service via signature-compute amplification on junk requests | DoS | Low | Availability |

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. HMAC-SHA256 shared secret** (chosen) | No external dependency; sub-millisecond verification; works with any webhook sender via a pre-shared secret pasted into the Azure Monitor action group URL/header configuration; rotatable via ACA secret update | Shared secret must be distributed and rotated; no per-sender identity |
| B. Azure AD bearer token validation | Cryptographically verifiable sender identity; integrates with Azure tenant RBAC | Azure Monitor action groups do not natively sign outbound webhooks with an AAD token without a Logic App / Azure Function shim; adds 100-300ms JWKS fetch + validation on first request; requires tenant + audience config and JWKS caching |
| C. Mutual TLS via ACA ingress | Transport-layer strong identity | ACA client certificate support is limited in managed-ingress mode; operational cost of distributing and rotating client certs is high; overkill for an internal alerting webhook |
| D. IP allowlist via ACA ingress rules | Zero code | Azure Monitor egress IPs are published but span large CIDR ranges; does not prevent spoofing from another tenant inside the same range; brittle against Azure IP-space changes |

## Decision

### 1. HMAC-SHA256 middleware on `POST /api/v1/alerts/azure` only

A new Tower/Axum middleware `src/middleware/hmac.rs` validates every request to the alert endpoint:

- Reads `X-SRE-Signature: sha256=<hex>` request header (64-char lowercase hex).
- Reads `X-SRE-Timestamp: <unix_seconds>` request header for replay resistance.
- Buffers the raw request body (bounded by the existing `DefaultBodyLimit::max(64 * 1024)` layer).
- Computes `HMAC-SHA256(secret, f"{timestamp}.{raw_body}")` and compares with the supplied signature using a constant-time comparison (`subtle::ConstantTimeEq`).
- Rejects with `401 Unauthorized` and no response body on any failure (missing header, malformed, signature mismatch, timestamp outside ±300s skew window).
- Health probes (`/startupz`, `/healthz`, `/readyz`) remain unauthenticated — ACA probes cannot sign requests, and exposing liveness carries negligible risk.

### 2. Timestamp-bound signature format

The signed payload is `{timestamp}.{raw_body}` rather than `{raw_body}` alone. This binds each signature to a specific moment in time and makes replays outside a 5-minute window invalid even if the attacker captures a valid signed request. The skew window is configurable via `SRE__WEBHOOK__HMAC_SKEW_SECONDS` (default: 300).

### 3. Configuration

Secret is read from environment variable `SRE_WEBHOOK_SECRET` (32+ random bytes, base64 or hex encoded — format-agnostic, treated as an opaque byte string). Surfaced in `PlatformConfig` under `WebhookConfig::hmac_secret: Option<String>`:

- `None` → middleware is **not mounted** (explicit opt-out for local dev; startup logs a loud warning).
- `Some(secret)` → middleware is mounted on `/api/v1/alerts/azure`.

Staging and production must set the secret. CI smoke tests set a fixed test secret. Local `cargo run -- serve` without the env var works, logs a warning, and runs unauthenticated (Phase 1 behavior preserved for developer ergonomics).

### 4. Secret provisioning

- **Staging**: Terraform adds an `azurerm_container_app_environment_secret` (or `secret` block on the container app) named `webhook-hmac-secret`, sourced from a GitHub Actions secret `SRE_WEBHOOK_HMAC_SECRET`. Mirrors the existing `SRE_AGENTS_DB_PASSWORD` flow established during the Postgres cut-over (Phase 2a task #3).
- **Production**: same pattern, separate secret.
- **Rotation**: update the GitHub secret, re-run the Terraform deploy workflow. ACA rolls the revision with the new secret. Brief (seconds) overlap where both old and new secrets may be accepted is **not implemented in Phase 2a** — accept a short alert-delivery gap during rotation (Azure Monitor retries). Dual-secret grace window can be added later if rotation-without-gap becomes a requirement.

### 5. Constant-time comparison

Signature verification uses the `subtle` crate (`subtle = "2.5"`) for `ConstantTimeEq`. Hand-rolled `==` on bytes is forbidden. This closes threat #4 (timing side-channel).

### 6. Load test integration

The k6 load harness (Phase 2a task #5, `tests/load/`) must sign every request with the same secret the staging deployment is using. The secret is injected into the k6 scenario via an env var (`K6_WEBHOOK_SECRET`). **No auth-bypass path is added for load testing** — the test harness plays the role of a legitimate signed client.

### 7. Deferred: Azure AD bearer validation

AAD bearer validation is **deferred to Phase 2b** (tracked as a follow-up ADR). Rationale: the shim required to make Azure Monitor sign with an AAD token (Logic App or Function proxy) adds operational complexity that outweighs the marginal security improvement over HMAC for an internal alerting flow. If/when the webhook begins accepting traffic from third-party integrations, AAD validation becomes a complement to — not a replacement for — HMAC.

## Consequences

### Positive

- The `/api/v1/alerts/azure` endpoint is no longer trust-all-network.
- Replay window is bounded at 5 minutes; attacker needs both the secret and a fresh timestamp.
- Constant-time verification closes a theoretical side channel.
- No external-service dependency on the hot path (unlike AAD/JWKS).
- Pattern generalizes to any future webhook sender: pre-share a secret, sign requests.
- Unblocks Phase 2a load baseline (signed k6 traffic is apples-to-apples with real signed alerts).

### Negative

- Azure Monitor action groups require the signer logic to live somewhere — either a Logic App pre-processor stamps the headers, or (simpler) a Function App receives the raw Azure Monitor webhook and re-posts with HMAC headers. This adds a small hop and an additional thing to monitor. Cost: one Logic App per action group, <$0.01/alert.
- Shared-secret rotation has a momentary alert-delivery gap. Alerts delivered during the gap are retried by Azure Monitor per its built-in retry policy, so loss is unlikely but not impossible.
- No per-sender identity — if the secret is compromised, every caller is compromised. Mitigated by scoping the secret to a single webhook, rotating on suspicion, and layering ACA ingress isolation beneath.
- Middleware tightens the contract: any legitimate future caller must also sign. Discoverability of this requirement lives in ADR-008 + operator docs.

### Risks Addressed / Left Open

| Threat | Addressed? | How |
|--------|-----------|-----|
| #1 Forged alert | Yes | HMAC-SHA256 with shared secret |
| #2 Replay | Yes (bounded) | 5-minute timestamp skew window |
| #3 Secret leak | Partial | Stored only in ACA secret + GitHub Actions secret; never logged; rotatable. Does not prevent leaks from compromised CI — see ADR-011 for identity separation |
| #4 Timing side-channel | Yes | `subtle::ConstantTimeEq` |
| #5 DoS via signature-compute | Mitigated | 64 KB body limit + HMAC-SHA256 is cheap (microseconds); fronted by ACA ingress |

### Test Coverage (Phase 2a exit criteria)

Four integration tests in `tests/hmac_auth.rs` (webhook-level, exercising the real middleware stack, not a mirror — per Phase 1 Round 2 lesson):

1. Valid signature + valid timestamp → 202 Accepted.
2. Invalid signature (right format, wrong HMAC) → 401.
3. Missing `X-SRE-Signature` or `X-SRE-Timestamp` header → 401.
4. Valid signature with timestamp outside ±300s window → 401.

Plus two unit tests on the constant-time comparison path to guard against regression to a non-CT implementation.

## References

- [ADR-007: CI/CD Pipeline, Containerization, and ACA Deployment](007-cicd-containerization-aca-deployment.md) — establishes the ACA secret pattern this ADR reuses.
- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — planned, draft in flight. Webhook auth is a precondition for any remediation surface that accepts external triggers.
- [ADR-011: Identity Separation for Remediation](011-identity-separation.md) — planned. Moves credential handling off shared-secret onto scoped managed identity for write operations.
- OWASP: [Webhook Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/REST_Security_Cheat_Sheet.html) — background reading.
