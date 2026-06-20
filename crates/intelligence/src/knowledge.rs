//! Knowledge store (Phase 3) — a vector memory of past incidents.
//!
//! Closes the `related_incidents` RAG seam in [`crate::ReasoningInput`]: before
//! synthesizing an RCA the orchestrator retrieves the most similar past
//! incidents and feeds their summaries to the reasoning engine as grounding;
//! after an investigation the learning loop embeds the new incident here.
//!
//! Same trait + mock + real-binding shape as the rest of the platform:
//! [`KnowledgeStore`] with a deterministic [`MockKnowledgeStore`] (in-memory
//! cosine search, CI/dev) and a real [`crate::QdrantKnowledgeStore`] selected by
//! config.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sre_common::config::KnowledgeConfig;

use crate::embed::{cosine, Embedder, HashEmbedder};

/// A past incident, embedded and retrievable. Built from a completed
/// investigation (plus, when available, the remediation taken and its outcome).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRecord {
    pub investigation_id: String,
    pub affected_resource: String,
    pub trigger: String,
    pub severity: String,
    /// Synthesized root cause (LLM or rule-based).
    pub root_cause: String,
    /// Human-readable summary / narrative.
    pub summary: String,
    /// Finding category names, e.g. `["ErrorSpike", "DeploymentCorrelation"]`.
    pub finding_categories: Vec<String>,
    /// Remediation taken, if any (free-form, e.g. `"RestartWorkload"`).
    pub remediation: Option<String>,
    /// `"success"` | `"failure"` | `"inconclusive"` — set by the learning loop.
    pub outcome: Option<String>,
    /// RFC3339 timestamp; optional (callers stamp it — the workflow layer has the
    /// clock, this crate stays clock-free for deterministic tests).
    pub recorded_at: Option<String>,
}

impl IncidentRecord {
    /// The text fed to the embedder. Concatenates the salient fields so lexical
    /// overlap (resource, trigger, category, root cause) drives similarity.
    pub fn embedding_text(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.affected_resource,
            self.trigger,
            self.severity,
            self.root_cause,
            self.summary,
            self.finding_categories.join(" "),
        )
    }

    /// One-line context string surfaced to the reasoning engine as grounding.
    pub fn context_line(&self) -> String {
        format!(
            "[{}] {} on {} → {} (remediation: {}; outcome: {})",
            self.severity,
            self.trigger,
            self.affected_resource,
            self.root_cause,
            self.remediation.as_deref().unwrap_or("none"),
            self.outcome.as_deref().unwrap_or("unknown"),
        )
    }
}

/// A retrieval query — the current incident we want neighbors for.
#[derive(Debug, Clone)]
pub struct IncidentQuery {
    pub affected_resource: String,
    pub trigger: String,
    pub severity: String,
    pub summary: String,
    pub finding_categories: Vec<String>,
}

impl IncidentQuery {
    pub fn embedding_text(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.affected_resource,
            self.trigger,
            self.severity,
            self.summary,
            self.finding_categories.join(" "),
        )
    }
}

/// A retrieved past incident with its similarity score.
#[derive(Debug, Clone)]
pub struct RelatedIncident {
    pub investigation_id: String,
    pub score: f32,
    /// Pre-formatted context line for the matched incident.
    pub summary: String,
    pub outcome: Option<String>,
}

impl RelatedIncident {
    /// The grounding string fed into `ReasoningInput.related_incidents`.
    pub fn grounding_line(&self) -> String {
        format!("(similarity {:.2}) {}", self.score, self.summary)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("knowledge store HTTP error: {0}")]
    Http(String),
    #[error("knowledge store provider error: {status} {message}")]
    Provider { status: u16, message: String },
    #[error("failed to parse knowledge store response: {0}")]
    Parse(String),
}

/// A vector memory of past incidents.
///
/// Implementations are best-effort: the orchestrator treats every error here as
/// non-fatal (retrieval failure → no grounding; record failure → logged), so a
/// down knowledge store never blocks or fails an investigation.
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    /// Engine name, for logging (`"mock"` | `"qdrant"`).
    fn name(&self) -> &str;

    /// Best-effort one-time readiness (e.g. create the Qdrant collection).
    /// Default no-op for stores that need no setup (the mock).
    async fn ensure_ready(&self) -> Result<(), KnowledgeError> {
        Ok(())
    }

    /// Embed and persist an incident.
    async fn record(&self, incident: &IncidentRecord) -> Result<(), KnowledgeError>;

    /// Retrieve up to `k` most-similar past incidents, highest score first.
    async fn search(
        &self,
        query: &IncidentQuery,
        k: usize,
    ) -> Result<Vec<RelatedIncident>, KnowledgeError>;
}

/// In-memory, deterministic knowledge store. Embeds with [`HashEmbedder`] and
/// ranks by cosine similarity. No network, reproducible — the CI/dev default and
/// a genuinely functional RAG backend for demos.
pub struct MockKnowledgeStore {
    embedder: Arc<dyn Embedder>,
    points: Mutex<Vec<StoredPoint>>,
}

struct StoredPoint {
    vector: Vec<f32>,
    record: IncidentRecord,
}

