# ADR-018: ChatOps Interface

## Status

Accepted (2026-06-20). Fourth increment of the Phase 3 intelligence layer — a
conversational entry point for operators.

## Context

The Phase 3 architecture diagram lists a **ChatOps bot (Slack/Teams)** as a
first-class entry point into the orchestrator, alongside Azure Alerts, the gRPC
API, and scheduled scans. The operational value is immediacy: during an incident
an on-call engineer is already in Slack; letting them type `investigate
payments-api sev1` or `status <id>` without leaving the channel removes a context
switch and makes the platform's output land where the conversation is happening.

The platform already has the pieces this builds on:

- the **alert channel** (`mpsc::Sender<AlertInfo>`) — the webhook's fire-and-forget
  ingestion path into the orchestrator loop;
- the **store** — `get_workflow` for status, plus the persisted `report_json`;
- the **notification engine** (ADR-013/Phase 2b) — already posts investigation
  results *to* Slack. ChatOps is the inbound direction of the same channel.

The design tensions:

1. **Transport coupling.** Slack and Teams have different request/response shapes.
   The command-handling logic should not be welded to either, and must be testable
   without a live chat workspace.
2. **Triggering is asynchronous.** The webhook enqueues an `AlertInfo` and returns
   immediately; the orchestrator assigns the investigation id later. A chat user
   needs a reference they can use to follow up.
3. **Authentication.** A chat endpoint is a new ingress. Slack signs requests with
   an HMAC over `v0:timestamp:body`; verifying it is the right long-term control,
   but the platform already has an ingress-isolation posture (Phase 1) and an HMAC
   pattern (ADR-008) to build on.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. A transport-agnostic `sre-chatops` crate (pure parser + handler over a `ChatBackend` trait) + a Slack slash-command endpoint on the existing webhook server** (chosen) | Command grammar + dispatch are pure and fully unit-tested offline. `ChatBackend` abstracts trigger/status, so the handler never touches Axum, the store, or the channel directly. Slack today, Teams later, both behind one trait. Reuses the existing server, `AppState`, and alert channel — minimal new surface. | Slash-command text is a small grammar, not full NLP. Slack request-signing is deferred (ingress isolation in the interim). |
| B. Put the handler logic directly in an Axum handler in `src/` | Less indirection. | Couples parsing + dispatch to the web layer and the store types; hard to unit-test; a Teams transport means duplicating the logic. |
| C. Stand up a separate ChatOps bot service | Clean process isolation. | A second deployable, its own auth, its own orchestrator access path — disproportionate for a slash-command surface; diverges from the single-binary model. |
| D. Full NLP / LLM intent parsing | "Natural" language. | The reasoning engine is for RCA, not command parsing; an LLM in the command path adds latency, cost, and nondeterminism to `investigate`/`status`. A small grammar is faster, free, and predictable. (LLM-assisted querying can be a later, additive command.) |

## Decision

Adopt **Option A**.

### 1. `sre-chatops` crate (transport-agnostic core)

- `parse_command(&str) -> ChatCommand` — a small, forgiving grammar:
  `investigate <resource> [severity]`, `status <id>`, `help`. Verb aliases
  (`inv`/`triage`, `show`/`get`), severity aliases (`sev0`–`sev4`, bare `0`–`4`,
  `critical|high|medium|low|info`, default `Sev3`), tolerant of a leading
  `/command` token and Slack's `` `code` ``/`<link>` decoration. A **pure
  function**, exhaustively unit-tested.
- `ChatBackend` trait — `trigger_investigation(resource, severity) -> reference`
  and `get_status(id) -> Option<InvestigationStatus>`. The seam that keeps the
  handler decoupled from the web/store layers; `sre-common`-only.
- `ChatOpsHandler::handle(text) -> ChatResponse` — parses, dispatches to the
  backend, and formats a Slack-style mrkdwn reply (`ephemeral` vs `in_channel`).
  Tested against a mock backend.

### 2. Webhook-server binding

- `WebhookChatBackend` implements `ChatBackend` over `AppState`:
  - **trigger** — builds an `AlertInfo` (alert id `chatops-<uuid>`, signal type
    `ChatOps`) and `try_send`s it on the **same alert channel** the webhook uses.
    Returns the alert id as the **tracking reference**.
  - **status** — accepts a workflow UUID (from notifications/logs) *or* the
    ChatOps tracking reference: a new `WorkflowStore::get_workflow_by_alert_id`
    resolves the latter, so the reference handed back by `trigger` round-trips.
    Pulls the summary + confidence out of the persisted `report_json`.
- `POST /api/v1/chatops` (Axum `Form` extractor for Slack's URL-encoded body),
  config-gated by `[chatops] enabled` (default off), mounted only when enabled.

### 3. Authentication

The endpoint is mounted only when enabled and, like the Phase 1 webhook, relies on
ACA ingress isolation for now. Slack request-signature verification (HMAC over
`v0:timestamp:body`, mirroring ADR-008) is the documented forward step; the
`signing_secret_env` config field is already in place for it.

## Consequences

### Positive

- Operators trigger investigations and read RCAs from chat — the platform meets
  them where the incident is already being discussed.
- Command parsing + dispatch are pure and fully offline-tested (13 unit tests);
  the transport binding is thin and reuses the existing server, state, and channel.
- Slack now, Teams (or any transport) later, behind one `ChatBackend` trait — no
  handler changes.
- The tracking-reference round-trip (`get_workflow_by_alert_id`) gives a coherent
  `investigate` → `status` UX despite asynchronous triggering.
- Opt-in and isolated: disabled by default; when off, zero new surface.

### Negative

- The grammar is intentionally small (slash-command verbs, not free-form NLP). An
  LLM-assisted query command is a possible additive follow-up, deliberately kept
  out of the `investigate`/`status` hot path.
- Slack request signing is deferred; until then the endpoint depends on ingress
  isolation. It must not be exposed publicly without the signing check.
- `get_status` immediately after `trigger` can race the orchestrator (the workflow
  row may not exist yet) and return "not found" — inherent to async triggering;
  the user retries.
- Interactive approvals (approve a remediation from chat) are out of scope here;
  they pair with ADR-009's Slack-approval forward step.

### Follow-ups

- Slack signing-secret verification middleware (HMAC `v0:…`, reuse the ADR-008
  pattern).
- A Teams `ChatBackend`/transport (Adaptive Cards).
- An interactive approval command wired to the `sre-gates` `approve()` flow.
- An optional LLM-assisted `ask` command routing natural-language questions
  through the reasoning engine.

## References

- [ADR-008: Webhook Authentication](008-webhook-authentication.md) — the HMAC
  request-signing pattern the Slack verification forward step will mirror.
- [ADR-013: NATS JetStream Ingest](013-nats-jetstream-ingest.md) / Phase 2b
  Notification Engine — the *outbound* Slack channel; ChatOps is its inbound twin.
- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — the
  interactive-approval forward step shares its human-approval flow.
- `docs/diagrams/phase3-architecture.md` — the ChatOps entry point this implements.
