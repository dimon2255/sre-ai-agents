// Phase 2a load baseline — scenario #1: steady state.
//
// Keeps a fixed concurrency against the signed webhook endpoint for 5
// minutes to establish P50/P95/P99 baseline numbers and confirm no
// steady-state pressure on the webhook layer.
//
// Run:
//   export K6_WEBHOOK_SECRET="<matches SRE_WEBHOOK_SECRET on the server>"
//   k6 run tests/load/scenarios/01-steady-state.js \
//       --env BASE_URL=https://aca-sre-agents-demo.<region>.azurecontainerapps.io
//
// For local dev against a server running with SRE_WEBHOOK_SECRET unset,
// pass --env NO_AUTH=true to skip signing (still sends a dummy timestamp).

import http from 'k6/http';
import { check, fail } from 'k6';
import { SharedArray } from 'k6/data';
import { signHeaders } from '../lib/signing.js';

const TEMPLATE = JSON.parse(open('../fixtures/alerts/sample-alert.json'));

const SECRET = __ENV.K6_WEBHOOK_SECRET;
const NO_AUTH = __ENV.NO_AUTH === 'true';
const BASE_URL = __ENV.BASE_URL;

if (!BASE_URL) {
    fail('BASE_URL env var is required (e.g. https://aca-sre-agents-demo.<region>.azurecontainerapps.io)');
}
if (!SECRET && !NO_AUTH) {
    fail('K6_WEBHOOK_SECRET is required unless --env NO_AUTH=true');
}

export const options = {
    scenarios: {
        steady: {
            // constant-arrival-rate paces the load — each second brings
            // exactly `rate` iterations regardless of how long each takes.
            // That is what "steady state" means: a sustainable arrival
            // rate the platform services without backpressure.
            // constant-vus (unbounded per-VU throughput) is NOT steady
            // state — it is an overload test, which is scenario #3's job.
            executor: 'constant-arrival-rate',
            rate: 50,
            timeUnit: '1s',
            duration: '5m',
            preAllocatedVUs: 20,
            maxVUs: 100,
        },
    },
    // Webhook endpoint returns 202 Accepted and enqueues the alert — no
    // downstream I/O on the hot path, so these thresholds target the
    // webhook layer itself, not investigation duration. Investigation
    // latency must be measured from OTel traces in a follow-up run.
    //
    // 50 req/s steady was chosen empirically from an earlier 10-VU
    // constant-vus run that saturated the channel near ~95 req/s
    // (channel_capacity=64, max_concurrent_investigations=10,
    // staging ACA @ 0.5 vCPU). 50 leaves comfortable headroom so
    // backpressure is not the story being measured here — scenario #3
    // covers that.
    thresholds: {
        'http_req_failed': ['rate<0.01'],
        'http_req_duration{expected_response:true}': ['p(95)<500', 'p(99)<1000'],
        'checks': ['rate>0.99'],
    },
};

function buildAlert(vu, iter) {
    // Each request gets a unique alertId so the platform does not dedupe
    // and so the `workflows` audit table reflects real fan-out.
    const firedAt = new Date().toISOString();
    const alertId =
        `/subscriptions/load-test/providers/Microsoft.AlertsManagement/alerts/` +
        `loadtest-${vu}-${iter}-${Date.now()}`;

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
        'status is 202': (r) => r.status === 202,
        'latency < 1s': (r) => r.timings.duration < 1000,
    });
}
