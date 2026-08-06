//! Language registry: maps a source file to its tree-sitter grammar and the
//! AST node kinds that represent definitions. Scoped to the five core
//! languages (Rust, Python, Go, TypeScript, JavaScript); other files are not
//! symbol-indexed. Ported from minni's `indexer/languages.rs`, trimmed.

use std::path::Path;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
    Go,
    TypeScript,
    JavaScript,
}

impl SupportedLanguage {
    /// Lowercase label, matching `search::index::lang_for` so the two systems
    /// agree on language names.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
        }
    }

    /// Node kinds that declare a function/method.
    pub fn function_node_types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["function_item"],
            Self::Python => &["function_definition"],
            Self::Go => &["function_declaration", "method_declaration"],
            Self::TypeScript | Self::JavaScript => &["function_declaration", "method_definition"],
        }
    }

    /// Node kinds that declare a type/class-like construct.
    pub fn class_node_types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["struct_item", "enum_item", "trait_item"],
            Self::Python => &["class_definition"],
            Self::Go => &["type_declaration"],
            Self::TypeScript | Self::JavaScript => &["class_declaration"],
        }
    }

    /// Field holding the declared name on a definition node.
    pub fn name_field(self) -> &'static str {
        match self {
            Self::Rust | Self::Python | Self::Go | Self::TypeScript | Self::JavaScript => "name",
        }
    }
}

/// Resolve a source file to `(language, grammar)`. Returns `None` for
/// unsupported extensions. `.tsx` uses the TSX grammar but is classified as
/// TypeScript (it shares function/class node kinds).
pub fn language_for_path(path: &Path) -> Option<(SupportedLanguage, Language)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => (SupportedLanguage::Rust, tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" => (
            SupportedLanguage::Python,
            tree_sitter_python::LANGUAGE.into(),
        ),
        "go" => (SupportedLanguage::Go, tree_sitter_go::LANGUAGE.into()),
        "ts" | "mts" | "cts" => (
            SupportedLanguage::TypeScript,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        "tsx" => (
            SupportedLanguage::TypeScript,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
        ),
        "js" | "mjs" | "cjs" | "jsx" => (
            SupportedLanguage::JavaScript,
            tree_sitter_javascript::LANGUAGE.into(),
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_core_extensions() {
        assert!(matches!(
            language_for_path(Path::new("a.rs")),
            Some((SupportedLanguage::Rust, _))
        ));
        assert!(matches!(
            language_for_path(Path::new("a.py")),
            Some((SupportedLanguage::Python, _))
        ));
        assert!(matches!(
            language_for_path(Path::new("a.go")),
            Some((SupportedLanguage::Go, _))
        ));
        assert!(matches!(
            language_for_path(Path::new("a.ts")),
            Some((SupportedLanguage::TypeScript, _))
        ));
        assert!(matches!(
            language_for_path(Path::new("a.js")),
            Some((SupportedLanguage::JavaScript, _))
        ));
    }

    #[test]
    fn unsupported_extensions_return_none() {
        assert!(language_for_path(Path::new("a.md")).is_none());
        assert!(language_for_path(Path::new("a.c")).is_none());
        assert!(language_for_path(Path::new("no_ext")).is_none());
    }
}
