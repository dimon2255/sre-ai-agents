# ADR-007: CI/CD Pipeline, Containerization, and ACA Deployment

## Status

Accepted (2026-04-10)

## Context

Phase 1 core implementation is complete — orchestrator, agents, KQL templates, webhook handler, SQLite persistence — but the platform lacks CI/CD automation, container artifacts, and deployment configuration. The exit criterion "CI pipeline builds and tests pass" is unmet. The platform targets Azure Container Apps (ACA) for hosting.

Key questions:
1. Where should TLS terminate?
2. What CI gates are required before merging to main?
3. How should the binary be packaged for deployment?
4. How do we validate NATS readiness for Phase 2 without introducing NATS?

## Decision

### 1. GitHub Actions CI Pipeline

Three independent workflows running on push and PR to main:

- **ci.yml**: Lint (rustfmt + clippy with `-D warnings`), test (`cargo test --workspace`), and release build. Lint and test run in parallel; build depends on both passing. Uses `dtolnay/rust-toolchain` for pinned Rust and `actions/cache` for cargo registry and target directory.
- **security.yml**: `cargo-audit` dependency scanning on push and weekly Monday cron (catches new RustSec advisories between pushes).
- **docker.yml**: Docker image build and smoke test — build the image, start the container, verify `/healthz` returns 200.

### 2. Multi-Stage Dockerfile

Two-stage build:
- **Builder**: `rust:1.86-bookworm` — full toolchain, compiles release binary.
- **Runtime**: `debian:bookworm-slim` — minimal image (~80-100MB) with only `ca-certificates` for outbound TLS to Azure APIs. Non-root user (`sre`). No inbound TLS configuration.

Release profile optimizations: `lto = "thin"`, `strip = true`, `codegen-units = 1` for smaller binary (~10-15MB vs ~30MB default).

### 3. Azure Container Apps Deployment

- **TLS termination at ACA ingress** — auto-provisioned certificates, no container-level TLS. Container listens on plain HTTP port 8080.
- **Health probes**:
  - `GET /healthz` — liveness probe (unconditional 200)
  - `GET /readyz` — readiness probe (verifies SQLite connectivity via `SELECT 1`)
  - ACA startup probe on `/readyz` with higher failure threshold for migration window
- **Auth**: Managed Identity for Azure API access (zero-credential deployment)
- **Registry**: Azure Container Registry (ACR), native ACA integration
- **Scaling**: Start with `min: 1, max: 3` — never scale to zero (incident response platform must be warm)
- **Logging**: stdout/stderr → ACA Log Analytics (structured JSON via tracing-subscriber)

### 4. Serialization Roundtrip Spike

All message types (`AgentMessage`, `AgentPayload`, `AlertInfo`, `Finding`, `InvestigationReport`) already derive `Serialize`/`Deserialize`. Roundtrip tests validate JSON encode/decode for all payload variants.

**Key finding**: `InboundMessage` contains `mpsc::Sender<AgentMessage>` which is inherently non-serializable. For Phase 2 NATS migration, replace the sender with a reply subject string:
```rust
// Phase 1 (in-process):
pub struct InboundMessage {
    pub message: AgentMessage,
    pub reply_tx: mpsc::Sender<AgentMessage>,
}

// Phase 2 (NATS):
pub struct InboundMessage {
    pub message: AgentMessage,
    pub reply_subject: String,  // NATS reply subject
}
```
The agent trait signature remains the same — only the reply mechanism changes. This is a clean boundary documented by the roundtrip tests.

## Consequences

### What becomes easier
- Every PR is gated by lint, test, build, and security checks — regressions caught before merge
- Container image is reproducible and portable (can deploy to AKS, ACI, or any container platform)
- ACA handles TLS, scaling, and health probes — zero Kubernetes complexity in Phase 1
- Serialization roundtrip tests give confidence for NATS migration in Phase 2

### What becomes harder
- CI adds ~3-5 minutes to PR cycle (acceptable for correctness guarantee)
- ACA vendor lock-in for deployment (mitigated: Dockerfile is platform-agnostic)
- Base image pinning requires deliberate updates for security patches

### What we're giving up
- No Kubernetes flexibility (custom ingress controllers, service mesh) — can migrate to AKS if ACA limits are hit
- No blue-green or canary deployment in Phase 1 — ACA revision management handles basic traffic splitting
- No multi-region deployment — single-region sufficient for Phase 1 read-only platform
