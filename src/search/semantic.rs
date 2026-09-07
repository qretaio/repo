//! Phase 5 — semantic search: 3-stage hybrid retrieval with RRF fusion.
//!
//! Stage 1: BM25 (Tantivy) — lexical candidates.
//! Stage 2: dense — query embedding vs stored chunk embeddings (cosine).
//! Fusion: the two ranked lists merge via reciprocal rank fusion (RRF, k=60).
//! Raw BM25 and cosine scores aren't comparable; ranks are (the fusion lesson
//! from zvec-grep's pipeline).
//! Stage 3: cross-encoder rerank — `/v1/rerank` over the fused candidates.
//! When the reranker scores every candidate irrelevant (≤ 0) the fused
//! ranking stands rather than returning nothing.
//!
//! Inference is externalized to a local llama.cpp server — defaults
//! `:8081/v1/embeddings` (model `bge-small`) and `:8082/v1/rerank` (model
//! `bge-reranker`), configurable via the `semantic:` section of repo.yaml.
//! Storage is a sidecar `vectors.db` beside the BM25 index: every chunk is
//! embedded at index time and cached there. Updates are incremental — the
//! manifest's per-file mtime map is diffed against a fresh walk and only the
//! delta is re-embedded; a full re-embed happens on `--force`, model or
//! schema changes, or when more than half the tree moved.
//!
//! Contract: semantic is **on by default** (`semantic.enabled: true` in the
//! embedded defaults). OFF → pure BM25, never touches the network. ON → the
//! server MUST be reachable at index/query time; unreachable is a hard error,
//! **never** a silent BM25 fallback. `repo search --bm25` overrides
//! per-invocation.

use anyhow::{anyhow, bail, Context as _, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::search::chunker::chunks_for;
use crate::search::discovery;
use crate::search::index::{BuildStats, Hit};
use crate::search::trace::SearchTrace;

const SEMANTIC_VERSION: u32 = 2;
const EMBED_BATCH: usize = 16;
/// Standard RRF constant: dampens the head of the ranking so a #1 from one
/// list can't drown ten strong hits from the other.
const RRF_K: f32 = 60.0;

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
    /// Rerank relevance cutoff: candidates above it are "relevant" and come
    /// first; the rest only backfill remaining slots (in fused order).
    /// Lower it to trust negative rerank scores as relevant too.
    pub rerank_threshold: f32,
}

impl SemanticSettings {
    /// Which pipeline [`search`] runs with these settings (used for
    /// telemetry on failed queries, where no trace comes back).
    pub fn mode_name(&self) -> &'static str {
        if self.enabled {
            "semantic"
        } else {
            "bm25"
        }
    }
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
            rerank_threshold: 0.0,
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
    rerank_threshold: Option<f32>,
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
    if let Some(v) = raw.rerank_threshold {
        settings.rerank_threshold = v;
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
    /// Canonical root — matches how the BM25 index keys its cache dir, so a
    /// symlinked invocation doesn't look perpetually stale.
    root: String,
    files: std::collections::HashMap<String, u64>,
    model: String,
}

/// The sidecar DB lives next to the BM25 index: `~/.cache/repo/index/<key>/vectors.db`.
fn vectors_path(root: &Path) -> Option<PathBuf> {
    crate::search::index::index_dir(root).map(|d| d.join("vectors.db"))
}

fn canonical_root(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canon.to_string_lossy().into_owned()
}

fn load_meta(conn: &Connection) -> Option<SemanticMeta> {
    let meta_json: String = conn
        .query_row("SELECT value FROM meta WHERE key='semantic'", [], |r| {
            r.get(0)
        })
        .ok()?;
    serde_json::from_str(&meta_json).ok()
}

fn write_meta(conn: &Connection, root: &Path, files: &[(String, u64)], model: &str) -> Result<()> {
    let meta = SemanticMeta {
        schema_version: SEMANTIC_VERSION,
        root: canonical_root(root),
        files: files.iter().cloned().collect(),
        model: model.to_string(),
    };
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('semantic', ?1)",
        params![serde_json::to_string(&meta)?],
    )?;
    Ok(())
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
fn stored_chunks(conn: &Connection) -> usize {
    conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| {
        r.get::<_, i64>(0).map(|n| n as usize)
    })
    .unwrap_or(0)
}

/// Build (or refresh) the sidecar vector store, recording a telemetry event
/// (see [`crate::observe`]). See [`build_impl`] for the refresh policy.
pub fn build(root: &Path, settings: &SemanticSettings, force: bool) -> Result<BuildStats> {
    let t0 = std::time::Instant::now();
    let result = build_impl(root, settings, force);
    crate::observe::record_build(root, "semantic", &result, t0.elapsed());
    result
}

