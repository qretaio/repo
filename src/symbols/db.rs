//! SQLite-backed symbol + import-edge store.
//!
//! One database per repository at `~/.cache/repo/symbols/<fnv1a(root)>.db`,
//! keyed identically to the search index so both map the same repo to the same
//! cache bucket. Staleness is tracked by a `tracked_files` table (path → mtime)
//! compared against the current file walk; a mismatch triggers a rebuild.

use super::lang::language_for_path;
use super::parse;
use super::{DefKind, Definition, ImportEdge};
use crate::search::discovery::source_files;
use crate::search::index::fnv1a64;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 2;

/// Resolve the SQLite path for `root`'s symbol store.
pub fn db_path(root: &Path) -> Option<PathBuf> {
    let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let key = fnv1a64(&canon.to_string_lossy());
    let dir = cache_base()?;
    Some(dir.join(format!("{key:016x}.db")))
}

fn cache_base() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".cache")
                .join("repo")
                .join("symbols"),
        );
    }
    Some(std::env::temp_dir().join("repo-symbols"))
}

/// True when the store is missing, incompatible, or any tracked file changed.
pub fn is_stale(root: &Path) -> bool {
    let Some(path) = db_path(root) else {
        return true;
    };
    let Ok(conn) = Connection::open(&path) else {
        return true;
    };
    if !meta_is_current(&conn, root) {
        return true;
    }
    let tracked = match read_tracked(&conn) {
        Ok(t) => t,
        Err(_) => return true,
    };
    let current = current_files(root);
    if current.len() != tracked.len() {
        return true;
    }
    for (path, mtime) in &current {
        match tracked.get(path) {
            Some(stored) if *stored == *mtime => {}
            _ => return true,
        }
    }
    false
}

/// Source files (of the supported languages) under `root` with their mtimes.
fn current_files(root: &Path) -> Vec<(String, u64)> {
    source_files(root)
        .into_iter()
        .filter(|(rel, _)| language_for_path(&root.join(rel)).is_some())
        .collect()
}

