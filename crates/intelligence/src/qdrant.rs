//! Real knowledge-store binding: [Qdrant](https://qdrant.tech) vector DB over
//! its REST API. There is no need for a heavyweight client — three endpoints
//! cover it: create-collection (`PUT /collections/{c}`), upsert-points
//! (`PUT /collections/{c}/points`), and search (`POST /collections/{c}/points/search`).
//!
//! Embeddings are produced locally by an [`Embedder`] (the [`HashEmbedder`] by
//! default), so this binding needs no separate embedding service. Swap in a
//! semantic embedder without touching the store.
//!
//! Following the `ClaudeReasoningEngine` pattern, the request-body builders and
//! the response parser are pure functions, unit-tested with no network.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

use crate::embed::Embedder;
use crate::knowledge::{
    IncidentQuery, IncidentRecord, KnowledgeError, KnowledgeStore, RelatedIncident,
};

/// Qdrant-backed [`KnowledgeStore`]. Activated by `knowledge.provider = "qdrant"`.
pub struct QdrantKnowledgeStore {
    http: reqwest::Client,
    base_url: String,
    collection: String,
    embedder: std::sync::Arc<dyn Embedder>,
    api_key: Option<String>,
}

impl QdrantKnowledgeStore {
    pub fn new(
        base_url: String,
        collection: String,
        embedder: std::sync::Arc<dyn Embedder>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            collection,
            embedder,
            api_key,
        }
    }

    /// `PUT /collections/{c}` body — cosine distance, the embedder's dimension.
    pub fn build_collection_body(dim: usize) -> serde_json::Value {
        json!({ "vectors": { "size": dim, "distance": "Cosine" } })
    }

    /// `PUT /collections/{c}/points` body. The investigation id (a UUID) is the
    /// point id, so re-recording an incident upserts rather than duplicates.
    pub fn build_upsert_body(vector: &[f32], record: &IncidentRecord) -> serde_json::Value {
        json!({
            "points": [{
                "id": record.investigation_id,
                "vector": vector,
                "payload": {
                    "investigation_id": record.investigation_id,
                    "affected_resource": record.affected_resource,
                    "trigger": record.trigger,
                    "severity": record.severity,
                    "root_cause": record.root_cause,
                    "context_line": record.context_line(),
                    "outcome": record.outcome,
                    "recorded_at": record.recorded_at,
                }
            }]
        })
    }

    /// `POST /collections/{c}/points/search` body.
    pub fn build_search_body(vector: &[f32], k: usize) -> serde_json::Value {
        json!({ "vector": vector, "limit": k, "with_payload": true })
    }

    /// Parse a Qdrant search response into [`RelatedIncident`]s. Pure.
    pub fn parse_search_response(body: &str) -> Result<Vec<RelatedIncident>, KnowledgeError> {
        let parsed: SearchResponse =
            serde_json::from_str(body).map_err(|e| KnowledgeError::Parse(e.to_string()))?;
        Ok(parsed
            .result
            .into_iter()
            .map(|p| RelatedIncident {
                investigation_id: p.payload.investigation_id.unwrap_or_default(),
                score: p.score,
                summary: p.payload.context_line.unwrap_or_default(),
                outcome: p.payload.outcome,
            })
            .collect())
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.header("api-key", k),
            None => req,
        }
    }
}

#[async_trait]
impl KnowledgeStore for QdrantKnowledgeStore {
    fn name(&self) -> &str {
        "qdrant"
    }

