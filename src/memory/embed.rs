//! Embedding generation and its availability state.
//!
//! There is deliberately **no enable/disable setting**. Either the endpoint
//! answers or it doesn't, and the agent can tell — so availability is a *detected
//! runtime state*, never a configured one. A manual off-switch would be actively
//! harmful here: with embeddings disabled, memories still get written but get no
//! vectors, so the corpus silently splits into an embedded half and an
//! unembedded half, and vector recall then misses exactly the memories written
//! during the off window, with no error anywhere.
//!
//! What replaces the switch:
//!
//! * **Endpoint missing** — the first embed attempt fails, the state goes
//!   [`Availability::Unavailable`], and recall runs on BM25 + graph. Re-probed on
//!   a cooldown.
//! * **Transient failure** — three consecutive failures trip the breaker to
//!   [`Availability::Degraded`], and recall skips the vector leg entirely rather
//!   than paying the timeout on every turn. One success closes it.
//! * **Tests** — [`NullEmbedder`] is selected in code by the harness. Production
//!   never constructs it.
//!
//! All three self-heal, because anything missing a vector is picked up by
//! [`super::backfill_embeddings`].
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rig::client::embeddings::EmbeddingsClient;
use rig::embeddings::EmbeddingModel as _;

/// Default embedding model. `text-embedding-3-small` is natively 1536-dim but
/// supports Matryoshka truncation via the `dimensions` parameter, which rig
/// forwards.
pub const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Default dimensionality. 384 rather than the native 1536 because every vector
/// is resident in RAM: 384 × f32 is 1.5 KB per memory instead of 6 KB, which is
/// the difference between a 100k-memory store costing ~150 MB and ~600 MB.
/// Retrieval quality at 384 is close enough that the trade is worth it here.
pub const DEFAULT_DIMS: usize = 384;

/// Consecutive failures before the breaker opens.
const FAILURES_TO_TRIP: u32 = 3;
/// How long the breaker stays open before a single request is allowed through.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(60);
/// Batch size for backfill. Well under rig's `MAX_DOCUMENTS` (1024) so one
/// failure costs little.
pub const BATCH_SIZE: usize = 256;

/// What the embedding endpoint is currently doing for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Working.
    Ready,
    /// Recently failing — the vector leg of recall is skipped until the cooldown
    /// elapses and a probe succeeds.
    Degraded,
    /// Never succeeded in this process. Almost always a deployment problem: the
    /// gateway does not proxy `/embeddings`, or the base URL points at something
    /// that has no such route.
    Unavailable,
}

impl Availability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Anything that can turn text into vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    fn dims(&self) -> usize;
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String>;
}

/// The real one: OpenAI's embeddings API, reached through whatever
/// `OPENAI_BASE_URL` points at — so on a managed pod this bills through the
/// metalcraft-inference gateway like every other call.
pub struct OpenAiEmbedder {
    model_name: String,
    dims: usize,
    model: rig::providers::openai::EmbeddingModel,
}

impl OpenAiEmbedder {
    pub fn new(api_key: &str, model_name: &str, dims: usize) -> Result<Self, String> {
        let client = crate::runtime::build_openai_client(api_key)
            .map_err(|e| format!("could not build the embeddings client: {e}"))?;
        Ok(Self {
            model: client.embedding_model_with_ndims(model_name, dims),
            model_name: model_name.to_string(),
            dims,
        })
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn model(&self) -> &str {
        &self.model_name
    }
    fn dims(&self) -> usize {
        self.dims
    }
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let n = texts.len();
        let embeddings = self
            .model
            .embed_texts(texts)
            .await
            .map_err(|e| format!("embeddings request failed: {e}"))?;
        if embeddings.len() != n {
            return Err(format!("embeddings endpoint returned {} vectors for {n} inputs", embeddings.len()));
        }
        // rig hands back f64; we store f32. At cosine-similarity precision the
        // difference is far below anything that changes a ranking, and it halves
        // both the file and the resident footprint.
        Ok(embeddings
            .into_iter()
            .map(|e| e.vec.into_iter().map(|v| v as f32).collect())
            .collect())
    }
}

/// A deterministic stand-in for tests: a cheap hashed bag-of-words projection.
/// Similar text lands in a similar direction, which is enough to exercise the
/// ranking without a network call. Never used in production.
pub struct NullEmbedder {
    dims: usize,
}

