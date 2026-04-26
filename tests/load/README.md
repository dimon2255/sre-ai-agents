# Load Baseline Harness (Phase 2a task #5)

k6-based load testing for the SRE Agents platform. Establishes steady-state,
ramp, burst, and chaos baselines so Phase 2b's NATS migration has a
regression oracle.

## Layout

```
tests/load/
├── fixtures/
│   └── alerts/
│       └── sample-alert.json    # Azure Common Alert Schema template
├── lib/
│   └── signing.js               # HMAC-SHA256 signing helper (ADR-008)
├── scenarios/
│   ├── 01-steady-state.js       # 10 VUs × 5 min, webhook-layer latency
│   ├── 02-ramp.js               # 0 → 50 VUs over 10 min, find the ceiling
│   └── 03-burst.js              # 50 VUs × 2 min, exercise backpressure
└── README.md
```

## Prerequisites

- [k6](https://k6.io/docs/getting-started/installation/) (v0.50+)
- The staging (or target) webhook endpoint reachable from this machine
- The HMAC secret matching whatever `SRE_WEBHOOK_SECRET` is on the server

## Running

### Against staging

```bash
export K6_WEBHOOK_SECRET="<same value as GitHub secret SRE_WEBHOOK_HMAC_SECRET>"
export BASE_URL="https://aca-sre-agents-demo.<region>.azurecontainerapps.io"

k6 run tests/load/scenarios/01-steady-state.js --env BASE_URL="$BASE_URL"
```

### Against a local server with auth disabled

```bash
k6 run tests/load/scenarios/01-steady-state.js \
    --env BASE_URL=http://localhost:8080 \
    --env NO_AUTH=true
```

## Scenarios

| # | File | Purpose | Thresholds | Status |
|---|------|---------|------------|--------|
| 1 | `01-steady-state.js` | 10 VUs × 5 min — establishes steady-state P50/P95/P99 | **gating** (fail < 1% errors, P95 < 500ms, P99 < 1s) | Done |
| 2 | `02-ramp.js` | Linear ramp 0 → 50 VUs over 12 min — finds throughput ceiling | informational (loose pass gates) | Done |
| 3 | `03-burst.js` | 50 VUs for 2 min — exercises backpressure (channel + concurrency limit) | gates: no 500s, no `401`s, ≥30% 202s (rest are 503 backpressure) | Done |
| 4 | `04-chaos-drain.js` | Kill a replica mid-load — validates graceful drain under real traffic | — | TODO (needs `az containerapp revision restart` hook) |

Each scenario accepts `--env NO_AUTH=true` to run against an unauthenticated local server. When `K6_WEBHOOK_SECRET` is set, requests are signed via `lib/signing.js`.

## What we're measuring (and what we're NOT)

The webhook endpoint returns `202 Accepted` as soon as the alert is
enqueued on the in-process `mpsc::Sender` — investigation runs async in
the orchestrator loop. That means:

- **k6 measures webhook-layer latency** (parse + auth + enqueue + 202).
  Expected: P95 < 500ms, P99 < 1s on staging.
- **k6 does NOT measure investigation duration.** For investigation
  P50/P95/P99, query the `workflows` table in Postgres or OTel traces
  post-run:

  ```sql
  SELECT
      percentile_cont(0.50) WITHIN GROUP (ORDER BY (updated_at - created_at)) AS p50,
      percentile_cont(0.95) WITHIN GROUP (ORDER BY (updated_at - created_at)) AS p95,
      percentile_cont(0.99) WITHIN GROUP (ORDER BY (updated_at - created_at)) AS p99
  FROM workflows
  WHERE created_at > now() - interval '10 minutes'
      AND state IN ('resolved', 'escalated', 'failed');
  ```

The baseline report in `docs/` combines k6 output (webhook latency,
throughput, error rate) with post-run Postgres / OTel analysis
(investigation duration, agent timings).

## Reporting

After each run, capture:

- k6 summary (`--summary-export` flag) — stats by HTTP code, P50/95/99, throughput
- `workflows` table percentiles (query above)
- ACA container metrics for the run window (CPU, memory) via `az monitor metrics list`
- Any readiness-probe flaps from ACA logs

Bundle into `docs/load-baseline-phase2a.md` once all four scenarios have
been executed at least once.
