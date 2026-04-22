//! Integration tests for the HMAC webhook authentication middleware (ADR-008).
//!
//! These tests exercise the production `verify` function via a real Axum
//! router + tokio TcpListener + reqwest client. The downstream handler is
//! minimal (returns 202) — we are testing the middleware's header parsing,
//! body extraction, signature comparison, and status-code path, not the
//! alert-processing flow (which is covered by `webhook_tests.rs`).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;

use sre_agents_platform::middleware::hmac::{self as hmac_auth, HmacAuth};

const SECRET: &[u8] = b"test-hmac-secret-32-bytes-xxxxxx";
const SKEW_SECONDS: u64 = 300;

async fn setup_auth_server() -> String {
    let auth = HmacAuth::new(SECRET.to_vec(), SKEW_SECONDS);

    // Minimal downstream handler — echoes 202 Accepted on success.
    async fn ok() -> StatusCode {
        StatusCode::ACCEPTED
    }

    let app = Router::new().route("/api/v1/alerts/azure", post(ok)).layer(
        axum::middleware::from_fn_with_state(auth, hmac_auth::verify),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn valid_signature_and_timestamp_returns_202() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();
    let sig = hmac_auth::sign(SECRET, ts, body);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, sig)
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn invalid_signature_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();
    // Sign with the wrong secret — same format, wrong HMAC.
    let sig = hmac_auth::sign(b"wrong-secret", ts, body);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, sig)
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_signature_header_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_timestamp_header_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();
    let sig = hmac_auth::sign(SECRET, ts, body);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, sig)
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_timestamp_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    // 10 minutes in the past — outside the 5-minute skew window.
    let ts = now_secs() - 600;
    let sig = hmac_auth::sign(SECRET, ts, body);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, sig)
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_signature_prefix_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();
    // Missing "sha256=" prefix.
    let bare_hex = "a".repeat(64);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, bare_hex)
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(body.to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn body_tampering_returns_401() {
    let base_url = setup_auth_server().await;
    let body = br#"{"alert":"test"}"#;
    let ts = now_secs();
    // Sign the original body, then send a different one.
    let sig = hmac_auth::sign(SECRET, ts, body);

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/alerts/azure"))
        .header(hmac_auth::SIGNATURE_HEADER, sig)
        .header(hmac_auth::TIMESTAMP_HEADER, ts.to_string())
        .body(b"tampered-body".to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
