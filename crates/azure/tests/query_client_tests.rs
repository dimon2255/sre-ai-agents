use std::sync::Arc;

use wiremock::matchers::{body_string_contains, header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sre_azure::alerts_client::AzureAlertsClient;
use sre_azure::auth::MockTokenProvider;
use sre_azure::models::AzureCommonAlert;
use sre_azure::query_client::AzureQueryClient;

/// Sample KQL response matching Azure Log Analytics format
fn sample_kql_response(rows: Vec<Vec<serde_json::Value>>) -> serde_json::Value {
    serde_json::json!({
        "tables": [{
            "name": "PrimaryResult",
            "columns": [
                {"name": "TimeGenerated", "type": "datetime"},
                {"name": "ExceptionType", "type": "string"},
                {"name": "ProblemId", "type": "string"},
                {"name": "ErrorCount", "type": "long"},
                {"name": "DistinctErrors", "type": "long"}
            ],
            "rows": rows
        }]
    })
}

fn sample_latency_response() -> serde_json::Value {
    serde_json::json!({
        "tables": [{
            "name": "PrimaryResult",
            "columns": [
                {"name": "TimeGenerated", "type": "datetime"},
                {"name": "P50", "type": "real"},
                {"name": "P95", "type": "real"},
                {"name": "P99", "type": "real"},
                {"name": "FailureRate", "type": "real"},
                {"name": "TotalRequests", "type": "long"}
            ],
            "rows": [
                ["2026-04-06T02:00:00Z", 45.0, 250.0, 980.0, 2.5, 1500],
                ["2026-04-06T01:59:00Z", 42.0, 230.0, 900.0, 1.8, 1450]
            ]
        }]
    })
}

fn sample_empty_response() -> serde_json::Value {
    serde_json::json!({
        "tables": [{
            "name": "PrimaryResult",
            "columns": [],
            "rows": []
        }]
    })
}

/// Helper: create an AzureQueryClient pointed at the mock server
fn test_client(mock_uri: &str) -> AzureQueryClient {
    let credential = Arc::new(MockTokenProvider::new("test-token"));
    AzureQueryClient::new(credential)
        .with_log_analytics_base_url(mock_uri)
        .with_app_insights_base_url(mock_uri)
}

// ─── Query Client Tests (exercising real AzureQueryClient code) ───

#[tokio::test]
async fn test_query_log_analytics_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .and(header("authorization", "Bearer test-token"))
        .and(body_string_contains("AppExceptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sample_kql_response(vec![
                vec![
                    "2026-04-06T02:00:00Z".into(),
                    "NullReferenceException".into(),
                    "prob-001".into(),
                    serde_json::json!(42),
                    serde_json::json!(3),
                ],
                vec![
                    "2026-04-06T01:59:00Z".into(),
                    "TimeoutException".into(),
                    "prob-002".into(),
                    serde_json::json!(15),
                    serde_json::json!(1),
                ],
            ])),
        )
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client
        .query_log_analytics(
            "test-workspace",
            "AppExceptions | where TimeGenerated > ago(30m)",
            "PT30M",
        )
        .await
        .unwrap();

    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].row_count(), 2);

    let records = result.tables[0].to_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["ExceptionType"], "NullReferenceException");
    assert_eq!(records[0]["ErrorCount"], 42);
    assert_eq!(records[1]["ExceptionType"], "TimeoutException");
}

#[tokio::test]
async fn test_query_returns_empty_results() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_empty_response()))
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client
        .query_log_analytics("test-workspace", "test query", "PT1H")
        .await
        .unwrap();

    assert_eq!(result.tables[0].row_count(), 0);
}

#[tokio::test]
async fn test_query_handles_rate_limiting() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Rate limited"))
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client
        .query_log_analytics("test-workspace", "test query", "PT1H")
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        sre_common::errors::AzureError::RateLimited => {} // expected
        other => panic!("Expected RateLimited, got: {other}"),
    }
}

#[tokio::test]
async fn test_query_handles_server_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "error": {
                "message": "Internal Server Error",
                "code": "InternalError"
            }
        })))
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client
        .query_log_analytics("test-workspace", "test query", "PT1H")
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        sre_common::errors::AzureError::ApiError { status, .. } => {
            assert_eq!(status, 500);
        }
        other => panic!("Expected ApiError, got: {other}"),
    }
}

