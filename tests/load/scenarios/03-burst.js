// Phase 2a load baseline — scenario #3: burst.
//
// Holds 50 constant VUs for 2 minutes. Purpose: stress the concurrency
// limit (`max_concurrent_investigations = 10`) and the webhook channel
// (`channel_capacity = 64`) above their configured ceilings so we can
// observe the platform's backpressure signal (503 on channel full).
//
// Expected at peak: a non-zero 503 rate is CORRECT backpressure, not
// a failure. The test passes if 503s are handled cleanly (no 500s, no
// hung connections) and the orchestrator heartbeat stays alive during
// and after the burst.
//
// Run:
//   export K6_WEBHOOK_SECRET="..."
//   k6 run tests/load/scenarios/03-burst.js \
//       --env BASE_URL=https://... \
//       --summary-export=load-baseline-burst.json

import http from 'k6/http';
import { check, fail } from 'k6';
import { Counter, Rate } from 'k6/metrics';
import { signHeaders } from '../lib/signing.js';

const TEMPLATE = JSON.parse(open('../fixtures/alerts/sample-alert.json'));

const SECRET = __ENV.K6_WEBHOOK_SECRET;
const NO_AUTH = __ENV.NO_AUTH === 'true';
const BASE_URL = __ENV.BASE_URL;

if (!BASE_URL) {
    fail('BASE_URL env var is required');
}
if (!SECRET && !NO_AUTH) {
    fail('K6_WEBHOOK_SECRET is required unless --env NO_AUTH=true');
}

// Custom metrics so the backpressure breakdown survives into the summary.
const accepted = new Counter('webhook_202_accepted');
const backpressure = new Counter('webhook_503_backpressure');
const unexpected = new Counter('webhook_other_status');
const accepted_rate = new Rate('webhook_accepted_rate');

export const options = {
    scenarios: {
        burst: {
            executor: 'constant-vus',
            vus: 50,
            duration: '2m',
        },
    },
    thresholds: {
        // No 401s, no 5xx (except 503). If the platform returns anything
        // outside {202, 503} under burst load, something is wrong.
        'webhook_other_status': ['count==0'],
        'http_req_failed{status:500}': ['rate==0'],
        // 202 rate must remain non-trivial under burst. Observed drain at
        // 50-VU saturation on staging (0.5 vCPU, max_concurrent=10) is
        // ~5.2% of offered load (8,191/157,263 over 2 min, 2026-04-26
        // baseline run). Threshold pinned at 0.04 — 20% buffer below
        // observed. Drops below this mean either investigation duration
        // has regressed or the channel is over-shedding.
        'webhook_accepted_rate': ['rate>0.04'],
    },
};

function buildAlert(vu, iter) {
    const firedAt = new Date().toISOString();
    const alertId =
        `/subscriptions/load-test/providers/Microsoft.AlertsManagement/alerts/` +
        `burst-${vu}-${iter}-${Date.now()}`;

    const payload = JSON.parse(JSON.stringify(TEMPLATE));
    payload.data.essentials.alertId = alertId;
    payload.data.essentials.firedDateTime = firedAt;
    return JSON.stringify(payload);
}

export default function () {
    const body = buildAlert(__VU, __ITER);

    const headers = { 'Content-Type': 'application/json' };
    if (!NO_AUTH) {
        Object.assign(headers, signHeaders(SECRET, body));
    }

    const resp = http.post(`${BASE_URL}/api/v1/alerts/azure`, body, { headers });

    if (resp.status === 202) {
        accepted.add(1);
        accepted_rate.add(true);
    } else if (resp.status === 503) {
        backpressure.add(1);
        accepted_rate.add(false);
    } else {
        unexpected.add(1);
        accepted_rate.add(false);
    }

    check(resp, {
        'status in {202, 503}': (r) => r.status === 202 || r.status === 503,
    });
}
