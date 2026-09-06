use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default, Clone)]
pub struct TurnMetrics {
    pub model_calls: Arc<AtomicU64>,
    pub embedding_calls: Arc<AtomicU64>,
    pub prompt_tokens: Arc<AtomicU64>,
    pub output_tokens: Arc<AtomicU64>,
    pub thought_tokens: Arc<AtomicU64>,
    pub embedding_tokens: Arc<AtomicU64>,
    pub model_usage_reported: Arc<AtomicBool>,
    pub embedding_usage_reported: Arc<AtomicBool>,
    // 检索耗时（毫秒存为 u64）
    pub retrieval_ms: Arc<AtomicU64>,
}

impl TurnMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_model_calls(&self) {
        self.model_calls.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_embedding_calls(&self) {
        self.embedding_calls.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_retrieval_seconds(&self, seconds: f64) {
        let ms = (seconds * 1000.0).max(0.0) as u64;
        self.retrieval_ms.fetch_add(ms, Ordering::Relaxed);
    }

    pub fn record_model_usage(&self, prompt: u64, output: u64, thought: u64) {
        self.model_usage_reported.store(true, Ordering::Relaxed);
        self.prompt_tokens.fetch_add(prompt, Ordering::Relaxed);
        self.output_tokens.fetch_add(output, Ordering::Relaxed);
        self.thought_tokens.fetch_add(thought, Ordering::Relaxed);
    }

    pub fn record_embedding_usage(&self, tokens: u64) {
        self.embedding_usage_reported.store(true, Ordering::Relaxed);
        self.embedding_tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    pub fn to_public_json(&self) -> serde_json::Value {
        let retrieval_s = (self.retrieval_ms.load(Ordering::Relaxed) as f64) / 1000.0;
        serde_json::json!({
            "event": "metrics",
            "model_calls": self.model_calls.load(Ordering::Relaxed),
            "embedding_calls": self.embedding_calls.load(Ordering::Relaxed),
            "prompt_tokens": self.prompt_tokens.load(Ordering::Relaxed),
            "output_tokens": self.output_tokens.load(Ordering::Relaxed),
            "thought_tokens": self.thought_tokens.load(Ordering::Relaxed),
            "embedding_tokens": self.embedding_tokens.load(Ordering::Relaxed),
            "model_usage_reported": self.model_usage_reported.load(Ordering::Relaxed),
            "embedding_usage_reported": self.embedding_usage_reported.load(Ordering::Relaxed),
            "retrieval_s": retrieval_s
        })
    }
}
