//! HMAC-SHA256 webhook authentication middleware (ADR-008).
//!
//! Validates every request with headers:
//!   X-SRE-Signature: sha256=<hex-of-HMAC-SHA256(secret, "{timestamp}.{body}")>
//!   X-SRE-Timestamp: <unix_seconds>
//!
//! Rejects with 401 on any failure — missing header, malformed signature,
//! signature mismatch, or timestamp outside the configured skew window.
//! Constant-time comparison via `subtle::ConstantTimeEq` closes the timing
//! side-channel.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use hmac::{Hmac, KeyInit, Mac}; // hmac 0.13: `new_from_slice` moved to the KeyInit trait
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

pub const SIGNATURE_HEADER: &str = "x-sre-signature";
pub const TIMESTAMP_HEADER: &str = "x-sre-timestamp";
pub const SIGNATURE_PREFIX: &str = "sha256=";

/// Middleware state: shared HMAC secret + allowed clock skew in seconds.
#[derive(Clone)]
pub struct HmacAuth {
    secret: Arc<Vec<u8>>,
    skew_seconds: u64,
}

impl HmacAuth {
    pub fn new(secret: impl Into<Vec<u8>>, skew_seconds: u64) -> Self {
        Self {
            secret: Arc::new(secret.into()),
            skew_seconds,
        }
    }
}

/// Axum middleware fn. Mount via `middleware::from_fn_with_state(auth, verify)`.
pub async fn verify(
    State(auth): State<HmacAuth>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let (parts, body) = request.into_parts();

    let sig_header = parts
        .headers
        .get(SIGNATURE_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| reject("missing X-SRE-Signature"))?;
    let ts_header = parts
        .headers
        .get(TIMESTAMP_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| reject("missing X-SRE-Timestamp"))?;

    let sig_hex = sig_header
        .strip_prefix(SIGNATURE_PREFIX)
        .ok_or_else(|| reject("signature missing 'sha256=' prefix"))?;
    let provided = hex::decode(sig_hex).map_err(|_| reject("signature is not valid hex"))?;

    let ts: u64 = ts_header
        .parse()
        .map_err(|_| reject("timestamp is not a u64"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let skew = now.abs_diff(ts);
    if skew > auth.skew_seconds {
        warn!(
            skew,
            limit = auth.skew_seconds,
            "HMAC timestamp out of skew window"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Bound the body BEFORE buffering it. This middleware runs *before*
    // signature verification, so an unauthenticated client must not be able to
    // force an unbounded allocation (a `usize::MAX` cap here is a pre-auth
    // memory-exhaustion DoS). The router's `DefaultBodyLimit` does not apply to
    // this manual `to_bytes` call, so we enforce the same 64 KiB ceiling here —
    // Azure Common Alert Schema payloads are 2–5 KB. `to_bytes` returns Err when
    // the body exceeds the limit.
    const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;
    let body_bytes = axum::body::to_bytes(body, MAX_WEBHOOK_BODY_BYTES)
        .await
        .map_err(|_| reject("request body exceeds 64 KiB limit or is unreadable"))?;

    let mut mac =
        HmacSha256::new_from_slice(&auth.secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(ts_header.as_bytes());
    mac.update(b".");
    mac.update(&body_bytes);
    let expected = mac.finalize().into_bytes();

    if provided.ct_eq(expected.as_slice()).into() {
        let request = Request::from_parts(parts, Body::from(body_bytes));
        Ok(next.run(request).await)
    } else {
        warn!("HMAC signature mismatch");
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn reject(reason: &'static str) -> StatusCode {
    warn!(reason, "HMAC auth rejected request");
    StatusCode::UNAUTHORIZED
}

/// Helper for clients (tests, k6 signer): compute the signature header value
/// for a given timestamp + body. Returns `"sha256=<hex>"`.
#[allow(dead_code)] // consumed by integration tests + lib re-export, not the binary
pub fn sign(secret: &[u8], timestamp: u64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!(
        "{SIGNATURE_PREFIX}{}",
        hex::encode(mac.finalize().into_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_matches_verification_math() {
        let secret = b"test-secret-32-bytes-for-hmac256";
        let ts: u64 = 1_700_000_000;
        let body = br#"{"hello":"world"}"#;

        let header = sign(secret, ts, body);
        assert!(header.starts_with(SIGNATURE_PREFIX));

        let sig_hex = header.strip_prefix(SIGNATURE_PREFIX).unwrap();
        let provided = hex::decode(sig_hex).unwrap();

        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let expected = mac.finalize().into_bytes();

        assert!(bool::from(provided.ct_eq(expected.as_slice())));
    }

    #[test]
    fn ct_eq_rejects_different_signature() {
        let a = [0u8; 32];
        let b = [1u8; 32];
        assert!(!bool::from(a.ct_eq(&b)));
    }

    #[test]
    fn ct_eq_rejects_length_mismatch() {
        let a = [0u8; 32];
        let b = [0u8; 31];
        assert!(!bool::from(a.as_slice().ct_eq(&b)));
    }

    #[test]
    fn sign_body_sensitivity() {
        let secret = b"test-secret";
        let ts: u64 = 1_700_000_000;

        let sig_a = sign(secret, ts, b"body-a");
        let sig_b = sign(secret, ts, b"body-b");
        assert_ne!(
            sig_a, sig_b,
            "different bodies must produce different signatures"
        );
    }

    #[test]
    fn sign_timestamp_sensitivity() {
        let secret = b"test-secret";
        let body = b"same-body";

        let sig_a = sign(secret, 1_700_000_000, body);
        let sig_b = sign(secret, 1_700_000_001, body);
        assert_ne!(
            sig_a, sig_b,
            "different timestamps must produce different signatures"
        );
    }
}
