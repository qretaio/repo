//! Tantivy-backed ranked code search — index build.
//!
//! One index per repository, stored under `~/.cache/repo/index/<fnv(root)>/`.
//! Chunks are definition-aligned where tree-sitter knows the language (see
//! [`chunker::chunks_for`]); each chunk carries the raw source for display, a
//! breadcrumb, and a pre-expanded token stream (see [`tokenizer`]) that is
//! what Tantivy actually ranks.
//!
//! Re-indexing is incremental: the manifest's per-file mtime map is diffed
//! against a fresh walk, and only added/changed/removed files are deleted and
//! re-chunked (Tantivy term deletes on the indexed `path` field). A full
//! rebuild is reserved for `--force`, schema changes, or diffs touching more
//! than half the tree — bulk deletes of that size cost more than a fresh
//! index, and a full rebuild is obviously correct.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexWriter, TantivyDocument, Term};

use super::chunker::chunks_for;
use super::discovery::{self, FileDiff};
use super::tokenizer::expand;

const SCHEMA_VERSION: u32 = 2;

/// One ranked search hit.
#[derive(Debug, Clone)]
pub struct Hit {
    pub path: String,
    pub start: u64,
    pub end: u64,
    pub lang: String,
    /// Ranking score (only comparable within one query): Tantivy BM25 in
    /// BM25 mode, reranker relevance in semantic mode.
    pub score: f32,
    pub source: String,
}

/// Outcome of an index build.
#[derive(Debug, Clone)]
pub struct BuildStats {
    pub files: usize,
    /// Chunks written this run (0 when the index was already up to date).
    pub chunks: usize,
    /// True when the on-disk index changed (full rebuild or delta).
    pub rebuilt: bool,
    /// True when only the diffed delta was re-chunked (vs a full rebuild).
    pub incremental: bool,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    built_at: u64,
    /// Canonical root — keyed the same way as [`index_dir`], so invoking
    /// through a symlinked path doesn't look perpetually stale.
    root: String,
    files: HashMap<String, u64>, // rel path -> mtime secs
}

/// Resolve the cache directory for `root`'s index. Deterministic via FNV-1a of
/// the canonical path so the same repo always maps to the same dir.
pub fn index_dir(root: &Path) -> Option<PathBuf> {
    let key = discovery_key(root);
    let base = cache_base()?;
    Some(base.join(format!("{key:016x}")))
}

fn discovery_key(root: &Path) -> u64 {
    let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    fnv1a64(&canon.to_string_lossy())
}

fn cache_base() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".cache")
                .join("repo")
                .join("index"),
        );
    }
    // No HOME (rare on the unix hosts `repo` targets): fall back to tmp.
    Some(std::env::temp_dir().join("repo-index"))
}

/// FNV-1a 64-bit — deterministic, no random seed (unlike `DefaultHasher`).
/// Shared with the symbol store and task cache for cache-dir keying.
pub(crate) fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Load the on-disk manifest, or `None` when missing/incompatible/for another
/// root (canonicalized on both sides).
fn load_manifest(root: &Path, dir: &Path) -> Option<Manifest> {
    let bytes = fs::read(dir.join("manifest.json")).ok()?;
    let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
    if manifest.schema_version != SCHEMA_VERSION {
        return None;
    }
    let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if manifest.root != canon.to_string_lossy() {
        return None;
    }
    Some(manifest)
}

/// True when the on-disk index is missing, incompatible, or any tracked
/// file's mtime changed since the last build.
pub fn is_stale(root: &Path) -> bool {
    let Some(dir) = index_dir(root) else {
        return true;
    };
    let Some(manifest) = load_manifest(root, &dir) else {
        return true;
    };
    let current = discovery::source_files(root);
    !discovery::diff_files(&current, &manifest.files).is_empty()
}