impl MockKnowledgeStore {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            embedder,
            points: Mutex::new(Vec::new()),
        }
    }

    /// Number of stored incidents (test/introspection helper).
    pub fn len(&self) -> usize {
        self.points.lock().expect("knowledge store lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl KnowledgeStore for MockKnowledgeStore {
    fn name(&self) -> &str {
        "mock"
    }

    async fn record(&self, incident: &IncidentRecord) -> Result<(), KnowledgeError> {
        let vector = self.embedder.embed(&incident.embedding_text());
        // Lock is released at the end of the statement — never held across await.
        self.points
            .lock()
            .expect("knowledge store lock")
            .push(StoredPoint {
                vector,
                record: incident.clone(),
            });
        Ok(())
    }

    async fn search(
        &self,
        query: &IncidentQuery,
        k: usize,
    ) -> Result<Vec<RelatedIncident>, KnowledgeError> {
        let qv = self.embedder.embed(&query.embedding_text());
        let mut scored: Vec<RelatedIncident> = {
            let points = self.points.lock().expect("knowledge store lock");
            points
                .iter()
                .map(|p| RelatedIncident {
                    investigation_id: p.record.investigation_id.clone(),
                    score: cosine(&qv, &p.vector),
                    summary: p.record.context_line(),
                    outcome: p.record.outcome.clone(),
                })
                .collect()
        };
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }
}

/// Build the knowledge store from config (mirrors `auth_mode` / reasoning
/// selection). Returns `None` when disabled. `provider = "qdrant"` returns the
/// real Qdrant binding; anything else returns the deterministic mock so CI/dev
/// always work.
pub fn build_knowledge_store(cfg: &KnowledgeConfig) -> Option<Arc<dyn KnowledgeStore>> {
    if !cfg.enabled {
        return None;
    }

    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(cfg.vector_dim));

    match cfg.provider.as_str() {
        "qdrant" => {
            info!(url = %cfg.url, collection = %cfg.collection, "Knowledge store: Qdrant");
            Some(Arc::new(crate::qdrant::QdrantKnowledgeStore::new(
                cfg.url.clone(),
                cfg.collection.clone(),
                embedder,
                resolve_api_key(cfg),
            )))
        }
        other => {
            if other != "mock" {
                warn!(provider = %other, "Unknown knowledge store provider — using mock");
            } else {
                info!("Knowledge store: deterministic in-memory mock");
            }
            Some(Arc::new(MockKnowledgeStore::new(embedder)))
        }
    }
}

/// Read the Qdrant API key from the configured env var, if set + non-empty.
fn resolve_api_key(cfg: &KnowledgeConfig) -> Option<String> {
    let var = cfg.api_key_env.as_ref()?;
    match std::env::var(var) {
        Ok(k) if !k.is_empty() => Some(k),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::hash_embedder;

    fn record(id: &str, resource: &str, trigger: &str, root_cause: &str) -> IncidentRecord {
        IncidentRecord {
            investigation_id: id.to_string(),
            affected_resource: resource.to_string(),
            trigger: trigger.to_string(),
            severity: "Sev2".to_string(),
            root_cause: root_cause.to_string(),
            summary: format!("{trigger} on {resource}: {root_cause}"),
            finding_categories: vec!["ErrorSpike".to_string()],
            remediation: Some("RestartWorkload".to_string()),
            outcome: Some("success".to_string()),
            recorded_at: None,
        }
    }

    fn store() -> MockKnowledgeStore {
        MockKnowledgeStore::new(hash_embedder(512))
    }

    #[tokio::test]
    async fn record_then_search_returns_most_similar_first() {
        let s = store();
        s.record(&record(
            "1",
            "payments-api",
            "High Error Rate",
            "deploy regression returning 500s",
        ))
        .await
        .unwrap();
        s.record(&record(
            "2",
            "billing-cron",
            "Batch Failure",
            "nightly report job timed out on disk",
        ))
        .await
        .unwrap();
        assert_eq!(s.len(), 2);

        let query = IncidentQuery {
            affected_resource: "payments-api".to_string(),
            trigger: "High Error Rate".to_string(),
            severity: "Sev1".to_string(),
            summary: "payments-api returning 500 errors after a deploy".to_string(),
            finding_categories: vec!["ErrorSpike".to_string()],
        };
        let hits = s.search(&query, 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].investigation_id, "1",
            "the payments-api 500 incident should rank first"
        );
        assert!(hits[0].score >= hits[1].score);
        assert!(hits[0].summary.contains("payments-api"));
        assert_eq!(hits[0].outcome.as_deref(), Some("success"));
    }

    #[tokio::test]
    async fn search_respects_k_limit() {
        let s = store();
        for i in 0..5 {
            s.record(&record(
                &i.to_string(),
                "svc",
                "trigger",
                "some recurring cause",
            ))
            .await
            .unwrap();
        }
        let query = IncidentQuery {
            affected_resource: "svc".to_string(),
            trigger: "trigger".to_string(),
            severity: "Sev3".to_string(),
            summary: "cause".to_string(),
            finding_categories: vec![],
        };
        let hits = s.search(&query, 3).await.unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn empty_store_returns_no_hits() {
        let hits = store()
            .search(
                &IncidentQuery {
                    affected_resource: "x".to_string(),
                    trigger: "y".to_string(),
                    severity: "Sev4".to_string(),
                    summary: "z".to_string(),
                    finding_categories: vec![],
                },
                5,
            )
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn build_returns_none_when_disabled() {
        let cfg = KnowledgeConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(build_knowledge_store(&cfg).is_none());
    }

    #[test]
    fn build_returns_mock_by_default_when_enabled() {
        let cfg = KnowledgeConfig {
            enabled: true,
            ..Default::default()
        };
        let store = build_knowledge_store(&cfg).expect("store");
        assert_eq!(store.name(), "mock");
    }

    #[test]
    fn context_line_includes_outcome_and_remediation() {
        let line =
            record("1", "payments-api", "High Error Rate", "deploy regression").context_line();
        assert!(line.contains("payments-api"));
        assert!(line.contains("RestartWorkload"));
        assert!(line.contains("success"));
    }
}
