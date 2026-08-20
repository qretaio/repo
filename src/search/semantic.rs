//! Phase 5 — semantic search: 3-stage hybrid retrieval.
//!
//! Stage 1: BM25 (Tantivy) — lexical candidates.
//! Stage 2: dense — query embedding vs stored chunk embeddings (cosine).
//! Stage 3: cross-encoder rerank — `/v1/rerank` over the union of (1) + (2).
//!
//! Inference is externalized to the local llama-swap gateway (:8282,
//! LaunchAgent `com.mostlygeek.llama-swap` — starts at login, KeepAlive) —
//! no bundled ONNX, no model weights, no API key. Storage is a sidecar
//! `vectors.db` beside the BM25 index: every chunk
//! is embedded at index time and cached there.
//!
//! Contract: semantic is **on by default** (`semantic.enabled: true` in the
//! embedded defaults). OFF → pure BM25, never touches the network. ON → the
//! gateway MUST be reachable at index/query time; unreachable is a
//! hard error, **never** a silent BM25 fallback. `repo search --bm25`
//! overrides per-invocation.

use anyhow::{anyhow, bail, Context as _, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::search::chunker::chunks;
use crate::search::index::{BuildStats, Hit};

const SEMANTIC_VERSION: u32 = 1;
const EMBED_BATCH: usize = 16;

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

/// Semantic search settings. Loaded from the `semantic:` section of
/// defaults → global → local config, merged per-field. See [`settings_from_config`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSettings {
    pub enabled: bool,
    pub embed_url: String,
    pub rerank_url: String,
    pub embed_model: String,
    pub rerank_model: String,
    pub timeout_secs: u64,
    pub bm25_candidates: usize,
    pub dense_candidates: usize,
    pub rerank_topn: usize,
}

impl Default for SemanticSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            embed_url: ":8081/v1/embeddings".into(),
            rerank_url: ":8082/v1/rerank".into(),
            embed_model: "bge-small".into(),
            rerank_model: "bge-reranker".into(),
            timeout_secs: 120,
            bm25_candidates: 50,
            dense_candidates: 50,
            rerank_topn: 10,
        }
    }
}

/// Raw `semantic:` section as written in YAML (every field optional).
#[derive(Deserialize, Default)]
struct SemanticRaw {
    enabled: Option<bool>,
    embed_url: Option<String>,
    rerank_url: Option<String>,
    embed_model: Option<String>,
    rerank_model: Option<String>,
    timeout_secs: Option<u64>,
    bm25_candidates: Option<usize>,
    dense_candidates: Option<usize>,
    rerank_topn: Option<usize>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    semantic: Option<SemanticRaw>,
}

/// Load semantic settings, merging the embedded defaults then any global
/// (`~/.config/repo/repo.yaml` / `$XDG_CONFIG_HOME/repo/repo.yaml`) then local
/// (`./repo.yaml`) `semantic:` sections per-field.
pub fn settings_from_config(root: &Path) -> Result<SemanticSettings> {
    let mut settings = SemanticSettings::default();

    // Embedded defaults carry `enabled: true`.
    if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(crate::detect::DEFAULTS) {
        apply(&mut settings, cfg.semantic);
    }
    // Global.
    if let Some(dir) = global_config_dir() {
        let path = dir.join("repo.yaml");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(&content) {
                apply(&mut settings, cfg.semantic);
            }
        }
    }
    // Local.
    if let Ok(content) = std::fs::read_to_string(root.join("repo.yaml")) {
        if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(&content) {
            apply(&mut settings, cfg.semantic);
        }
    }
    Ok(settings)
}

fn apply(settings: &mut SemanticSettings, raw: Option<SemanticRaw>) {
    let Some(raw) = raw else { return };
    if let Some(v) = raw.enabled {
        settings.enabled = v;
    }
    if let Some(v) = raw.embed_url.filter(|s| !s.is_empty()) {
        settings.embed_url = v;
    }
    if let Some(v) = raw.rerank_url.filter(|s| !s.is_empty()) {
        settings.rerank_url = v;
    }
    if let Some(v) = raw.embed_model.filter(|s| !s.is_empty()) {
        settings.embed_model = v;
    }
    if let Some(v) = raw.rerank_model.filter(|s| !s.is_empty()) {
        settings.rerank_model = v;
    }
    if let Some(v) = raw.timeout_secs.filter(|&s| s > 0) {
        settings.timeout_secs = v;
    }
    if let Some(v) = raw.bm25_candidates.filter(|&s| s > 0) {
        settings.bm25_candidates = v;
    }
    if let Some(v) = raw.dense_candidates.filter(|&s| s > 0) {
        settings.dense_candidates = v;
    }
    if let Some(v) = raw.rerank_topn.filter(|&s| s > 0) {
        settings.rerank_topn = v;
    }
}