/// Build (or refresh) the index for `root`, recording a telemetry event (see
/// [`crate::observe`]). Incremental when a compatible manifest exists and the
/// delta is small; full rebuild when `force`, on schema changes, or when more
/// than half the tree moved. A no-op when current.
pub fn build(root: &Path, force: bool) -> Result<BuildStats> {
    let t0 = std::time::Instant::now();
    let result = build_impl(root, force);
    crate::observe::record_build(root, "bm25", &result, t0.elapsed());
    result
}

fn build_impl(root: &Path, force: bool) -> Result<BuildStats> {
    let dir = index_dir(root).context("no cache dir available")?;
    let current = discovery::source_files(root);
    if current.is_empty() {
        anyhow::bail!("no indexable source files found under {}", root.display());
    }

    if !force {
        if let Some(manifest) = load_manifest(root, &dir) {
            let diff = discovery::diff_files(&current, &manifest.files);
            if diff.is_empty() {
                return Ok(BuildStats {
                    files: current.len(),
                    chunks: 0,
                    rebuilt: false,
                    incremental: false,
                });
            }
            if diff.total() * 2 <= current.len() {
                // Unopenable index (hand-deleted files, …) → rebuild below.
                if let Ok(stats) = update_incremental(root, &dir, &current, &diff) {
                    return Ok(stats);
                }
            }
        }
    }
    full_rebuild(root, &dir, current)
}

/// Delta path: term-delete every chunk of removed/changed files, re-chunk the
/// added/changed ones, and leave untouched documents (and their segments)
/// alone.
fn update_incremental(
    root: &Path,
    dir: &Path,
    current: &[(String, u64)],
    diff: &FileDiff,
) -> Result<BuildStats> {
    let index = Index::open_in_dir(dir)?;
    let (_, fields) = make_schema_for(&index.schema());
    let mut writer = index.writer(50_000_000)?;

    for rel in diff.removed.iter().chain(&diff.changed) {
        let term = Term::from_field_text(fields.path, rel);
        writer.delete_query(Box::new(TermQuery::new(term, IndexRecordOption::Basic)))?;
    }
    let mut chunks = 0usize;
    for rel in diff.added.iter().chain(&diff.changed) {
        chunks += add_file(&mut writer, &fields, root, rel)?;
    }
    writer.commit()?;
    let _ = writer.wait_merging_threads();

    write_manifest(dir, root, current)?;
    Ok(BuildStats {
        files: current.len(),
        chunks,
        rebuilt: true,
        incremental: true,
    })
}

/// Wipe and recreate for clean segments + correct counts.
fn full_rebuild(root: &Path, dir: &Path, files: Vec<(String, u64)>) -> Result<BuildStats> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir)?;

    let (schema, fields) = make_schema();
    let index = Index::create_in_dir(dir, schema)?;
    let mut writer = index.writer(50_000_000)?;

    let mut total_chunks = 0usize;
    for (rel, _mtime) in &files {
        total_chunks += add_file(&mut writer, &fields, root, rel)?;
    }
    writer.commit()?;
    // Merge down so subsequent searches touch few segments.
    let _ = writer.wait_merging_threads();

    write_manifest(dir, root, &files)?;
    Ok(BuildStats {
        files: files.len(),
        chunks: total_chunks,
        rebuilt: true,
        incremental: false,
    })
}

