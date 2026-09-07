//! File discovery — one walker shared by the search index, the symbol store,
//! and the context gatherer.
//!
//! Uses the `ignore` crate (the walker behind ripgrep): `.gitignore`,
//! `.ignore`, global git ignores and `.git/info/exclude` all apply, hidden
//! entries are skipped, and no external `rg` process is needed. On top of the
//! ignore rules we apply a small blocklist of generated directories and a
//! per-file size cap — belt and braces for repos with sloppy ignore hygiene
//! (lessons from zvec-grep's discovery pipeline).

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;

/// Files larger than this are never indexed: build artifacts, minified
/// bundles, data dumps. (zg grows type-aware caps; one cap is enough at the
/// codebase sizes `repo` targets.)
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Generated/dependency directories we skip even when not gitignored. Matched
/// by path component so `target/debug/repo` is excluded but `src/target.rs`
/// is not.
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

/// Every indexable source file under `root` as (rel path, mtime secs), sorted
/// by rel path. Extension-gated by [`lang_for`].
pub fn source_files(root: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        // Apply .gitignore even when `root` isn't inside a git repo.
        .require_git(false)
        .filter_entry(keep_entry);
    for entry in builder.build().flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        if lang_for(entry.path()).is_none() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((rel.to_string_lossy().replace('\\', "/"), mtime));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Rel paths only — for callers that don't need mtimes.
pub fn list_source_files(root: &Path) -> Vec<String> {
    source_files(root).into_iter().map(|(rel, _)| rel).collect()
}

/// Delta between the current tree and a stored (rel path → mtime) snapshot.
#[derive(Debug, Default)]
pub struct FileDiff {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }
}

/// Diff a fresh [`source_files`] listing against a stored snapshot. `changed`
/// covers mtime bumps; `added` and `removed` are symmetric difference.
pub fn diff_files(current: &[(String, u64)], stored: &HashMap<String, u64>) -> FileDiff {
    let mut diff = FileDiff::default();
    for (rel, mtime) in current {
        match stored.get(rel) {
            Some(stored) if *stored == *mtime => {}
            Some(_) => diff.changed.push(rel.clone()),
            None => diff.added.push(rel.clone()),
        }
    }
    diff.removed = stored
        .keys()
        .filter(|rel| !current.iter().any(|(c, _)| c == *rel))
        .cloned()
        .collect();
    diff.removed.sort();
    diff
}

/// Map a source extension to a language label, or `None` for non-code files.
pub fn lang_for(path: &Path) -> Option<&'static str> {
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

/// Prune predicate for the walker: `false` skips the entry (and, for
/// directories, never descends). Only directories are blocklisted here; files
/// are gated by extension and size in [`source_files`].
fn keep_entry(entry: &ignore::DirEntry) -> bool {
    if !entry.file_type().is_some_and(|t| t.is_dir()) {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !EXCLUDE_NAMES.contains(&name.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("repo-discovery-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lang_mapping_covers_common_exts() {
        assert_eq!(lang_for(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(lang_for(Path::new("a.go")), Some("go"));
        assert_eq!(lang_for(Path::new("x.tsx")), Some("typescript"));
        assert_eq!(lang_for(Path::new("README.md")), None);
        assert_eq!(lang_for(Path::new("no_ext")), None);
    }

    #[test]
    fn gitignore_rules_are_respected() {
        let tmp = temp_dir("gitignore");
        fs::write(tmp.join(".gitignore"), "generated/\nsecret.rs\n").unwrap();
        fs::create_dir_all(tmp.join("generated")).unwrap();
        fs::write(tmp.join("generated").join("g.rs"), "fn g() {}\n").unwrap();
        fs::write(tmp.join("secret.rs"), "fn secret() {}\n").unwrap();
        fs::write(tmp.join("keep.rs"), "fn keep() {}\n").unwrap();

        let files = list_source_files(&tmp);
        assert!(files.contains(&"keep.rs".to_string()), "got: {files:?}");
        assert!(!files.iter().any(|f| f.contains("generated/")));
        assert!(!files.contains(&"secret.rs".to_string()));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn generated_dirs_are_blocked_without_gitignore() {
        let tmp = temp_dir("blocklist");
        fs::create_dir_all(tmp.join("node_modules").join("pkg")).unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("node_modules").join("pkg").join("i.js"), "x").unwrap();
        fs::write(tmp.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let files = list_source_files(&tmp);
        assert_eq!(files, vec!["src/main.rs".to_string()]);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn oversized_files_are_skipped() {
        let tmp = temp_dir("sizecap");
        let big = "fn filler() {}\n".repeat(80_000); // > 1 MiB
        fs::write(tmp.join("big.rs"), &big).unwrap();
        fs::write(tmp.join("small.rs"), "fn small() {}\n").unwrap();

        let files = list_source_files(&tmp);
        assert_eq!(files, vec!["small.rs".to_string()]);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn diff_classifies_added_changed_removed() {
        let current = vec![
            ("a.rs".to_string(), 1),
            ("b.rs".to_string(), 9), // mtime bumped → changed
            ("d.rs".to_string(), 1), // new → added
        ];
        let stored = HashMap::from([
            ("a.rs".to_string(), 1),
            ("b.rs".to_string(), 2),
            ("c.rs".to_string(), 1), // gone → removed
        ]);
        let diff = diff_files(&current, &stored);
        assert_eq!(diff.added, vec!["d.rs".to_string()]);
        assert_eq!(diff.changed, vec!["b.rs".to_string()]);
        assert_eq!(diff.removed, vec!["c.rs".to_string()]);
        assert!(!diff.is_empty());
        assert_eq!(diff.total(), 3);
    }

    #[test]
    fn diff_empty_when_snapshots_match() {
        let current = vec![("a.rs".to_string(), 7)];
        let stored = HashMap::from([("a.rs".to_string(), 7)]);
        assert!(diff_files(&current, &stored).is_empty());
    }
}
