// HMAC-SHA256 request signing helper for the SRE Agents webhook (ADR-008).
//
// Mirrors the server-side `src/middleware/hmac.rs` logic byte-for-byte:
// signs `{timestamp}.{body}` with the shared secret, returns the two
// header values the middleware requires.

import crypto from 'k6/crypto';

/**
 * Sign a request body for the webhook middleware.
 *
 * @param {string} secret - HMAC shared secret (must match SRE_WEBHOOK_SECRET on the server).
 * @param {string} body   - Raw request body — must be passed to http.post UNCHANGED.
 * @param {number} [ts]   - Optional unix-seconds timestamp. Defaults to now.
 * @returns {{ 'X-SRE-Signature': string, 'X-SRE-Timestamp': string }}
 */
export function signHeaders(secret, body, ts) {
    const timestamp = ts ?? Math.floor(Date.now() / 1000);
    const signingInput = `${timestamp}.${body}`;
    const hex = crypto.hmac('sha256', secret, signingInput, 'hex');
    return {
        'X-SRE-Signature': `sha256=${hex}`,
        'X-SRE-Timestamp': String(timestamp),
    };
}