fn write_manifest(dir: &Path, root: &Path, files: &[(String, u64)]) -> Result<()> {
    let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        built_at: now_secs(),
        root: canon.to_string_lossy().into_owned(),
        files: files.iter().cloned().collect(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

/// Chunk `rel` and add its documents to the writer. Returns the chunk count.
fn add_file(writer: &mut IndexWriter, fields: &Fields, root: &Path, rel: &str) -> Result<usize> {
    let abs = root.join(rel);
    let Ok(text) = fs::read_to_string(&abs) else {
        return Ok(0);
    };
    let Some(lang) = discovery::lang_for(&abs) else {
        return Ok(0);
    };
    let mut n = 0usize;
    for c in chunks_for(&text, &abs) {
        // The breadcrumb is prepended to the token stream (not the stored
        // source) so symbol paths rank without polluting displayed snippets.
        let tokens = match &c.breadcrumb {
            Some(crumb) => expand(&format!("{crumb}\n{}", c.text)),
            None => expand(&c.text),
        };
        writer.add_document(doc!(
            fields.path => rel,
            fields.start => c.start as u64,
            fields.end => c.end as u64,
            fields.lang => lang,
            fields.breadcrumb => c.breadcrumb.unwrap_or_default(),
            fields.source => c.text.as_str(),
            fields.tokens => tokens.as_str(),
        ))?;
        n += 1;
    }
    Ok(n)
}

/// Run a ranked BM25 search against `root`'s index. The index must already
/// exist (call [`build`] first, or rely on the command layer to do so).
///
/// `lang` and `path_filter` become query clauses (no over-fetch window), with
/// a post-filter fallback for path needles the query syntax can't express.
pub fn search(
    root: &Path,
    query: &str,
    limit: usize,
    lang: Option<&str>,
    path_filter: Option<&str>,
) -> Result<Vec<Hit>> {
    let dir = index_dir(root).context("no cache dir available")?;
    if !dir.join("meta.json").exists() {
        anyhow::bail!("no index found — run `repo index` first");
    }
    let index = Index::open_in_dir(&dir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();

    let (_, fields) = make_schema_for(&index.schema());
    let qp = QueryParser::for_index(&index, vec![fields.tokens]);
    let user = match qp.parse_query(query) {
        Ok(q) => q,
        Err(_) => {
            // Special chars in the query (parens, colons, …) confuse the parser.
            // Fall back to a phrase query of the raw text.
            qp.parse_query(&format!("\"{}\"", query.escape_default()))?
        }
    };

    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, Box::new(user))];
    if let Some(want) = lang {
        let term = Term::from_field_text(fields.lang, &want.to_ascii_lowercase());
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
        ));
    }
    // Path substring: regex over the raw path terms when the needle can be
    // embedded safely, otherwise post-filter with an over-fetch window.
    let mut post_path: Option<String> = None;
    if let Some(needle) = path_filter {
        match path_substring_clause(fields.path, needle) {
            Some(q) => clauses.push((Occur::Must, q)),
            None => post_path = Some(needle.to_string()),
        }
    }
    let combined = BooleanQuery::new(clauses);

    let fetch = if post_path.is_some() {
        (limit * 5).max(limit + 10)
    } else {
        limit
    };
    let top: Vec<(tantivy::Score, tantivy::DocAddress)> =
        searcher.search(&combined, &TopDocs::with_limit(fetch).order_by_score())?;

    let mut hits = Vec::with_capacity(limit);
    for (score, addr) in top {
        if hits.len() >= limit {
            break;
        }
        let d: TantivyDocument = searcher.doc(addr)?;
        let path = d
            .get_first(fields.path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(needle) = &post_path {
            if !path.contains(needle) {
                continue;
            }
        }
        let lang_val = d
            .get_first(fields.lang)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let start = d
            .get_first(fields.start)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let end = d
            .get_first(fields.end)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let breadcrumb = d
            .get_first(fields.breadcrumb)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let source = d
            .get_first(fields.source)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        hits.push(Hit {
            path,
            start,
            end,
            lang: lang_val,
            score,
            source,
        });
        let _ = breadcrumb; // stored for future display; unused in ranking
    }
    Ok(hits)
}