#[tokio::test]
async fn test_query_app_insights_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/v1/apps/.*/query"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_latency_response()))
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client
        .query_app_insights("test-app", "AppRequests | summarize ...", "PT30M")
        .await
        .unwrap();

    let records = result.tables[0].to_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["P95"], 250.0);
    assert_eq!(records[0]["FailureRate"], 2.5);
    assert_eq!(records[0]["TotalRequests"], 1500);
}

#[tokio::test]
async fn test_query_injects_bearer_token() {
    let mock_server = MockServer::start().await;

    // Only match requests with the correct bearer token
    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .and(header("authorization", "Bearer my-secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_empty_response()))
        .mount(&mock_server)
        .await;

    let credential = Arc::new(MockTokenProvider::new("my-secret-token"));
    let client = AzureQueryClient::new(credential).with_log_analytics_base_url(&mock_server.uri());

    let result = client.query_log_analytics("ws-1", "test", "PT1H").await;

    // If the bearer token wasn't injected correctly, wiremock returns 404
    assert!(
        result.is_ok(),
        "Bearer token should be injected: {result:?}"
    );
}

#[tokio::test]
async fn test_query_sends_kql_in_body() {
    let mock_server = MockServer::start().await;

    let kql = "AppExceptions | where TimeGenerated > ago(30m) | take 100";

    Mock::given(method("POST"))
        .and(path_regex("/v1/workspaces/.*/query"))
        .and(body_string_contains(kql))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_empty_response()))
        .mount(&mock_server)
        .await;

    let client = test_client(&mock_server.uri());

    let result = client.query_log_analytics("ws-1", kql, "PT30M").await;

    assert!(
        result.is_ok(),
        "KQL query should be in request body: {result:?}"
    );
}

// ─── Alert Parsing Tests ───

#[tokio::test]
async fn test_parse_common_alert_schema() {
    let payload: AzureCommonAlert = serde_json::from_value(serde_json::json!({
        "schemaId": "azureMonitorCommonAlertSchema",
        "data": {
            "essentials": {
                "alertId": "/subscriptions/abc/providers/Microsoft.AlertsManagement/alerts/alert-123",
                "alertRule": "High Error Rate",
                "severity": "Sev1",
                "signalType": "Metric",
                "monitorCondition": "Fired",
                "alertTargetIDs": [
                    "/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/payments-api"
                ],
                "firedDateTime": "2026-04-06T02:00:00.000Z",
                "description": "Error rate exceeded 5%"
            },
            "alertContext": {
                "conditionType": "SingleResourceMultipleMetricCriteria",
                "condition": {
                    "allOf": [{
                        "metricName": "Http5xx",
                        "metricValue": 150.0,
                        "threshold": 10.0
                    }]
                }
            }
        }
    }))
    .unwrap();

    let alert = AzureAlertsClient::parse_common_alert(&payload).unwrap();

    assert_eq!(alert.alert_rule, "High Error Rate");
    assert!(matches!(
        alert.severity,
        sre_common::messages::AlertSeverity::Sev1
    ));
    assert_eq!(alert.signal_type, "Metric");
    assert_eq!(alert.monitor_condition, "Fired");
    assert_eq!(
        alert.affected_resource,
        "/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/payments-api"
    );
    assert_eq!(alert.description.as_deref(), Some("Error rate exceeded 5%"));
    assert!(alert.raw_context.is_some());
}

#[tokio::test]
async fn test_parse_common_alert_invalid_severity() {
    let payload: AzureCommonAlert = serde_json::from_value(serde_json::json!({
        "schemaId": "azureMonitorCommonAlertSchema",
        "data": {
            "essentials": {
                "alertId": "alert-1",
                "alertRule": "Test",
                "severity": "SevUnknown",
                "signalType": "Metric",
                "monitorCondition": "Fired",
                "firedDateTime": "2026-04-06T02:00:00.000Z"
            },
            "alertContext": {}
        }
    }))
    .unwrap();

    let result = AzureAlertsClient::parse_common_alert(&payload);
    assert!(result.is_err());
}

// ─── KQL Table Helper Tests ───

#[test]
fn test_kql_table_to_records() {
    let response: sre_azure::models::KqlQueryResponse =
        serde_json::from_value(sample_kql_response(vec![vec![
            "2026-04-06T02:00:00Z".into(),
            "NullReferenceException".into(),
            "prob-001".into(),
            serde_json::json!(42),
            serde_json::json!(3),
        ]]))
        .unwrap();

    let table = &response.tables[0];
    assert_eq!(table.row_count(), 1);

    let records = table.to_records();
    assert_eq!(records[0]["ExceptionType"], "NullReferenceException");
    assert_eq!(records[0]["ErrorCount"], 42);
}