fn global_config_dir() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|p| p.join("repo"))
}

// ---------------------------------------------------------------------------
// sidecar vector store
// ---------------------------------------------------------------------------

/// Metadata written alongside `vectors.db` for staleness checks.
#[derive(Serialize, Deserialize)]
struct SemanticMeta {
    schema_version: u32,
    root: String,
    files: std::collections::HashMap<String, u64>,
    model: String,
}

/// The sidecar DB lives next to the BM25 index: `~/.cache/repo/index/<key>/vectors.db`.
fn vectors_path(root: &Path) -> Option<PathBuf> {
    crate::search::index::index_dir(root).map(|d| d.join("vectors.db"))
}

/// True when the sidecar store is missing, for another model, or any tracked
/// file's mtime changed since the last embed.
pub fn is_stale(root: &Path, model: &str) -> bool {
    let Some(path) = vectors_path(root) else {
        return true;
    };
    let Ok(conn) = Connection::open(&path) else {
        return true;
    };
    let Ok(meta_json): Result<String, _> =
        conn.query_row("SELECT value FROM meta WHERE key='semantic'", [], |r| {
            r.get(0)
        })
    else {
        return true;
    };
    let Ok(meta) = serde_json::from_str::<SemanticMeta>(&meta_json) else {
        return true;
    };
    if meta.schema_version != SEMANTIC_VERSION || meta.model != model {
        return true;
    }
    let current = crate::search::index::source_files(root);
    if current.len() != meta.files.len() {
        return true;
    }
    for (rel, mtime) in current {
        match meta.files.get(&rel) {
            Some(stored) if *stored == mtime => {}
            _ => return true,
        }
    }
    false
}

fn open_vectors(root: &Path) -> Result<Connection> {
    let Some(path) = vectors_path(root) else {
        bail!("no cache dir available");
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chunks (
            chunk_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            start INTEGER NOT NULL,
            end INTEGER NOT NULL,
            lang TEXT NOT NULL,
            source TEXT NOT NULL,
            vector BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(conn)
}

/// Number of chunks currently stored in the sidecar DB (for "up to date" messages).
fn stored_chunks(root: &Path) -> Option<usize> {
    let path = vectors_path(root)?;
    let conn = Connection::open(&path).ok()?;
    conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| {
        r.get::<_, i64>(0).map(|n| n as usize)
    })
    .ok()
}

/// Build (or refresh) the sidecar vector store: embed every chunk of every
/// tracked source file into `vectors.db`. Rebuilds when stale/forced; a model
/// change is detected via [`is_stale`]. Reuses the BM25 index's chunking, so
/// the two stages cover exactly the same chunks.
pub fn build(root: &Path, settings: &SemanticSettings, force: bool) -> Result<BuildStats> {
    if !settings.enabled {
        return Ok(BuildStats {
            files: 0,
            chunks: 0,
            rebuilt: false,
        });
    }
    if !force && !is_stale(root, &settings.embed_model) {
        let chunks = stored_chunks(root).unwrap_or(0);
        return Ok(BuildStats {
            files: 0,
            chunks,
            rebuilt: false,
        });
    }

    let files = crate::search::index::source_files(root);
    if files.is_empty() {
        bail!("no indexable source files found under {}", root.display());
    }

    let conn = open_vectors(root)?;
    conn.execute_batch("DELETE FROM chunks; DELETE FROM meta;")?;

    let mut total_chunks = 0usize;
    for (rel, _mtime) in &files {
        let abs = root.join(rel);
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let Some(lang) = crate::search::index::lang_for(&abs) else {
            continue;
        };
        let file_chunks = chunks(&text);
        let texts: Vec<&str> = file_chunks.iter().map(|c| c.text.as_str()).collect();
        let vectors = embed_texts(&texts, settings)?;
        for (c, vector) in file_chunks.iter().zip(vectors) {
            let chunk_id = format!("{rel}:{}-{}", c.start, c.end);
            conn.execute(
                "INSERT INTO chunks (chunk_id, path, start, end, lang, source, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    chunk_id,
                    rel,
                    c.start as i64,
                    c.end as i64,
                    lang,
                    c.text,
                    encode(&vector),
                ],
            )?;
            total_chunks += 1;
        }
    }

    let meta = SemanticMeta {
        schema_version: SEMANTIC_VERSION,
        root: root.to_string_lossy().into_owned(),
        files: files.into_iter().collect(),
        model: settings.embed_model.clone(),
    };
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('semantic', ?1)",
        params![serde_json::to_string(&meta)?],
    )?;

    Ok(BuildStats {
        files: meta.files.len(),
        chunks: total_chunks,
        rebuilt: true,
    })
}