/// Substring match on the raw `path` terms via a `.*needle.*` regex —
/// Tantivy has no wildcard query, and its query parser would treat `*` as a
/// literal. Case-sensitive, matching the old post-filter semantics. `None`
/// when the needle can't be embedded safely (regex metacharacters are
/// escaped; whitespace and query-syntax specials fall back to the caller's
/// post-filter).
fn path_substring_clause(field: tantivy::schema::Field, needle: &str) -> Option<Box<dyn Query>> {
    const QUERY_SPECIALS: &str = "+-&|!(){}[]^\"~*?:\\/";
    if needle.is_empty() {
        return None;
    }
    let mut pattern = String::with_capacity(needle.len() + 4);
    pattern.push_str(".*");
    for c in needle.chars() {
        if !c.is_ascii_alphanumeric() {
            if QUERY_SPECIALS.contains(c) || c.is_whitespace() {
                return None;
            }
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push_str(".*");
    let q = RegexQuery::from_pattern(&pattern, field).ok()?;
    Some(Box::new(q))
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

struct Fields {
    path: tantivy::schema::Field,
    start: tantivy::schema::Field,
    end: tantivy::schema::Field,
    lang: tantivy::schema::Field,
    breadcrumb: tantivy::schema::Field,
    source: tantivy::schema::Field,
    tokens: tantivy::schema::Field,
}

fn make_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let f = Fields {
        // STRING = raw single-token indexing (per-file term deletes, wildcard
        // path filters); `| STORED` so the values read back at search time.
        path: b.add_text_field("path", STRING | STORED),
        start: b.add_u64_field("start", STORED),
        end: b.add_u64_field("end", STORED),
        lang: b.add_text_field("lang", STRING | STORED),
        breadcrumb: b.add_text_field("breadcrumb", STORED),
        source: b.add_text_field("source", STORED),
        tokens: b.add_text_field("tokens", TEXT),
    };
    (b.build(), f)
}

/// Reconstruct field handles from an existing schema (used at search time when
/// we open an index rather than create one).
fn make_schema_for(schema: &Schema) -> ((), Fields) {
    let get = |name: &str| schema.get_field(name).expect("schema missing field");
    (
        (),
        Fields {
            path: get("path"),
            start: get("start"),
            end: get("end"),
            lang: get("lang"),
            breadcrumb: get("breadcrumb"),
            source: get("source"),
            tokens: get("tokens"),
        },
    )
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempfile_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("repo-search-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Open a file and pin a distinct mtime so the manifest diff (second
    /// granularity) reliably sees the change, even within the same second.
    fn bump_mtime(path: &Path, offset: u64) {
        let f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(
            std::fs::FileTimes::new().set_modified(
                UNIX_EPOCH
                    + std::time::Duration::from_secs(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + offset,
                    ),
            ),
        )
        .unwrap();
    }

    /// End-to-end: build an index in a temp dir and search it. Verifies the
    /// whole schema/writer/reader path works on this Tantivy version.
    #[test]
    fn build_and_search_roundtrip() {
        let tmp = tempfile_dir("roundtrip");
        let src = "#![allow(dead_code)]\n\
                   pub fn handle_login(user: &str) -> bool {\n\
                       let auth = authenticate(user);\n\
                       auth\n\
                   }\n\
                   pub fn authenticate(token: &str) -> bool {\n\
                       token.len() > 3\n\
                   }\n";
        fs::write(tmp.join("auth.rs"), src).unwrap();

        let stats = build(&tmp, true).expect("build");
        assert!(stats.rebuilt);
        assert_eq!(stats.files, 1);
        assert!(
            stats.chunks >= 1,
            "expected at least one chunk, got {}",
            stats.chunks
        );

        // Not stale immediately after build.
        assert!(!is_stale(&tmp));

        // "login" should rank the handle_login chunk highest.
        let hits = search(&tmp, "login", 5, None, None).expect("search");
        assert!(!hits.is_empty(), "expected hits for 'login'");
        assert!(
            hits[0].source.contains("handle_login"),
            "top hit was: {}",
            hits[0].source
        );

        // camelCase expansion: "auth" should also hit authenticate.
        let hits = search(&tmp, "authenticate", 5, None, None).expect("search2");
        assert!(hits.iter().any(|h| h.source.contains("authenticate")));

        fs::remove_dir_all(&tmp).ok();
    }

    /// The zg lesson, tested: editing one file must not re-chunk the world,
    /// and removed files must vanish from the index.
    #[test]
    fn incremental_update_touches_only_the_delta() {
        let tmp = tempfile_dir("incremental");
        fs::write(tmp.join("a.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
        fs::write(tmp.join("b.rs"), "pub fn gamma_unique() {}\n").unwrap();

        let first = build(&tmp, false).expect("initial build");
        assert!(first.rebuilt);
        assert_eq!(first.files, 2);

        // Change a.rs, delete b.rs, add c.rs.
        fs::write(
            tmp.join("a.rs"),
            "pub fn alpha() {}\npub fn delta_new() {}\n",
        )
        .unwrap();
        bump_mtime(&tmp.join("a.rs"), 5);
        fs::remove_file(tmp.join("b.rs")).unwrap();
        fs::write(tmp.join("c.rs"), "pub fn epsilon() {}\n").unwrap();

        let second = build(&tmp, false).expect("incremental build");
        assert!(second.rebuilt, "a real diff must rebuild something");
        assert_eq!(second.files, 2, "a.rs + c.rs remain");
        assert!(
            second.chunks <= 3,
            "only the delta is re-chunked, got {}",
            second.chunks
        );

        // Removed file's symbols are gone.
        let hits = search(&tmp, "gamma_unique", 5, None, None).expect("search removed");
        assert!(hits.is_empty(), "deleted b.rs must vanish: {hits:?}");

        // New and changed symbols are findable.
        let hits = search(&tmp, "delta_new", 5, None, None).expect("search changed");
        assert!(hits.iter().any(|h| h.path == "a.rs"));
        let hits = search(&tmp, "epsilon", 5, None, None).expect("search added");
        assert!(hits.iter().any(|h| h.path == "c.rs"));

        // And the index is settled again.
        assert!(!is_stale(&tmp));
        let noop = build(&tmp, false).expect("noop build");
        assert!(!noop.rebuilt);
        fs::remove_dir_all(&tmp).ok();
    }

    /// `--lang` must filter inside the query: a match buried beyond a small
    /// over-fetch window in another language still yields to the filter.
    #[test]
    fn lang_filter_is_query_level() {
        let tmp = tempfile_dir("langfilter");
        // Lots of rust noise so a post-filter window would drown the python hit.
        for i in 0..30 {
            fs::write(tmp.join(format!("noise{i}.rs")), "pub fn target_x() {}\n").unwrap();
        }
        fs::write(tmp.join("real.py"), "def target_x():\n    pass\n").unwrap();

        build(&tmp, false).expect("build");
        let hits = search(&tmp, "target_x", 1, Some("python"), None).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].lang, "python");

        let hits = search(&tmp, "target_x", 3, None, None).expect("search unfiltered");
        assert!(hits.len() >= 3, "unfiltered query keeps ranking rust noise");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn path_filter_is_query_level() {
        let tmp = tempfile_dir("pathfilter");
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("top.rs"), "pub fn needle_fn() {}\n").unwrap();
        fs::write(tmp.join("sub").join("deep.rs"), "pub fn needle_fn() {}\n").unwrap();

        build(&tmp, false).expect("build");
        let hits = search(&tmp, "needle_fn", 10, None, Some("sub")).expect("search");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.path.starts_with("sub")), "{hits:?}");

        // Special chars the wildcard syntax can't carry → post-filter path.
        let hits = search(&tmp, "needle_fn", 10, None, Some("sub/deep.rs")).expect("search2");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.path.contains("sub/deep.rs")));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn fnv_is_deterministic() {
        let a = fnv1a64("/Users/foo/src/repo");
        let b = fnv1a64("/Users/foo/src/repo");
        assert_eq!(a, b);
        assert_ne!(a, fnv1a64("/Users/foo/src/other"));
    }

    /// Silence unused-import noise if constants get optimized out in tests.
    #[test]
    fn chunk_constants_are_sane() {
        use crate::search::chunker::{chunks_for, CHUNK_LINES, OVERLAP_LINES};
        const { assert!(CHUNK_LINES > OVERLAP_LINES) };
        let _ = chunks_for("a\nb\n", Path::new("x.sh"));
    }
}
