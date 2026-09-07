//! Per-query instrumentation: stage timings and candidate counts.
//!
//! Surfaced three ways: `repo search -v` prints it, every query appends it to
//! the metrics log (see [`crate::observe`]), and eval reports aggregate it.
//! `Option` fields are absent in BM25-only runs where the stage didn't run.

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchTrace {
    /// `"bm25"` or `"semantic"` — which pipeline produced the hits.
    pub mode: &'static str,
    /// Whole query, end to end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_ms: Option<u64>,
    /// Query embedding request (semantic only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_ms: Option<u64>,
    /// Cosine scan over stored vectors (semantic only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_ms: Option<u64>,
    /// Cross-encoder rerank request (semantic only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_candidates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_candidates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fused_candidates: Option<usize>,
    /// Candidates the reranker scored above the confidence threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_survivors: Option<usize>,
    /// Output slots filled from the RRF fused order because the reranker
    /// rejected the candidate (score ≤ threshold) — the recall safety net.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_filled: Option<usize>,
    /// True when the reranker rejected every candidate and the output is
    /// entirely the RRF fused ranking.
    #[serde(default)]
    pub fallback: bool,
}

impl SearchTrace {
    /// One-line human summary (`repo search -v`, debugging).
    pub fn summary(&self) -> String {
        let mut stages = vec![format!("bm25 {}ms", self.bm25_ms.unwrap_or(0))];
        if let Some(ms) = self.embed_ms {
            stages.push(format!("embed {ms}ms"));
        }
        if let Some(ms) = self.dense_ms {
            stages.push(format!("dense {ms}ms"));
        }
        if let Some(ms) = self.rerank_ms {
            stages.push(format!("rerank {ms}ms"));
        }
        let mut s = format!(
            "{} in {}ms: {}",
            self.mode,
            self.total_ms.unwrap_or(0),
            stages.join(" + ")
        );
        if let (Some(b), Some(d), Some(f)) = (
            self.bm25_candidates,
            self.dense_candidates,
            self.fused_candidates,
        ) {
            s.push_str(&format!("; candidates {b}+{d}→{f}"));
        }
        if let Some(survivors) = self.rerank_survivors {
            s.push_str(&format!(", rerank survivors {survivors}"));
        }
        if let Some(filled) = self.rerank_filled {
            if filled > 0 {
                s.push_str(&format!(", {filled} backfilled from fusion"));
            }
        }
        if self.fallback {
            s.push_str(" [rerank fallback → fused order]");
        }
        s
    }
}
