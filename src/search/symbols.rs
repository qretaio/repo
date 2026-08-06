//! Symbol reference scanning (Phase 3b).
//!
//! Complements ranked [`crate::search`] with *exact* symbol lookup: where is
//! `Foo` defined, and where is it referenced? Index-independent regex matching
//! — works without `repo index`. Mirrors shebe's `find_references` split between
//! ranked search (fuzzy, natural-language) and exact symbol refs.
//!
//! Definitions are matched by per-language declaration patterns (`fn`, `struct`,
//! `def`, `func`, …). References are whole-word occurrences classified as
//! definitions (high confidence) or plain mentions (lower confidence).

use regex::Regex;
use std::path::Path;

/// One symbol occurrence.
#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub path: String,
    pub line: u32,
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A declaration of the symbol (`fn foo`, `struct Foo`, …).
    Definition,
    /// Any other whole-word occurrence.
    Reference,
}

/// Find every definition and reference of `symbol` under `root`.
///
/// `symbol` is matched as a whole word (regex `\b{symbol}\b`, escaped). Lines
/// that also match a declaration pattern for the file's language are classified
/// as [`Kind::Definition`]. Results are sorted definitions-first then by path.
pub fn find(root: &Path, symbol: &str) -> Vec<SymbolRef> {
    let sym_re = match whole_word_regex(symbol) {
        Some(re) => re,
        None => return Vec::new(),
    };
    let mut out = Vec::new();

    for rel in crate::search::index::list_source_files(root) {
        let abs = root.join(&rel);
        let Some(lang) = crate::search::index::lang_for(&abs) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let def_re = definition_regex(lang, symbol);

        for (i, line) in content.lines().enumerate() {
            if !sym_re.is_match(line) {
                continue;
            }
            let is_def = def_re.as_ref().is_some_and(|r| r.is_match(line));
            out.push(SymbolRef {
                path: rel.clone(),
                line: (i + 1) as u32,
                text: line.trim().to_string(),
                kind: if is_def {
                    Kind::Definition
                } else {
                    Kind::Reference
                },
            });
        }
    }

    // Definitions first, then references; within each group by path then line.
    out.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    out
}

impl Kind {
    fn rank(self) -> u8 {
        match self {
            Kind::Definition => 0,
            Kind::Reference => 1,
        }
    }
}

/// Build a `\b{escaped}\b` regex for whole-word matching. Returns `None` for an
/// empty/whitespace symbol.
fn whole_word_regex(symbol: &str) -> Option<Regex> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        return None;
    }
    let escaped = regex::escape(trimmed);
    // `\b` sits on a `[A-Za-z0-9_]` boundary, which is exactly the identifier
    // boundary we want for code.
    Regex::new(&format!(r"\b{escaped}\b")).ok()
}

/// Per-language declaration regex matching `symbol` as the declared name.
/// Compiled once per `(lang, symbol)` pair via the call site.
fn definition_regex(lang: &str, symbol: &str) -> Option<Regex> {
    let sym = regex::escape(symbol.trim());
    if sym.is_empty() {
        return None;
    }
    // `(?:pub\s+)?` lets `pub fn foo` and `fn foo` both match.
    let pat = match lang {
        "rust" => format!(
            r"(?:pub(?:\([^)]*\))?\s+)?(?:fn|struct|enum|trait|const|static|type|mod)\s+{sym}\b|macro_rules!\s+{sym}\b"
        ),
        "python" => format!(r"(?:def|class)\s+{sym}\b"),
        "go" => format!(r"func\s+(?:\([^)]*\)\s+)?{sym}\b|type\s+{sym}\b"),
        "javascript" | "typescript" => {
            format!(r"(?:export\s+)?(?:function|class|const|let|var|interface|type|enum)\s+{sym}\b")
        }
        "java" | "kotlin" | "scala" | "csharp" => {
            format!(
                r"(?:class|interface|enum|void|int|String|public|private|protected|static)\s+(?:[A-Za-z0-9_<>\[\],\s]+\s+)?{sym}\s*\("
            )
        }
        "c" | "cpp" => format!(r"(?:void|int|char|float|double|struct|enum|typedef)\s+\*?{sym}\b"),
        "ruby" => format!(r"(?:def|class|module)\s+{sym}\b"),
        "php" => format!(r"(?:function|class|interface)\s+{sym}\b"),
        "swift" => format!(r"(?:func|class|struct|enum|protocol|typealias)\s+{sym}\b"),
        _ => return None,
    };
    Regex::new(&pat).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_word_excludes_substrings() {
        let tmp = std::env::temp_dir().join(format!(
            "repo-refs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("a.rs"),
            "struct Foo {}\nimpl Foo {}\nlet foobar = 1;\nlet x = Foo;\n",
        )
        .unwrap();

        let refs = find(&tmp, "Foo");
        let defs: Vec<_> = refs.iter().filter(|r| r.kind == Kind::Definition).collect();
        let uses: Vec<_> = refs.iter().filter(|r| r.kind == Kind::Reference).collect();
        assert_eq!(defs.len(), 1, "one definition: {refs:?}");
        assert_eq!(defs[0].line, 1);
        // `foobar` must NOT match; `impl Foo` (line 2) and `Foo` (line 4) do.
        assert_eq!(uses.len(), 2, "two references: {refs:?}");
        assert!(uses.iter().all(|r| r.line == 2 || r.line == 4));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn empty_symbol_returns_nothing() {
        assert!(find(Path::new("."), "").is_empty());
        assert!(find(Path::new("."), "   ").is_empty());
    }

    #[test]
    fn definitions_sort_first() {
        let tmp = std::env::temp_dir().join(format!(
            "repo-refs-sort-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.rs"), "fn alpha() {}\nalpha();\n").unwrap();

        let refs = find(&tmp, "alpha");
        assert_eq!(refs[0].kind, Kind::Definition);
        assert_eq!(refs[1].kind, Kind::Reference);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
