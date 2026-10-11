//! Local stand-in for the Log Analytics and App Insights query endpoints.
//!
//! The agents and `AzureQueryClient` run unmodified; only the HTTP endpoint is
//! replaced. Each request's KQL is mapped back to the template that produced
//! it, and the scenario's fixture rows for that template are returned in the
//! Azure `tables[]` response shape.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::scenario::Fixture;

/// Fixture key that applies to every template without its own entry.
pub const WILDCARD_KEY: &str = "*";

/// Template name → text that appears in that template's KQL and no other.
/// Markers are template-specific expressions, never bare table names, so a
/// new template over the same table cannot silently borrow another's rows.
/// The tests below keep this in step with `sre_azure::kql_templates`.
pub const TEMPLATE_KEYS: &[(&str, &str)] = &[
    ("error_spike", "ErrorCount=count()"),
    ("error_details", "InnermostMessage"),
    ("log_patterns", "Message=substring(Message"),
    ("latency_analysis", "P50=percentile"),
    ("failure_breakdown", "by OperationName, ResultCode"),
    ("resource_utilization", "Name in (\"% Processor Time\""),
    ("dependency_performance", "CallCount=count()"),
    ("trace_failure_correlation", "FailedTraces=dcount"),
    ("trace_latency_breakdown", "AvgDependencyShare"),
    ("recent_deployments", "CategoryValue == \"Administrative\""),
    ("resource_health", "CategoryValue == \"ResourceHealth\""),
];

/// The template that produced `kql`, if exactly one marker matches.
pub fn classify(kql: &str) -> Option<&'static str> {
    let mut hits = TEMPLATE_KEYS
        .iter()
        .filter(|(_, marker)| kql.contains(marker))
        .map(|(key, _)| *key);
    match (hits.next(), hits.next()) {
        (Some(key), None) => Some(key),
        _ => None,
    }
}

/// One request the fixture server answered.
#[derive(Debug, Clone)]
pub struct QueryHit {
    /// Template key, or `None` when the KQL matched no known template.
    pub template: Option<&'static str>,
    pub path: String,
}

/// Convert row objects into the Azure query API response body.
pub fn to_tables_json(rows: &[serde_json::Map<String, Value>]) -> Value {
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        for key in row.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    let column_defs: Vec<Value> = columns
        .iter()
        .map(|name| {
            let sample = rows
                .iter()
                .filter_map(|r| r.get(name))
                .find(|v| !v.is_null());
            json!({ "name": name, "type": column_type(sample) })
        })
        .collect();
    let row_values: Vec<Value> = rows
        .iter()
        .map(|r| {
            Value::Array(
                columns
                    .iter()
                    .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        })
        .collect();
    json!({
        "tables": [{
            "name": "PrimaryResult",
            "columns": column_defs,
            "rows": row_values,
        }]
    })
}

fn column_type(sample: Option<&Value>) -> &'static str {
    match sample {
        Some(Value::String(_)) => "string",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(n)) if n.is_f64() => "real",
        Some(Value::Number(_)) => "long",
        _ => "dynamic",
    }
}

struct KqlResponder {
    fixtures: BTreeMap<String, Fixture>,
    hits: Arc<Mutex<Vec<QueryHit>>>,
}

impl Respond for KqlResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let kql = body.get("query").and_then(Value::as_str).unwrap_or("");
        let template = classify(kql);
        self.hits
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(QueryHit {
                template,
                path: request.url.path().to_string(),
            });

        let Some(key) = template else {
            return ResponseTemplate::new(400)
                .set_body_string("eval fixture server: KQL matched no known template");
        };
        match self
            .fixtures
            .get(key)
            .or_else(|| self.fixtures.get(WILDCARD_KEY))
        {
            Some(Fixture::Rows(rows)) => {
                ResponseTemplate::new(200).set_body_json(to_tables_json(rows))
            }
            Some(Fixture::Error { status, message }) => {
                ResponseTemplate::new(*status).set_body_string(message.clone())
            }
            None => ResponseTemplate::new(200).set_body_json(to_tables_json(&[])),
        }
    }
}

/// A running fixture server for one scenario.
pub struct FixtureServer {
    server: MockServer,
    hits: Arc<Mutex<Vec<QueryHit>>>,
}

impl FixtureServer {
    pub async fn start(fixtures: BTreeMap<String, Fixture>) -> Self {
        let server = MockServer::start().await;
        let hits = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .respond_with(KqlResponder {
                fixtures,
                hits: hits.clone(),
            })
            .mount(&server)
            .await;
        Self { server, hits }
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub fn hits(&self) -> Vec<QueryHit> {
        self.hits.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_azure::kql_templates as t;

    #[test]
    fn every_template_routes_to_exactly_one_key() {
        let cases = [
            ("error_spike", t::error_spike("svc", 30, 1000)),
            ("error_details", t::error_details("svc", 30, 20)),
            ("log_patterns", t::log_patterns("svc", 30, 100)),
            ("latency_analysis", t::latency_analysis("svc", 30)),
            ("failure_breakdown", t::failure_breakdown("svc", 30)),
            ("resource_utilization", t::resource_utilization("svc", 30)),
            (
                "dependency_performance",
                t::dependency_performance("svc", 30),
            ),
            (
                "trace_failure_correlation",
                t::trace_failure_correlation("svc", 30, 1000),
            ),
            (
                "trace_latency_breakdown",
                t::trace_latency_breakdown("svc", 30, 1000),
            ),
            ("recent_deployments", t::recent_deployments("svc", 30, 1000)),
            ("resource_health", t::resource_health("svc", 30, 1000)),
        ];
        assert_eq!(cases.len(), TEMPLATE_KEYS.len());
        for (key, kql) in cases {
            assert_eq!(classify(&kql), Some(key), "template {key} misrouted");
        }
    }

    /// A template added to `kql_templates.rs` without a fixture key would be
    /// answered with an empty table or another template's rows. Count the
    /// template functions in the source so that addition fails here.
    #[test]
    fn every_kql_template_has_a_fixture_key() {
        let source = include_str!("../../azure/src/kql_templates.rs");
        let templates = source
            .lines()
            .filter(|l| l.starts_with("pub fn ") && l.contains("resource_name: &str"))
            .count();
        assert_eq!(
            templates,
            TEMPLATE_KEYS.len(),
            "kql_templates.rs has {templates} templates but TEMPLATE_KEYS has {}; add a key and a case to every_template_routes_to_exactly_one_key",
            TEMPLATE_KEYS.len()
        );
    }

    #[test]
    fn unknown_kql_is_unclassified() {
        assert_eq!(classify("StormEvents | take 10"), None);
    }

    #[test]
    fn tables_json_unions_columns_and_infers_types() {
        let rows: Vec<serde_json::Map<String, Value>> = vec![
            json!({"Target": "sql", "FailureRate": 62.5})
                .as_object()
                .unwrap()
                .clone(),
            json!({"Target": "redis", "CallCount": 10})
                .as_object()
                .unwrap()
                .clone(),
        ];
        let body = to_tables_json(&rows);
        let table = &body["tables"][0];
        let cols: Vec<&str> = table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(cols.len(), 3);
        assert_eq!(table["rows"].as_array().unwrap().len(), 2);
        let rate = table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "FailureRate")
            .unwrap();
        assert_eq!(rate["type"], "real");
    }
}
