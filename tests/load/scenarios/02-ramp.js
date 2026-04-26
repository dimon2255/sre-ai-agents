// Phase 2a load baseline — scenario #2: ramp (arrival-rate).
//
// Linearly increases the OFFERED ARRIVAL RATE from 50 → 150 req/s over
// 10 minutes. Purpose: trace the latency-vs-throughput curve around the
// observed knee (~89 req/s drain on a single 0.5 vCPU ACA replica with
// max_concurrent_investigations=10). The first stage matches scenario #1
// (50 req/s, verified good), the last stage overshoots to validate that
// degradation past the knee is graceful (503 backpressure, no 5xx).
//
// The earlier ramping-vus design hammered every stage above the knee
// from t=0 (10 VUs at ~30 req/s each = ~300 req/s offered on the very
// first stage), producing 88% failures but no usable curve. Switched
// to ramping-arrival-rate so the knee position can be read off the
// chart instead of inferred.
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
            executor: 'ramping-arrival-rate',
            startRate: 25,
            timeUnit: '1s',
            preAllocatedVUs: 50,
            maxVUs: 200,
            stages: [
                { duration: '2m', target: 50 },   // matches scenario #1 baseline (verified good)
                { duration: '2m', target: 75 },   // approach knee
                { duration: '2m', target: 100 },  // around knee (~89 req/s observed drain)
                { duration: '2m', target: 125 },  // past knee
                { duration: '2m', target: 150 },  // overload — validates clean degradation
            ],
            gracefulStop: '30s',
        },
    },
    // Informational thresholds — we want the data, not a pass/fail gate.
    // Failures past the knee are EXPECTED (503 backpressure). The
    // steady-state scenario owns the hard gates.
    thresholds: {
        'http_req_failed': ['rate<0.50'],  // overload stages will push this
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