/// Embed every chunk of every tracked source file into `vectors.db`.
/// Incremental when a compatible meta exists and the delta is small; full
/// re-embed when `force`, on model or schema changes, or when more than half
/// the tree moved. Reuses the BM25 index's chunking, so the two stages cover
/// exactly the same chunks.
fn build_impl(root: &Path, settings: &SemanticSettings, force: bool) -> Result<BuildStats> {
    if !settings.enabled {
        return Ok(BuildStats {
            files: 0,
            chunks: 0,
            rebuilt: false,
            incremental: false,
        });
    }
    let files = discovery::source_files(root);
    if files.is_empty() {
        bail!("no indexable source files found under {}", root.display());
    }

    let conn = open_vectors(root)?;
    let meta = load_meta(&conn);
    let compatible = meta.as_ref().is_some_and(|m| {
        m.schema_version == SEMANTIC_VERSION
            && m.model == settings.embed_model
            && m.root == canonical_root(root)
    });

    if !force && compatible {
        let meta = meta.as_ref().expect("checked above");
        let diff = discovery::diff_files(&files, &meta.files);
        if diff.is_empty() {
            return Ok(BuildStats {
                files: files.len(),
                chunks: stored_chunks(&conn),
                rebuilt: false,
                incremental: false,
            });
        }
        if diff.total() * 2 <= files.len() {
            for rel in diff.removed.iter().chain(&diff.changed) {
                conn.execute("DELETE FROM chunks WHERE path = ?1", params![rel])?;
            }
            let mut embedded = 0usize;
            for rel in diff.added.iter().chain(&diff.changed) {
                embedded += embed_file(&conn, root, rel, settings)?;
            }
            write_meta(&conn, root, &files, &settings.embed_model)?;
            return Ok(BuildStats {
                files: files.len(),
                chunks: embedded,
                rebuilt: true,
                incremental: true,
            });
        }
    }

    // Full (re)embed: no usable meta, forced, or too much of the tree moved.
    conn.execute_batch("DELETE FROM chunks; DELETE FROM meta;")?;
    let mut total_chunks = 0usize;
    for (rel, _mtime) in &files {
        total_chunks += embed_file(&conn, root, rel, settings)?;
    }
    write_meta(&conn, root, &files, &settings.embed_model)?;

    Ok(BuildStats {
        files: files.len(),
        chunks: total_chunks,
        rebuilt: true,
        incremental: false,
    })
}