/// A handle to an open store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the store for `root`, initialising the schema. If the
    /// on-disk schema version is older than the current one, the tables are
    /// dropped and recreated (a rebuild repopulates them).
    pub fn open(root: &Path) -> Result<Store> {
        let path = db_path(root).context("no cache dir available")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        let stored = query_meta(&conn, "schema_version").ok().flatten();
        if stored.as_deref() != Some(SCHEMA_VERSION.to_string()).as_deref() {
            conn.execute_batch(
                "DROP TABLE IF EXISTS symbols;
                 DROP TABLE IF EXISTS imports;
                 DROP TABLE IF EXISTS tracked_files;
                 DROP TABLE IF EXISTS meta;",
            )?;
            conn.execute_batch(SCHEMA)?;
        }
        Ok(Store { conn })
    }

    /// Atomically replace all definitions, imports, and tracked files.
    pub fn replace_all(
        &mut self,
        defs: &[Definition],
        imports: &[ImportEdge],
        tracked: &[(String, u64)],
        root: &Path,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM symbols; DELETE FROM imports; DELETE FROM tracked_files;")?;
        for d in defs {
            tx.execute(
                "INSERT INTO symbols (name, kind, file_path, start_line, end_line, signature, lang, parent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    d.name,
                    d.kind.label(),
                    d.file_path,
                    d.start_line,
                    d.end_line,
                    d.signature,
                    d.lang,
                    d.parent,
                ],
            )?;
        }
        for e in imports {
            tx.execute(
                "INSERT INTO imports (source_file, target) VALUES (?1, ?2)",
                params![e.source_file, e.target],
            )?;
        }
        for (path, mtime) in tracked {
            tx.execute(
                "INSERT INTO tracked_files (path, mtime) VALUES (?1, ?2)",
                params![path, *mtime as i64],
            )?;
        }
        write_meta(&tx, root)?;
        tx.commit()?;
        Ok(())
    }

    /// Definitions whose name exactly equals `name`.
    pub fn definitions(&self, name: &str) -> Result<Vec<Definition>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, kind, file_path, start_line, end_line, signature, lang, parent FROM symbols WHERE name = ?1 ORDER BY file_path, start_line")?;
        let rows = stmt.query_map(params![name], row_to_definition)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every definition in the store, ordered by file then line.
    pub fn all_definitions(&self) -> Result<Vec<Definition>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, kind, file_path, start_line, end_line, signature, lang, parent FROM symbols ORDER BY file_path, start_line")?;
        let rows = stmt.query_map([], row_to_definition)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Definitions whose enclosing type (`parent`) equals `parent` — i.e. the
    /// methods of a type.
    pub fn children(&self, parent: &str) -> Result<Vec<Definition>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, kind, file_path, start_line, end_line, signature, lang, parent FROM symbols WHERE parent = ?1 ORDER BY file_path, start_line")?;
        let rows = stmt.query_map(params![parent], row_to_definition)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Files that import a target whose path ends with `target` (suffix match,
    /// so `repo::db::Store` matches `crate::repo::db::Store`).
    pub fn importers(&self, target: &str) -> Result<Vec<String>> {
        let like = format!("%{target}");
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT source_file FROM imports WHERE target = ?1 OR target LIKE ?2 ORDER BY source_file")?;
        let rows = stmt.query_map(params![target, like], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn row_to_definition(row: &rusqlite::Row<'_>) -> rusqlite::Result<Definition> {
    let kind_str: String = row.get(1)?;
    let kind = DefKind::from_label(&kind_str).unwrap_or(DefKind::Function);
    Ok(Definition {
        name: row.get(0)?,
        kind,
        file_path: row.get(2)?,
        start_line: row.get(3)?,
        end_line: row.get(4)?,
        signature: row.get(5)?,
        lang: row.get(6)?,
        parent: row.get(7)?,
    })
}

/// Parse every supported file under `root`, returning (definitions, imports,
/// tracked files).
pub fn extract_repo(root: &Path) -> (Vec<Definition>, Vec<ImportEdge>, Vec<(String, u64)>) {
    let mut defs = Vec::new();
    let mut imports = Vec::new();
    let mut tracked = Vec::new();

    for (rel, mtime) in current_files(root) {
        let abs = root.join(&rel);
        let Ok(content) = fs::read_to_string(&abs) else {
            continue;
        };
        defs.extend(parse::parse_definitions(&content, &rel));
        imports.extend(parse::parse_imports(&content, &rel));
        tracked.push((rel, mtime));
    }
    (defs, imports, tracked)
}

/// DDL for the symbol store, kept in SQL for first-class editing and embedded.
const SCHEMA: &str = include_str!("schema.sql");

fn meta_is_current(conn: &Connection, root: &Path) -> bool {
    let Ok(v) = query_meta(conn, "schema_version") else {
        return false;
    };
    if v.as_deref() != Some(SCHEMA_VERSION.to_string()).as_deref() {
        return false;
    }
    let Ok(root_stored) = query_meta(conn, "root") else {
        return false;
    };
    root_stored.as_deref() == Some(root.to_string_lossy()).as_deref()
}

fn query_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
}

fn read_tracked(conn: &Connection) -> rusqlite::Result<HashMap<String, u64>> {
    let mut stmt = conn.prepare("SELECT path, mtime FROM tracked_files")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (p, m) = row?;
        map.insert(p, m);
    }
    Ok(map)
}

fn write_meta(conn: &Connection, root: &Path) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('root', ?1)",
        params![root.to_string_lossy()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("repo-sym-test-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_then_query_roundtrip() {
        let root = tmp_root();
        fs::write(
            root.join("a.rs"),
            "use std::fs::File;\n\npub fn alpha() -> u32 { 1 }\nstruct Beta;\n",
        )
        .unwrap();
        fs::write(root.join("b.py"), "import os\n\ndef gamma():\n    pass\n").unwrap();

        let (defs, imports, tracked) = extract_repo(&root);
        assert!(defs.iter().any(|d| d.name == "alpha"));
        assert!(defs.iter().any(|d| d.name == "Beta"));
        assert!(defs.iter().any(|d| d.name == "gamma"));
        assert!(imports.iter().any(|e| e.target == "std::fs::File"));
        assert!(imports.iter().any(|e| e.target == "os"));
        assert_eq!(tracked.len(), 2);

        let mut store = Store::open(&root).unwrap();
        store.replace_all(&defs, &imports, &tracked, &root).unwrap();

        assert!(!is_stale(&root), "freshly built store must not be stale");

        let alpha = store.definitions("alpha").unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].kind, DefKind::Function);

        let all = store.all_definitions().unwrap();
        assert!(all.len() >= 3);

        let importers = store.importers("std::fs::File").unwrap();
        assert_eq!(importers, vec!["a.rs".to_string()]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_after_file_set_change() {
        let root = tmp_root();
        let path = root.join("a.rs");
        fs::write(&path, "fn one() {}\n").unwrap();
        let (defs, imports, tracked) = extract_repo(&root);
        let mut store = Store::open(&root).unwrap();
        store.replace_all(&defs, &imports, &tracked, &root).unwrap();
        assert!(!is_stale(&root));

        // Removing a tracked file changes the file set → store becomes stale.
        fs::remove_file(&path).unwrap();
        assert!(is_stale(&root), "changed file set must make store stale");

        fs::remove_dir_all(&root).ok();
    }
}
