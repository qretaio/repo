//! Ranked code search (Phase 3).
//!
//! Inspired by [shebe](https://gitlab.com/shebe-oss/shebe) (BM25 ranking) and
//! [minni](https://codeberg.org/drangus/minni) (code-aware tokenization), built
//! natively on Tantivy. Public surface: [`index::build`] / [`index::is_stale`]
//! / [`index::search`].
//!
//! Pipeline: [`discovery::source_files`] (ignore-respecting walk) →
//! definition-aligned chunks ([`chunker::chunks_for`]) → expand identifiers
//! into sub-tokens ([`tokenizer`]) → Tantivy BM25 index → ranked query with
//! lang/path clauses baked into the query. Indexing is incremental: the
//! manifest's mtime map is diffed and only the delta is re-chunked.
//!
//! Phase 5 extends this with a 3-stage hybrid retrieval (BM25 ∪ dense → RRF
//! fusion → cross-encoder rerank) via local llama.cpp. Integrated into the
//! search flow: when semantic is enabled (`semantic.enabled: true` in
//! repo.yaml), the search command uses the hybrid search. Use `--bm25` to
//! force pure BM25 regardless of config.

pub mod chunker;
pub mod discovery;
pub mod index;
pub mod semantic;
pub mod tokenizer;
pub mod trace;

use std::path::Path;

use anyhow::Result;

use crate::observe;
pub use trace::SearchTrace;

/// One executed query: hits plus its instrumentation trace.
pub struct QueryOutcome {
    pub hits: Vec<index::Hit>,
    pub trace: SearchTrace,
}

/// Everything a query needs beyond settings — who asked and with what filters.
pub struct QueryRequest<'a> {
    pub root: &'a Path,
    pub query: &'a str,
    pub limit: usize,
    pub lang: Option<&'a str>,
    pub path_filter: Option<&'a str>,
    /// Skip the semantic pipeline even when enabled.
    pub force_bm25: bool,
    /// Marker for the telemetry event: "cli", "mcp", or "eval".
    pub source: &'a str,
}

/// Run a query in the configured mode (or forced BM25), record a telemetry
/// event, and return hits + trace. The single entry point shared by the CLI,
/// the MCP server, and `repo eval`.
pub fn run_query(
    req: QueryRequest<'_>,
    settings: &semantic::SemanticSettings,
) -> Result<QueryOutcome> {
    let QueryRequest {
        root,
        query,
        limit,
        lang,
        path_filter,
        force_bm25,
        source,
    } = req;
    let t0 = std::time::Instant::now();
    // A disabled-settings clone routes through semantic::search's pure-BM25
    // path so both modes return uniformly traced outcomes.
    let effective = if settings.enabled && !force_bm25 {
        settings.clone()
    } else {
        semantic::SemanticSettings {
            enabled: false,
            ..settings.clone()
        }
    };
    let result = semantic::search(root, query, &effective, limit, lang, path_filter);

    match result {
        Ok((hits, mut trace)) => {
            trace.total_ms = Some(t0.elapsed().as_millis() as u64);
            observe::append_event(
                root,
                "search",
                serde_json::json!({
                    "source": source,
                    "mode": trace.mode,
                    "query": query,
                    "limit": limit,
                    "lang": lang,
                    "path": path_filter,
                    "total_ms": trace.total_ms,
                    "bm25_ms": trace.bm25_ms,
                    "embed_ms": trace.embed_ms,
                    "dense_ms": trace.dense_ms,
                    "rerank_ms": trace.rerank_ms,
                    "bm25_candidates": trace.bm25_candidates,
                    "dense_candidates": trace.dense_candidates,
                    "fused_candidates": trace.fused_candidates,
                    "rerank_survivors": trace.rerank_survivors,
                    "rerank_filled": trace.rerank_filled,
                    "fallback": trace.fallback,
                    "results": hits.len(),
                    "top_score": hits.first().map(|h| h.score),
                }),
            );
            Ok(QueryOutcome { hits, trace })
        }
        Err(e) => {
            observe::append_event(
                root,
                "search",
                serde_json::json!({
                    "source": source,
                    "mode": effective.mode_name(),
                    "query": query,
                    "limit": limit,
                    "lang": lang,
                    "path": path_filter,
                    "error": format!("{e:#}"),
                }),
            );
            Err(e)
        }
    }
}