    async fn ensure_ready(&self) -> Result<(), KnowledgeError> {
        // Idempotent create: Qdrant returns 200 with `{"result": true}` on
        // create and 409 if it already exists — both are fine, so we only treat
        // transport errors as failures and swallow a 409.
        let url = format!("{}/collections/{}", self.base_url, self.collection);
        let resp = self
            .auth(self.http.put(&url))
            .json(&Self::build_collection_body(self.embedder.dim()))
            .send()
            .await
            .map_err(|e| KnowledgeError::Http(e.to_string()))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            debug!(collection = %self.collection, "Qdrant collection ready");
            Ok(())
        } else {
            Err(KnowledgeError::Provider {
                status: status.as_u16(),
                message: resp.text().await.unwrap_or_default(),
            })
        }
    }

    async fn record(&self, incident: &IncidentRecord) -> Result<(), KnowledgeError> {
        let vector = self.embedder.embed(&incident.embedding_text());
        let url = format!(
            "{}/collections/{}/points?wait=true",
            self.base_url, self.collection
        );
        let resp = self
            .auth(self.http.put(&url))
            .json(&Self::build_upsert_body(&vector, incident))
            .send()
            .await
            .map_err(|e| KnowledgeError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(KnowledgeError::Provider {
                status: status.as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(())
    }

    async fn search(
        &self,
        query: &IncidentQuery,
        k: usize,
    ) -> Result<Vec<RelatedIncident>, KnowledgeError> {
        let vector = self.embedder.embed(&query.embedding_text());
        let url = format!(
            "{}/collections/{}/points/search",
            self.base_url, self.collection
        );
        let resp = self
            .auth(self.http.post(&url))
            .json(&Self::build_search_body(&vector, k))
            .send()
            .await
            .map_err(|e| KnowledgeError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(KnowledgeError::Provider {
                status: status.as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        let body = resp
            .text()
            .await
            .map_err(|e| KnowledgeError::Http(e.to_string()))?;
        Self::parse_search_response(&body)
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    result: Vec<ScoredPoint>,
}

#[derive(Deserialize)]
struct ScoredPoint {
    score: f32,
    #[serde(default)]
    payload: Payload,
}

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(default)]
    investigation_id: Option<String>,
    #[serde(default)]
    context_line: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> IncidentRecord {
        IncidentRecord {
            investigation_id: "abc-123".to_string(),
            affected_resource: "payments-api".to_string(),
            trigger: "High Error Rate".to_string(),
            severity: "Sev1".to_string(),
            root_cause: "deploy regression".to_string(),
            summary: "500s after deploy".to_string(),
            finding_categories: vec!["ErrorSpike".to_string()],
            remediation: Some("RestartWorkload".to_string()),
            outcome: Some("success".to_string()),
            recorded_at: Some("2026-06-20T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn collection_body_uses_cosine_and_dim() {
        let body = QdrantKnowledgeStore::build_collection_body(256);
        assert_eq!(body["vectors"]["size"], 256);
        assert_eq!(body["vectors"]["distance"], "Cosine");
    }

    #[test]
    fn upsert_body_uses_investigation_id_as_point_id() {
        let body = QdrantKnowledgeStore::build_upsert_body(&[0.1, 0.2, 0.3], &record());
        assert_eq!(body["points"][0]["id"], "abc-123");
        // f32 widens to f64 in JSON, so check the vector length rather than an
        // exact float value.
        assert_eq!(body["points"][0]["vector"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["points"][0]["payload"]["affected_resource"],
            "payments-api"
        );
        assert_eq!(body["points"][0]["payload"]["outcome"], "success");
        assert!(body["points"][0]["payload"]["context_line"]
            .as_str()
            .unwrap()
            .contains("RestartWorkload"));
    }

    #[test]
    fn search_body_has_vector_limit_and_payload() {
        let body = QdrantKnowledgeStore::build_search_body(&[0.5, 0.5], 4);
        assert_eq!(body["limit"], 4);
        assert_eq!(body["with_payload"], true);
        assert_eq!(body["vector"][0], 0.5);
    }

    #[test]
    fn parse_search_response_maps_payload_fields() {
        let body = r#"{"result":[
            {"id":"abc-123","score":0.91,"payload":{
                "investigation_id":"abc-123","context_line":"[Sev1] High Error Rate on payments-api",
                "outcome":"success"}},
            {"id":"def-456","score":0.42,"payload":{
                "investigation_id":"def-456","context_line":"other","outcome":"failure"}}
        ],"status":"ok","time":0.001}"#;
        let hits = QdrantKnowledgeStore::parse_search_response(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].investigation_id, "abc-123");
        assert_eq!(hits[0].score, 0.91);
        assert!(hits[0].summary.contains("payments-api"));
        assert_eq!(hits[1].outcome.as_deref(), Some("failure"));
    }

    #[test]
    fn parse_search_response_tolerates_empty_result() {
        let hits = QdrantKnowledgeStore::parse_search_response(r#"{"result":[]}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_search_response_errors_on_garbage() {
        assert!(QdrantKnowledgeStore::parse_search_response("not json").is_err());
    }
}