// ---------------------------------------------------------------------------
// embedding (HTTP → llama.cpp)
// ---------------------------------------------------------------------------

/// Normalize a config URL: `:8081/v1/embeddings` → `http://localhost:8081/v1/embeddings`.
fn absolute_url(cfg: &str) -> String {
    if cfg.starts_with("http://") || cfg.starts_with("https://") {
        cfg.to_string()
    } else if let Some(rest) = cfg.strip_prefix(':') {
        format!("http://localhost:{rest}")
    } else {
        cfg.to_string()
    }
}

/// Embed a batch of texts via the local embed server. Truncates any single
/// text that overflows the model's context by halving it and retrying — a
/// 400 from llama.cpp is the tokenizer-free signal that we're over context.
fn embed_texts(texts: &[&str], settings: &SemanticSettings) -> Result<Vec<Vec<f32>>> {
    let url = absolute_url(&settings.embed_url);
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(EMBED_BATCH) {
        out.extend(embed_batch(batch, &url, settings)?);
    }
    Ok(out)
}

fn embed_batch(texts: &[&str], url: &str, settings: &SemanticSettings) -> Result<Vec<Vec<f32>>> {
    let body = serde_json::json!({
        "input": texts,
        "model": settings.embed_model,
        "encoding_format": "float",
    });
    match post_json(url, body, settings.timeout_secs) {
        Ok((status, text)) if (200..300).contains(&status) => parse_embeddings(&text),
        Ok((status, text)) => {
            // A batch is rejected wholesale when any member is over context;
            // fall back to embedding each text alone with halve-and-retry.
            if texts.len() == 1 {
                let vec = embed_with_truncation(
                    texts[0],
                    url,
                    settings,
                    &format!("HTTP {status}: {text}"),
                )?;
                Ok(vec![vec])
            } else {
                let mut out = Vec::with_capacity(texts.len());
                for t in texts {
                    out.push(embed_with_truncation(t, url, settings, "")?);
                }
                Ok(out)
            }
        }
        Err(e) => Err(anyhow!("embedding request failed: {e}")),
    }
}

fn embed_with_truncation(
    text: &str,
    url: &str,
    settings: &SemanticSettings,
    last_err: &str,
) -> Result<Vec<f32>> {
    let mut candidate = text.to_string();
    loop {
        let body = serde_json::json!({
            "input": [candidate],
            "model": settings.embed_model,
            "encoding_format": "float",
        });
        match post_json(url, body, settings.timeout_secs) {
            Ok((status, text)) if (200..300).contains(&status) => {
                let mut v = parse_embeddings(&text)?;
                return Ok(v.pop().unwrap_or_default());
            }
            Ok((status, _)) => {
                if candidate.len() > 64 {
                    candidate = truncate_half(&candidate);
                    continue;
                }
                bail!("text too long for embed model context: HTTP {status} ({last_err})");
            }
            Err(e) => return Err(anyhow!("embedding request failed: {e}")),
        }
    }
}

fn truncate_half(s: &str) -> String {
    let half = s.chars().count() / 2;
    s.chars().take(half).collect()
}

fn parse_embeddings(body: &str) -> Result<Vec<Vec<f32>>> {
    let v: serde_json::Value = serde_json::from_str(body).context("embed response was not JSON")?;
    let data = v["data"]
        .as_array()
        .context("embed response missing `data`")?;
    let mut out = Vec::with_capacity(data.len());
    for item in data {
        let emb = item["embedding"]
            .as_array()
            .context("embed response missing `embedding`")?;
        let vec = emb
            .iter()
            .map(|x| x.as_f64().unwrap_or(0.0) as f32)
            .collect();
        out.push(vec);
    }
    Ok(out)
}