impl NullEmbedder {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl Embedder for NullEmbedder {
    fn model(&self) -> &str {
        "null"
    }
    fn dims(&self) -> usize {
        self.dims
    }
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dims];
                for token in super::index::tokenize(t) {
                    let mut h: u64 = 1469598103934665603;
                    for b in token.as_bytes() {
                        h ^= *b as u64;
                        h = h.wrapping_mul(1099511628211);
                    }
                    v[(h as usize) % self.dims] += 1.0;
                }
                v
            })
            .collect())
    }
}

/// An embedder plus the breaker state around it.
pub struct Embeddings {
    embedder: Arc<dyn Embedder>,
    consecutive_failures: AtomicU32,
    /// Unix millis before which the breaker stays open. 0 = closed.
    open_until_ms: AtomicI64,
    /// Whether any call has ever succeeded in this process.
    ever_succeeded: AtomicU32,
}

impl Embeddings {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            embedder,
            consecutive_failures: AtomicU32::new(0),
            open_until_ms: AtomicI64::new(0),
            ever_succeeded: AtomicU32::new(0),
        }
    }

    pub fn model(&self) -> &str {
        self.embedder.model()
    }

    pub fn dims(&self) -> usize {
        self.embedder.dims()
    }

    pub fn availability(&self) -> Availability {
        if self.breaker_open() {
            if self.ever_succeeded.load(Ordering::Relaxed) == 0 {
                return Availability::Unavailable;
            }
            return Availability::Degraded;
        }
        Availability::Ready
    }

    fn breaker_open(&self) -> bool {
        let until = self.open_until_ms.load(Ordering::Relaxed);
        until > 0 && Utc_now_ms() < until
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.open_until_ms.store(0, Ordering::Relaxed);
        self.ever_succeeded.store(1, Ordering::Relaxed);
    }

    fn record_failure(&self, context: &str, err: &str) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= FAILURES_TO_TRIP {
            let until = Utc_now_ms() + BREAKER_COOLDOWN.as_millis() as i64;
            self.open_until_ms.store(until, Ordering::Relaxed);
            log::warn!(
                "memory: embeddings failing ({context}: {err}) — pausing vector search for {}s. \
                 Recall continues on keyword + graph search; anything written meanwhile is embedded \
                 by the next backfill.",
                BREAKER_COOLDOWN.as_secs()
            );
        } else {
            log::debug!("memory: embedding attempt failed ({context}: {err}), {n}/{FAILURES_TO_TRIP}");
        }
    }

    /// Embed one query for search. Returns `None` — never an error — when the
    /// breaker is open or the call fails or times out, because a recall that
    /// loses its vector leg should quietly become a keyword recall, not fail.
    pub async fn embed_query(&self, text: &str, timeout: Duration) -> Option<Vec<f32>> {
        if self.breaker_open() {
            return None;
        }
        match tokio::time::timeout(timeout, self.embedder.embed(vec![text.to_string()])).await {
            Ok(Ok(mut v)) if !v.is_empty() => {
                self.record_success();
                Some(v.remove(0))
            }
            Ok(Ok(_)) => {
                self.record_failure("query", "endpoint returned no vector");
                None
            }
            Ok(Err(e)) => {
                self.record_failure("query", &e);
                None
            }
            Err(_) => {
                self.record_failure("query", &format!("timed out after {}ms", timeout.as_millis()));
                None
            }
        }
    }

    /// Embed a batch for backfill. Unlike [`Self::embed_query`] this reports the
    /// error, because a backfill caller wants to know why nothing happened — and
    /// it ignores an open breaker, since backfill *is* the retry.
    pub async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        match self.embedder.embed(texts).await {
            Ok(v) => {
                self.record_success();
                Ok(v)
            }
            Err(e) => {
                self.record_failure("backfill", &e);
                Err(e)
            }
        }
    }
}

