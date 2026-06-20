//! Text → vector embeddings for the knowledge store.
//!
//! The platform's offline-first bar (CI, dev without a key, deterministic tests)
//! applies here too. [`HashEmbedder`] is a dependency-free, fully deterministic
//! *lexical* embedding (the hashing trick): it turns an incident's text into a
//! fixed-dimension L2-normalized vector whose cosine similarity captures term
//! overlap. It is not a semantic model — a real embedding provider (e.g. Azure
//! OpenAI `text-embedding-3-small`) is a drop-in [`Embedder`] impl — but it gives
//! the mock knowledge store genuine, reproducible retrieval with zero external
//! dependencies, and lets the Qdrant binding run without an embedding service.

use std::sync::Arc;

/// Turns text into a fixed-dimension embedding vector. Implementations MUST be
/// deterministic for a given input (the mock knowledge store relies on it for
/// reproducible retrieval).
pub trait Embedder: Send + Sync {
    /// Embedding dimensionality.
    fn dim(&self) -> usize;
    /// Embed `text` into a `dim()`-length vector.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Deterministic lexical embedder using the feature-hashing trick. Each token is
/// hashed (FNV-1a, stable across platforms and Rust versions — unlike the std
/// `DefaultHasher`) into a bucket with a sign, the buckets are accumulated, and
/// the result is L2-normalized so cosine similarity reduces to a dot product.
#[derive(Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for token in tokenize(text) {
            let h = fnv1a(token.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            // Top bit picks the sign so colliding tokens don't only ever add.
            let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        l2_normalize(&mut v);
        v
    }
}

/// Cosine similarity in `[-1, 1]` (0 for a zero-length input). Robust to vectors
/// that are not pre-normalized.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Convenience: a shared `HashEmbedder` of the given dimension.
pub fn hash_embedder(dim: usize) -> Arc<dyn Embedder> {
    Arc::new(HashEmbedder::new(dim))
}

/// Lowercase alphanumeric tokens of length ≥ 3 (drops noise words like "to",
/// "of", "a" and punctuation).
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// FNV-1a 64-bit — small, fast, and stable forever (no std-hasher drift).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_has_fixed_dim_and_is_normalized() {
        let e = HashEmbedder::new(128);
        let v = e.embed("payments-api error spike 500s");
        assert_eq!(v.len(), 128);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn embedding_is_deterministic() {
        let e = HashEmbedder::default();
        assert_eq!(e.embed("database timeout"), e.embed("database timeout"));
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let e = HashEmbedder::new(512);
        let query = e.embed("payments-api error spike 500 internal server error");
        let similar = e.embed("payments-api returning 500 internal server error spike");
        let unrelated = e.embed("nightly batch report generation completed successfully");
        let s_sim = cosine(&query, &similar);
        let s_unrel = cosine(&query, &unrelated);
        assert!(
            s_sim > s_unrel,
            "similar={s_sim} should beat unrelated={s_unrel}"
        );
        assert!(s_sim > 0.4, "similar score unexpectedly low: {s_sim}");
    }

    #[test]
    fn empty_text_yields_zero_vector_and_zero_similarity() {
        let e = HashEmbedder::new(64);
        let empty = e.embed("");
        let other = e.embed("something");
        assert_eq!(cosine(&empty, &other), 0.0);
    }

    #[test]
    fn cosine_of_identical_normalized_vectors_is_one() {
        let e = HashEmbedder::new(256);
        let v = e.embed("disk pressure on node aks-prod-3");
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }
}