/// Chunk `rel` and embed its chunks into the sidecar DB. Returns the chunk
/// count embedded.
fn embed_file(
    conn: &Connection,
    root: &Path,
    rel: &str,
    settings: &SemanticSettings,
) -> Result<usize> {
    let abs = root.join(rel);
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return Ok(0);
    };
    let Some(lang) = discovery::lang_for(&abs) else {
        return Ok(0);
    };
    let file_chunks = chunks_for(&text, &abs);
    let texts: Vec<&str> = file_chunks.iter().map(|c| c.text.as_str()).collect();
    let vectors = embed_texts(&texts, settings)?;
    for (c, vector) in file_chunks.iter().zip(vectors) {
        let chunk_id = format!("{rel}:{}-{}", c.start, c.end);
        conn.execute(
            "INSERT OR REPLACE INTO chunks (chunk_id, path, start, end, lang, source, vector)
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
    }
    Ok(file_chunks.len())
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
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
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

/// Run a semantic (hybrid) search: BM25 ∪ dense → RRF fusion → rerank → top-K.
///
/// `limit` is the final result cap (defaults to `rerank_topn` when 0). `lang`
/// and `path_filter` are applied inside both stages. When semantic is
/// disabled this is pure BM25 (no network). Returns hits plus a
/// [`SearchTrace`] with stage timings and candidate counts.
pub fn search(
    root: &Path,
    query: &str,
    settings: &SemanticSettings,
    limit: usize,
    lang: Option<&str>,
    path_filter: Option<&str>,
) -> Result<(Vec<Hit>, SearchTrace)> {
    if !settings.enabled {
        let t = std::time::Instant::now();
        let hits = crate::search::index::search(root, query, limit, lang, path_filter);
        let trace = SearchTrace {
            mode: "bm25",
            bm25_ms: Some(t.elapsed().as_millis() as u64),
            bm25_candidates: hits.as_ref().map(Vec::len).ok(),
            ..Default::default()
        };
        return hits.map(|h| (h, trace));
    }

    // Stage 1: BM25 candidates (reuses the Tantivy index — same chunks).
    let t = std::time::Instant::now();
    let bm25_hits =
        crate::search::index::search(root, query, settings.bm25_candidates, lang, path_filter)?;
    let bm25_ms = t.elapsed().as_millis() as u64;
    let bm25_candidates = bm25_hits.len();

    // Stage 2: dense candidates (query embedding + cosine scan).
    let t = std::time::Instant::now();
    let qv = embed_query(query, settings)?;
    let embed_ms = t.elapsed().as_millis() as u64;
    let t = std::time::Instant::now();
    let dense_hits = dense_scan(&qv, root, settings, lang, path_filter)?;
    let dense_ms = t.elapsed().as_millis() as u64;
    let dense_candidates = dense_hits.len();
    if bm25_hits.is_empty() && dense_hits.is_empty() {
        return Ok((
            Vec::new(),
            SearchTrace {
                mode: "semantic",
                total_ms: None,
                bm25_ms: Some(bm25_ms),
                embed_ms: Some(embed_ms),
                dense_ms: Some(dense_ms),
                bm25_candidates: Some(bm25_candidates),
                dense_candidates: Some(dense_candidates),
                ..Default::default()
            },
        ));
    }

    // Fuse the two ranked lists with reciprocal rank fusion — each list votes
    // 1/(k + rank + 1) per candidate; a candidate's fused score is its vote
    // sum. Duplicates (same chunk from both lists) pool their votes.
    let mut fused: Vec<(Hit, f32)> = Vec::with_capacity(bm25_candidates + dense_candidates);
    for (rank, hit) in bm25_hits.into_iter().enumerate() {
        rrf_vote(&mut fused, hit, rank);
    }
    for (rank, hit) in dense_hits.into_iter().enumerate() {
        rrf_vote(&mut fused, hit, rank);
    }
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let candidates: Vec<Hit> = fused.iter().map(|(h, _)| h.clone()).collect();
    let fused_candidates = candidates.len();

    // Stage 3: rerank the fused candidates.
    let docs: Vec<RerankDoc> = candidates
        .iter()
        .map(|h| rerank_doc(&h.source, h.start, query, 10))
        .collect();
    let t = std::time::Instant::now();
    let scores = rerank(query, &docs, settings)?;
    let rerank_ms = t.elapsed().as_millis() as u64;

    let topn = if limit == 0 {
        settings.rerank_topn
    } else {
        limit.min(settings.rerank_topn)
    };
    let picked = select_topn(&scores, candidates.len(), settings.rerank_threshold, topn);
    if picked.is_empty() {
        return Ok((
            Vec::new(),
            SearchTrace {
                mode: "semantic",
                total_ms: None,
                bm25_ms: Some(bm25_ms),
                embed_ms: Some(embed_ms),
                dense_ms: Some(dense_ms),
                rerank_ms: Some(rerank_ms),
                bm25_candidates: Some(bm25_candidates),
                dense_candidates: Some(dense_candidates),
                fused_candidates: Some(fused_candidates),
                rerank_survivors: Some(0),
                rerank_filled: Some(0),
                fallback: false,
            },
        ));
    }
    let survivors = scores
        .iter()
        .filter(|&&s| s > settings.rerank_threshold)
        .count();
    let filled = picked
        .iter()
        .filter(|&&i| scores[i] <= settings.rerank_threshold)
        .count();

    let hits = picked
        .into_iter()
        .map(|i| {
            // Unscored candidates (rerank response gaps default to f32::MIN)
            // surface as 0 rather than an absurd negative.
            let score = if scores[i] == f32::MIN {
                0.0
            } else {
                scores[i]
            };
            Hit {
                path: candidates[i].path.clone(),
                start: docs[i].start,
                end: docs[i].start + docs[i].text.lines().count() as u64 - 1,
                lang: candidates[i].lang.clone(),
                score,
                source: docs[i].text.clone(),
            }
        })
        .collect();
    Ok((
        hits,
        SearchTrace {
            mode: "semantic",
            total_ms: None,
            bm25_ms: Some(bm25_ms),
            embed_ms: Some(embed_ms),
            dense_ms: Some(dense_ms),
            rerank_ms: Some(rerank_ms),
            bm25_candidates: Some(bm25_candidates),
            dense_candidates: Some(dense_candidates),
            fused_candidates: Some(fused_candidates),
            rerank_survivors: Some(survivors),
            rerank_filled: Some(filled),
            fallback: survivors == 0,
        },
    ))
}

/// Choose the final result set from rerank `scores` over the fused candidate
/// pool: rerank scores above `threshold` first (descending), then backfill in
/// fused order so a strict reranker can't crater recall — its *ordering*
/// stays authoritative, but a low absolute score no longer erases a candidate
/// the BM25 and dense stages both surfaced (the 2026-09-07 eval finding).
/// Returns candidate indices in output order, capped at `topn`.
fn select_topn(scores: &[f32], len: usize, threshold: f32, topn: usize) -> Vec<usize> {
    let mut rerank_order: Vec<usize> = (0..len).collect();
    rerank_order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut picked: Vec<usize> = rerank_order
        .into_iter()
        .filter(|&i| scores[i] > threshold)
        .take(topn)
        .collect();
    // Backfill from the fused order (candidates are fused-sorted by index).
    for i in 0..len {
        if picked.len() >= topn {
            break;
        }
        if !picked.contains(&i) {
            picked.push(i);
        }
    }
    picked
}

/// Cast one list's vote for `hit` at 0-based `rank` into the fused pool,
/// keyed by chunk identity (path + start line).
fn rrf_vote(fused: &mut Vec<(Hit, f32)>, hit: Hit, rank: usize) {
    let contribution = 1.0 / (RRF_K + rank as f32 + 1.0);
    match fused
        .iter_mut()
        .find(|(h, _)| h.path == hit.path && h.start == hit.start)
    {
        Some((_, score)) => *score += contribution,
        None => fused.push((hit, contribution)),
    }
}

/// Dense stage: embed the query, cosine against every stored chunk, top-K.
/// `lang`/`path_filter` filter rows in SQL so no candidate is fetched just to
/// be discarded.
/// Dense scan: cosine the (already embedded) query vector against every
/// stored chunk, top-K. `lang`/`path_filter` filter rows in SQL so no
/// candidate is fetched just to be discarded.
fn dense_scan(
    qv: &[f32],
    root: &Path,
    settings: &SemanticSettings,
    lang: Option<&str>,
    path_filter: Option<&str>,
) -> Result<Vec<Hit>> {
    let conn = open_vectors(root)?;
    let mut sql =
        String::from("SELECT path, start, end, lang, source, vector FROM chunks WHERE 1=1");
    let mut args: Vec<String> = Vec::new();
    if let Some(want) = lang {
        sql.push_str(" AND lang = ?");
        args.push(want.to_ascii_lowercase());
    }
    if let Some(needle) = path_filter {
        sql.push_str(" AND path LIKE ? ESCAPE '\\'");
        args.push(format!("%{}%", escape_like(needle)));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
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
        let score = cosine(qv, &vector);
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

/// Escape SQL LIKE metacharacters so a path filter is a literal substring.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
        assert_eq!(doc.start, 11);
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

    fn hit(path: &str, start: u64) -> Hit {
        Hit {
            path: path.to_string(),
            start,
            end: start,
            lang: "rust".into(),
            score: 1.0,
            source: String::new(),
        }
    }

    #[test]
    fn rrf_vote_pools_duplicates_and_dampens_by_rank() {
        let mut fused: Vec<(Hit, f32)> = Vec::new();
        rrf_vote(&mut fused, hit("a.rs", 1), 0);
        // Same chunk via the other list → votes pool.
        rrf_vote(&mut fused, hit("a.rs", 1), 3);
        rrf_vote(&mut fused, hit("b.rs", 1), 1);

        let score_of = |p: &str| fused.iter().find(|(h, _)| h.path == p).unwrap().1;
        // a.rs: 1/61 (rank 0) + 1/64 (rank 3) beats b.rs: 1/62 (rank 1)…
        assert!((score_of("a.rs") - (1.0 / 61.0 + 1.0 / 64.0)).abs() < 1e-6);
        // …but not by much: RRF keeps the lists comparably weighted.
        assert!(score_of("a.rs") > score_of("b.rs"));
        assert_eq!(fused.len(), 2, "duplicates pool into one entry");
    }

    #[test]
    fn escape_like_treats_metacharacters_literally() {
        assert_eq!(escape_like("src/main.rs"), "src/main.rs");
        assert_eq!(escape_like("50%_done"), "50\\%\\_done");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn select_topn_survivors_first_then_fused_backfill() {
        let scores = [0.9, -1.0, 0.5, f32::MIN];
        // Survivors 0 (0.9) and 2 (0.5) lead in rerank order; the rejected
        // 1 and unscored 3 backfill in fused order.
        assert_eq!(select_topn(&scores, 4, 0.0, 4), vec![0, 2, 1, 3]);
    }

    #[test]
    fn select_topn_all_rejected_keeps_fused_order() {
        let scores = [-1.0, -2.0, -0.5];
        assert_eq!(select_topn(&scores, 3, 0.0, 3), vec![0, 1, 2]);
    }

    #[test]
    fn select_topn_caps_at_topn() {
        let scores = [0.9, -1.0, 0.5];
        assert_eq!(select_topn(&scores, 3, 0.0, 2), vec![0, 2]);
    }

    #[test]
    fn select_topn_negative_threshold_trusts_negatives() {
        let scores = [0.9, -1.0, -0.5];
        // With the cutoff at -2 every candidate survives, in rerank order.
        assert_eq!(select_topn(&scores, 3, -2.0, 3), vec![0, 2, 1]);
    }

    #[test]
    fn select_topn_empty_pool_yields_nothing() {
        assert!(select_topn(&[], 0, 0.0, 5).is_empty());
    }
}