#[allow(non_snake_case)]
fn Utc_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Read the configured model name (`MEMORY_EMBED_MODEL`).
pub fn configured_model() -> String {
    std::env::var("MEMORY_EMBED_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Read the configured dimensionality (`MEMORY_EMBED_DIMS`).
pub fn configured_dims() -> usize {
    std::env::var("MEMORY_EMBED_DIMS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= u16::MAX as usize)
        .unwrap_or(DEFAULT_DIMS)
}

/// How long a query embedding may take before recall gives up on it
/// (`MEMORY_RECALL_TIMEOUT_MS`).
pub fn query_timeout() -> Duration {
    let ms = std::env::var("MEMORY_RECALL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(800);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_embedder_is_deterministic_and_shaped_right() {
        let e = NullEmbedder::new(64);
        let a = e.embed(vec!["the gateway proxies embeddings".into()]).await.unwrap();
        let b = e.embed(vec!["the gateway proxies embeddings".into()]).await.unwrap();
        assert_eq!(a[0].len(), 64);
        assert_eq!(a, b, "same input must give the same vector");
    }

    #[tokio::test]
    async fn null_embedder_puts_similar_text_in_a_similar_direction() {
        let e = NullEmbedder::new(128);
        let v = e
            .embed(vec![
                "the gateway proxies embeddings for pods".into(),
                "the gateway proxies embeddings".into(),
                "completely different subject entirely".into(),
            ])
            .await
            .unwrap();
        let close = super::super::vectors::cosine(&v[0], &v[1]);
        let far = super::super::vectors::cosine(&v[0], &v[2]);
        assert!(close > far, "overlapping text should score higher ({close} vs {far})");
    }

    #[tokio::test]
    async fn empty_batch_is_a_no_op() {
        let e = NullEmbedder::new(8);
        assert!(e.embed(vec![]).await.unwrap().is_empty());
    }

    /// An embedder that always fails, for exercising the breaker.
    struct AlwaysFails;
    #[async_trait]
    impl Embedder for AlwaysFails {
        fn model(&self) -> &str {
            "always-fails"
        }
        fn dims(&self) -> usize {
            8
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
            Err("no such endpoint".into())
        }
    }

    #[tokio::test]
    async fn a_fresh_embedder_reports_ready() {
        let e = Embeddings::new(Arc::new(NullEmbedder::new(8)));
        assert_eq!(e.availability(), Availability::Ready);
    }

    #[tokio::test]
    async fn three_failures_trip_the_breaker_to_unavailable() {
        let e = Embeddings::new(Arc::new(AlwaysFails));
        for _ in 0..2 {
            assert!(e.embed_query("x", Duration::from_secs(1)).await.is_none());
            assert_eq!(e.availability(), Availability::Ready, "not tripped yet");
        }
        assert!(e.embed_query("x", Duration::from_secs(1)).await.is_none());
        assert_eq!(
            e.availability(),
            Availability::Unavailable,
            "never succeeded, so this is a deployment problem, not a blip"
        );
        // With the breaker open the call short-circuits rather than retrying.
        assert!(e.embed_query("x", Duration::from_secs(1)).await.is_none());
    }

    #[tokio::test]
    async fn a_success_after_failures_closes_the_breaker() {
        let e = Embeddings::new(Arc::new(NullEmbedder::new(8)));
        // Drive the failure counter without an open breaker.
        e.record_failure("test", "boom");
        e.record_failure("test", "boom");
        assert_eq!(e.availability(), Availability::Ready);
        assert!(e.embed_query("hello world", Duration::from_secs(1)).await.is_some());
        assert_eq!(e.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(e.availability(), Availability::Ready);
    }

    #[tokio::test]
    async fn a_previously_working_endpoint_degrades_rather_than_reading_unavailable() {
        let e = Embeddings::new(Arc::new(NullEmbedder::new(8)));
        assert!(e.embed_query("warm it up", Duration::from_secs(1)).await.is_some());
        for _ in 0..FAILURES_TO_TRIP {
            e.record_failure("test", "boom");
        }
        assert_eq!(e.availability(), Availability::Degraded, "it worked before, so this is transient");
    }

    #[tokio::test]
    async fn backfill_surfaces_the_error_instead_of_swallowing_it() {
        let e = Embeddings::new(Arc::new(AlwaysFails));
        assert!(e.embed_batch(vec!["x".into()]).await.is_err());
    }

    #[tokio::test]
    async fn a_slow_endpoint_times_out_without_failing_the_caller() {
        struct Slow;
        #[async_trait]
        impl Embedder for Slow {
            fn model(&self) -> &str {
                "slow"
            }
            fn dims(&self) -> usize {
                8
            }
            async fn embed(&self, _t: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(vec![vec![0.0; 8]])
            }
        }
        let e = Embeddings::new(Arc::new(Slow));
        assert!(e.embed_query("x", Duration::from_millis(20)).await.is_none());
        assert_eq!(e.consecutive_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn config_defaults_are_the_documented_ones() {
        // These read the ambient env; in a clean test process they are unset.
        assert_eq!(configured_model(), DEFAULT_MODEL);
        assert_eq!(configured_dims(), DEFAULT_DIMS);
        assert_eq!(query_timeout(), Duration::from_millis(800));
    }
}
