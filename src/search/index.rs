//! Tantivy-backed ranked code search — index build.
//!
//! One index per repository, stored under `~/.cache/repo/index/<fnv(root)>/`.
//! The index is a set of overlapping line chunks; each chunk carries the raw
//! source (for display) plus a pre-expanded token stream (see [`tokenizer`])
//! that is what Tantivy actually ranks. Re-indexing is incremental by mtime:
//! if any tracked file changed since the last build we rebuild from scratch
//! (Tantivy handles segment merging; a full rebuild is fast and obviously
//! correct, which beats a subtle incremental delete/update story).

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{doc, Index};

use super::chunker::chunks;
use super::tokenizer::expand;

const SCHEMA_VERSION: u32 = 1;

/// Outcome of an index build.
#[derive(Debug, Clone)]
pub struct BuildStats {
    pub files: usize,
    pub chunks: usize,
    /// True when the on-disk index was replaced rather than reused.
    pub rebuilt: bool,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    built_at: u64,
    root: String,
    files: HashMap<String, u64>, // rel path -> mtime secs
}

/// Resolve the cache directory for `root`'s index. Deterministic via FNV-1a of
/// the canonical path so the same repo always maps to the same dir.
pub fn index_dir(root: &Path) -> Option<PathBuf> {
    let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let key = fnv1a64(&canon.to_string_lossy());
    let base = cache_base()?;
    Some(base.join(format!("{key:016x}")))
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

/// True when the on-disk index is missing, incompatible, or any tracked file's
/// mtime changed since the last build.
pub fn is_stale(root: &Path) -> bool {
    let Some(dir) = index_dir(root) else {
        return true;
    };
    let Ok(manifest_bytes) = fs::read(dir.join("manifest.json")) else {
        return true;
    };
    let Ok(manifest) = serde_json::from_slice::<Manifest>(&manifest_bytes) else {
        return true;
    };
    if manifest.schema_version != SCHEMA_VERSION {
        return true;
    }
    if manifest.root != root.to_string_lossy() {
        return true;
    }
    let current = collect_files(root);
    match current {
        None => true,
        Some(files) => {
            if files.len() != manifest.files.len() {
                return true;
            }
            for (rel, mtime) in &files {
                match manifest.files.get(rel) {
                    Some(stored) if *stored == *mtime => {}
                    _ => return true,
                }
            }
            false
        }
    }
}

/// Build (or refresh) the index for `root`. Rebuilds from scratch when `force`
/// or when [`is_stale`]; otherwise is a no-op returning zeroed stats.
pub fn build(root: &Path, force: bool) -> Result<BuildStats> {
    let stale = is_stale(root);
    if !force && !stale {
        // Reuse: report nothing was done but keep the function total.
        let dir = index_dir(root).context("no cache dir available")?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
        return Ok(BuildStats {
            files: manifest.files.len(),
            chunks: 0,
            rebuilt: false,
        });
    }

    let Some(files) = collect_files(root) else {
        anyhow::bail!("could not enumerate source files (is `rg` installed?)");
    };
    if files.is_empty() {
        anyhow::bail!("no indexable source files found under {}", root.display());
    }

    let dir = index_dir(root).context("no cache dir available")?;
    // Wipe and recreate for clean segments + correct counts.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;

    let (schema, fields) = make_schema();
    let index = Index::create_in_dir(&dir, schema)?;
    let mut writer = index.writer(50_000_000)?;

    let mut total_chunks = 0usize;
    for (rel, _mtime) in &files {
        let abs = root.join(rel);
        let Ok(text) = fs::read_to_string(&abs) else {
            continue;
        };
        let Some(lang) = lang_for(&abs) else { continue };
        for c in chunks(&text) {
            let tokens = expand(&c.text);
            writer.add_document(doc!(
                fields.path => rel.as_str(),
                fields.start => c.start as u64,
                fields.end => c.end as u64,
                fields.lang => lang,
                fields.source => c.text.as_str(),
                fields.tokens => tokens.as_str(),
            ))?;
            total_chunks += 1;
        }
    }
    writer.commit()?;
    // Merge down so subsequent searches touch few segments.
    let _ = writer.wait_merging_threads();

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        built_at: now_secs(),
        root: root.to_string_lossy().into_owned(),
        files: files.into_iter().collect(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(BuildStats {
        files: manifest.files.len(),
        chunks: total_chunks,
        rebuilt: true,
    })
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

struct Fields {
    path: tantivy::schema::Field,
    start: tantivy::schema::Field,
    end: tantivy::schema::Field,
    lang: tantivy::schema::Field,
    source: tantivy::schema::Field,
    tokens: tantivy::schema::Field,
}

fn make_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let f = Fields {
        path: b.add_text_field("path", STORED),
        start: b.add_u64_field("start", STORED),
        end: b.add_u64_field("end", STORED),
        lang: b.add_text_field("lang", STORED),
        source: b.add_text_field("source", STORED),
        tokens: b.add_text_field("tokens", TEXT),
    };
    (b.build(), f)
}

// ---------------------------------------------------------------------------
// file discovery + language mapping
// ---------------------------------------------------------------------------

/// Map a source extension to a language label, or `None` for non-code files.
pub(crate) fn lang_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "java" => "java",
        "kt" => "kotlin",
        "scala" => "scala",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "sh" | "bash" => "shell",
        _ => return None,
    })
}