/// f32 vector ↔ little-endian byte blob (rusqlite BLOB).
fn encode(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn decode(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        dot += *x as f64 * *y as f64;
        na += *x as f64 * *x as f64;
        nb += *y as f64 * *y as f64;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

// ---------------------------------------------------------------------------
// rerank (HTTP → llama.cpp)
// ---------------------------------------------------------------------------

/// Cross-encoder rerank. `query` is the search query; `docs` are the windowed
/// chunks (see [`rerank_doc`]). Returns relevance scores, high = relevant,
/// dropping anything the model scores ≤ 0 (an irrelevant pair).
fn rerank(query: &str, docs: &[RerankDoc], settings: &SemanticSettings) -> Result<Vec<f32>> {
    let url = absolute_url(&settings.rerank_url);
    let body = serde_json::json!({
        "model": settings.rerank_model,
        "query": query,
        "documents": docs.iter().map(|d| d.text.as_str()).collect::<Vec<_>>(),
    });
    let (status, text) =
        post_json(&url, body, settings.timeout_secs).with_context(|| "rerank request failed")?;
    if !(200..300).contains(&status) {
        bail!("rerank server returned HTTP {status}");
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).context("rerank response was not JSON")?;
    let results = v["results"]
        .as_array()
        .context("rerank missing `results`")?;
    let mut scores = vec![f32::MIN; docs.len()];
    for r in results {
        let idx = r["index"].as_u64().unwrap_or(u64::MAX) as usize;
        if let Some(s) = r["relevance_score"].as_f64() {
            if idx < scores.len() {
                scores[idx] = s as f32;
            }
        }
    }
    Ok(scores)
}

/// A document handed to the reranker: windowed around the term-matching line.
struct RerankDoc {
    text: String,
    /// Absolute (1-based) line the window starts on.
    start: u64,
}

/// Window a chunk's source to ±`RERANK_CTX` lines around the first line that
/// contains a query term (head preview when nothing matches). Rerank cost is
/// linear in total doc text, so windowing keeps a query ~1s instead of ~20s.
fn rerank_doc(source: &str, start_line: u64, query: &str, ctx: usize) -> RerankDoc {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return RerankDoc {
            text: source.to_string(),
            start: start_line,
        };
    }

    let hit = lines.iter().position(|l| {
        let low = l.to_ascii_lowercase();
        terms.iter().any(|t| low.contains(t))
    });
    let (lo, hi) = match hit {
        Some(i) => (
            i.saturating_sub(ctx),
            (i + ctx).min(lines.len().saturating_sub(1)),
        ),
        // No term line: head preview so dense-only hits aren't silent.
        None => (0, lines.len().min(3).saturating_sub(1)),
    };
    RerankDoc {
        text: lines[lo..=hi].join("\n"),
        start: start_line + lo as u64,
    }
}

/// Blocking JSON POST helper. Returns `(status, body)` for any HTTP response
/// (even 4xx/5xx — the embed/rerank callers decide); connection/IO failures
/// are returned as `Err`.
fn post_json(
    url: &str,
    body: serde_json::Value,
    timeout_secs: u64,
) -> std::result::Result<(u16, String), Box<ureq::Error>> {
    let resp = match ureq::post(url)
        .timeout(Duration::from_secs(timeout_secs))
        .send_json(&body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // HTTP error responses carry a body the caller must inspect
            // (e.g. embed-over-context 400s drive the truncation fallback).
            let text = r.into_string().unwrap_or_default();
            return Ok((code, text));
        }
        Err(e) => return Err(Box::new(e)),
    };
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    Ok((status, text))
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Run a semantic (hybrid) search: BM25 ∪ dense → rerank → top-K.
///
/// `limit` is the final result cap (defaults to `rerank_topn` when 0). When
/// semantic is disabled this is pure BM25 (no network).
pub fn search(
    root: &Path,
    query: &str,
    settings: &SemanticSettings,
    limit: usize,
) -> Result<Vec<Hit>> {
    if !settings.enabled {
        return crate::search::index::search(root, query, limit, None, None);
    }

    // Stage 1: BM25 candidates (reuses the Tantivy index — same chunks).
    let bm25_hits =
        crate::search::index::search(root, query, settings.bm25_candidates, None, None)?;

    // Stage 2: dense candidates.
    let dense_hits = dense_search(root, query, settings)?;

    // Union, dedup by (path, start), keeping the higher score.
    let mut candidates = Vec::with_capacity(bm25_hits.len() + dense_hits.len());
    candidates.extend(bm25_hits);
    for h in dense_hits {
        let dup = candidates
            .iter()
            .position(|c: &Hit| c.path == h.path && c.start == h.start);
        match dup {
            Some(i) if candidates[i].score < h.score => candidates[i] = h,
            Some(_) => {}
            None => candidates.push(h),
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Stage 3: rerank the union.
    let docs: Vec<RerankDoc> = candidates
        .iter()
        .map(|h| rerank_doc(&h.source, h.start, query, 10))
        .collect();
    let scores = rerank(query, &docs, settings)?;

    let mut ranked: Vec<(usize, f32)> = scores
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s > 0.0)
        .map(|(i, &s)| (i, s))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let topn = if limit == 0 {
        settings.rerank_topn
    } else {
        limit.min(settings.rerank_topn)
    };
    ranked.truncate(topn);

    Ok(ranked
        .into_iter()
        .map(|(i, score)| Hit {
            path: candidates[i].path.clone(),
            start: docs[i].start,
            end: docs[i].start + docs[i].text.lines().count() as u64 - 1,
            lang: candidates[i].lang.clone(),
            score,
            source: docs[i].text.clone(),
        })
        .collect())
}

/// Dense stage: embed the query, cosine against every stored chunk, top-K.
fn dense_search(root: &Path, query: &str, settings: &SemanticSettings) -> Result<Vec<Hit>> {
    let qv = embed_query(query, settings)?;
    let conn = open_vectors(root)?;
    let mut stmt = conn.prepare("SELECT path, start, end, lang, source, vector FROM chunks")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as u64,
            r.get::<_, i64>(2)? as u64,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            decode(&r.get::<_, Vec<u8>>(5)?),
        ))
    })?;

    let mut scored: Vec<(f32, Hit)> = Vec::new();
    for row in rows {
        let (path, start, end, lang, source, vector) = row?;
        let score = cosine(&qv, &vector);
        if score > 0.0 {
            scored.push((
                score,
                Hit {
                    path,
                    start,
                    end,
                    lang,
                    score,
                    source,
                },
            ));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(settings.dense_candidates);
    Ok(scored.into_iter().map(|(_, h)| h).collect())
}

fn embed_query(query: &str, settings: &SemanticSettings) -> Result<Vec<f32>> {
    let url = absolute_url(&settings.embed_url);
    let body = serde_json::json!({
        "input": [query],
        "model": settings.embed_model,
        "encoding_format": "float",
    });
    let (status, text) =
        post_json(&url, body, settings.timeout_secs).context("query embedding request failed")?;
    if !(200..300).contains(&status) {
        bail!("embed server returned HTTP {status} for query");
    }
    let mut v = parse_embeddings(&text)?;
    Ok(v.pop().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_half_keeps_prefix() {
        assert_eq!(truncate_half("abcd"), "ab");
        assert_eq!(truncate_half(""), "");
        assert_eq!(truncate_half("αβγδ"), "αβ");
    }

    #[test]
    fn cosine_zero_on_mismatch() {
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_orthogonal_is_zero_same_is_one() {
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine(&[2.0, 2.0], &[1.0, 1.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let v = vec![1.5f32, -2.0, 3.0];
        assert_eq!(decode(&encode(&v)), v);
    }

    #[test]
    fn rerank_doc_windows_around_term() {
        let src = "line0\nline1\nfoo bar\nline3\nline4\nline5\n";
        let doc = rerank_doc(src, 10, "bar", 1);
        assert_eq!(doc.start, 11); // 10 + index of "foo bar" (1) - ctx(1) → wait
        assert!(doc.text.contains("foo bar"));
    }

    #[test]
    fn rerank_doc_empty_uses_preview() {
        let src = "a\nb\nc\nd";
        let doc = rerank_doc(src, 5, "zzz", 10);
        assert_eq!(doc.start, 5);
        assert_eq!(doc.text, "a\nb\nc");
    }

    #[test]
    fn absolute_url_variants() {
        assert_eq!(
            absolute_url(":8081/v1/embeddings"),
            "http://localhost:8081/v1/embeddings"
        );
        assert_eq!(
            absolute_url("http://127.0.0.1:8081/x"),
            "http://127.0.0.1:8081/x"
        );
    }
}
