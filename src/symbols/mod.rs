//! Symbol store (Phase 4) — tree-sitter-powered definitions, references, and
//! import edges, backed by a SQLite cache.
//!
//! Inspired by minni's tree-sitter indexer but built natively for `repo`:
//! parse-on-demand for `refs`/`ctx` precision, plus a persisted SQLite symbol
//! + import-edge store for instant `repo symbols` lookups and `imported_by`.
//!
//! Languages (day 1): Rust, Python, Go, TypeScript, JavaScript.

pub mod db;
pub mod lang;
pub mod parse;

use serde::{Deserialize, Serialize};

/// A declared symbol (function, struct, class, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub kind: DefKind,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Declaration signature (text up to the body), when extractable.
    pub signature: Option<String>,
    pub lang: String,
    /// Enclosing type for methods (e.g. the implementee of an `impl` block, or
    /// the class a method belongs to). `None` for top-level definitions.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Type,
    Impl,
}

impl DefKind {
    pub fn label(self) -> &'static str {
        match self {
            DefKind::Function => "function",
            DefKind::Method => "method",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            DefKind::Class => "class",
            DefKind::Type => "type",
            DefKind::Impl => "impl",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Some(match s {
            "function" => DefKind::Function,
            "method" => DefKind::Method,
            "struct" => DefKind::Struct,
            "enum" => DefKind::Enum,
            "trait" => DefKind::Trait,
            "class" => DefKind::Class,
            "type" => DefKind::Type,
            "impl" => DefKind::Impl,
            _ => return None,
        })
    }

    /// Language keyword for declaration-style display (`fn foo`, `struct Foo`).
    pub fn keyword(self) -> &'static str {
        match self {
            DefKind::Function | DefKind::Method => "fn",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            DefKind::Class => "class",
            DefKind::Type => "type",
            DefKind::Impl => "impl",
        }
    }
}

/// A whole-word usage of a symbol (never inside a comment or string literal).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
}

/// A module-level import edge: `source_file` imports `target` (a module/path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEdge {
    pub source_file: String,
    pub target: String,
}

// ================================ facade ==================================

use anyhow::{Context as _, Result};
use std::path::Path;

/// Outcome of a symbol-store build.
#[derive(Debug, Clone)]
pub struct BuildStats {
    pub files: usize,
    pub defs: usize,
    pub imports: usize,
    pub rebuilt: bool,
}

/// Build (or refresh) the symbol store for `root`. No-op when current unless
/// `force`.
pub fn build(root: &Path, force: bool) -> Result<BuildStats> {
    if !force && !is_stale(root) {
        let all = all_definitions(root).unwrap_or_default();
        return Ok(BuildStats {
            files: 0,
            defs: all.len(),
            imports: 0,
            rebuilt: false,
        });
    }
    let (defs, imports, tracked) = db::extract_repo(root);
    if tracked.is_empty() {
        anyhow::bail!("no indexable source files found under {}", root.display());
    }
    let mut store = db::Store::open(root).context("open symbol store")?;
    store.replace_all(&defs, &imports, &tracked, root)?;
    Ok(BuildStats {
        files: tracked.len(),
        defs: defs.len(),
        imports: imports.len(),
        rebuilt: true,
    })
}

/// True when the store is missing/incompatible/stale.
pub fn is_stale(root: &Path) -> bool {
    db::is_stale(root)
}

/// Definitions whose name equals `name` (from the store).
pub fn definitions(root: &Path, name: &str) -> Result<Vec<Definition>> {
    let store = db::Store::open(root).context("open symbol store")?;
    store.definitions(name)
}

/// All definitions (from the store), ordered by file then line.
pub fn all_definitions(root: &Path) -> Result<Vec<Definition>> {
    let store = db::Store::open(root).context("open symbol store")?;
    store.all_definitions()
}

/// Files that import `target` (suffix match on the import path).
pub fn importers(root: &Path, target: &str) -> Result<Vec<String>> {
    let store = db::Store::open(root).context("open symbol store")?;
    store.importers(target)
}

/// Definitions whose enclosing type is `parent` (the methods of a type).
pub fn children(root: &Path, parent: &str) -> Result<Vec<Definition>> {
    let store = db::Store::open(root).context("open symbol store")?;
    store.children(parent)
}

/// Whole-word usages of `symbol`, on-demand (no store). Skips comments/strings
/// via the tree-sitter parse.
pub fn references(root: &Path, symbol: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    for rel in crate::search::index::list_source_files(root) {
        if lang::language_for_path(Path::new(&rel)).is_none() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(root.join(&rel)) else {
            continue;
        };
        out.extend(parse::parse_references(&content, &rel, symbol));
    }
    out
}