/// Generated/dependency directories we never want to index. Matched by path
/// *component* (not substring) so root-relative paths from `rg --files` like
/// `target/debug/repo` are excluded correctly.
fn is_excluded(rel: &str) -> bool {
    let normalized = rel.replace('\\', "/");
    normalized
        .split('/')
        .any(|seg| EXCLUDE_NAMES.contains(&seg))
}
const EXCLUDE_NAMES: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    "vendor",
    ".cache",
    "__pycache__",
];

/// Enumerate indexable source files under `root` with their mtimes. Uses `rg`
/// when available (fast, respects ignore-ish behavior via plain --files) and
/// falls back to a recursive walk otherwise. Returns `None` only when listing
/// itself is impossible.
fn collect_files(root: &Path) -> Option<Vec<(String, u64)>> {
    let (ok, out) = crate::context::capture("rg", &["--files", "--no-ignore-vcs"], root);
    let lines: Vec<String> = if ok {
        out.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        // Fallback: walk the tree ourselves.
        walk(root)?
    };

    let mut files: Vec<(String, u64)> = Vec::new();
    for rel in lines {
        if is_excluded(&rel) {
            continue;
        }
        let abs = root.join(&rel);
        let Some(lang) = lang_for(&abs) else { continue };
        let _ = lang; // presence is the gate; we don't store lang here
        let Ok(meta) = fs::metadata(&abs) else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        files.push((rel, mtime));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Some(files)
}

/// Minimal recursive directory walk fallback (no external crate).
fn walk(root: &Path) -> Option<Vec<String>> {
    fn rec(base: &Path, rel: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(base.join(rel)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let entry_rel = rel.join(name.as_ref());
            let entry_rel_str = entry_rel.to_string_lossy().replace('\\', "/");
            if is_excluded(&entry_rel_str) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                rec(base, &entry_rel, out);
            } else if path.is_file() {
                out.push(entry_rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    rec(root, Path::new(""), &mut out);
    Some(out)
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

/// FNV-1a 64-bit — deterministic, no random seed (unlike `DefaultHasher`).
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_mapping_covers_common_exts() {
        assert_eq!(lang_for(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(lang_for(Path::new("a.go")), Some("go"));
        assert_eq!(lang_for(Path::new("x.tsx")), Some("typescript"));
        assert_eq!(lang_for(Path::new("README.md")), None);
        assert_eq!(lang_for(Path::new("no_ext")), None);
    }

    #[test]
    fn excludes_generated_dirs() {
        assert!(is_excluded("target/debug/repo"));
        assert!(is_excluded("node_modules/foo/index.js"));
        assert!(!is_excluded("src/main.rs"));
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
        use crate::search::chunker::{CHUNK_LINES, OVERLAP_LINES};
        const { assert!(CHUNK_LINES > OVERLAP_LINES) };
        let _ = chunks("a\nb\n");
    }
}
