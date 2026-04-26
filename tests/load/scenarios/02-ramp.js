// Phase 2a load baseline — scenario #2: ramp.
//
// Linearly increases concurrency from 0 → 50 VUs over 10 minutes, then
// holds at 50 for 2 minutes. Purpose: find the throughput ceiling and
// the VU count at which P99 latency starts to degrade. Drives the
// "concurrent investigations before heartbeat starvation" number in
// the baseline report.
//
// Thresholds here are INFORMATIONAL (not gating) — this scenario is an
// exploration. Hard gates live in scenario #1 (steady state).
//
// Run:
//   export K6_WEBHOOK_SECRET="..."
//   k6 run tests/load/scenarios/02-ramp.js \
//       --env BASE_URL=https://... \
//       --summary-export=load-baseline-ramp.json

import http from 'k6/http';
import { check, fail } from 'k6';
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

export const options = {
    scenarios: {
        ramp: {
            executor: 'ramping-vus',
            startVUs: 0,
            stages: [
                { duration: '2m', target: 10 },   // warm up
                { duration: '3m', target: 25 },   // mid ramp
                { duration: '3m', target: 40 },   // approach ceiling
                { duration: '2m', target: 50 },   // peak
                { duration: '2m', target: 50 },   // hold at peak
            ],
            gracefulRampDown: '30s',
        },
    },
    // Informational thresholds — we want the data, not a pass/fail gate.
    // The steady-state scenario owns the hard gates.
    thresholds: {
        'http_req_failed': ['rate<0.05'],  // allow up to 5% 503s at peak
        'http_req_duration{expected_response:true}': ['p(95)<2000', 'p(99)<5000'],
        'checks': ['rate>0.90'],
    },
};

function buildAlert(vu, iter) {
    const firedAt = new Date().toISOString();
    const alertId =
        `/subscriptions/load-test/providers/Microsoft.AlertsManagement/alerts/` +
        `ramp-${vu}-${iter}-${Date.now()}`;

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

    check(resp, {
        'status is 202 or 503': (r) => r.status === 202 || r.status === 503,
        'not rejected by auth': (r) => r.status !== 401,
        'not server error': (r) => r.status !== 500,
    });
}
