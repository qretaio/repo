//! tree-sitter extraction: definitions, references, and import edges.
//!
//! Definitions and the Rust/Python import walkers are ported from minni's
//! indexer (node-kind + `name` field + byte spans); Go/JS/TS imports and the
//! reference scanner are ours. References deliberately skip comment and string
//! subtrees — the precision win that justified adopting a parser.

use std::path::Path;

use tree_sitter::{Language, Node, Parser, Tree};

use super::lang::{language_for_path, SupportedLanguage};
use super::{DefKind, Definition, ImportEdge, Reference};

/// Parse `src` with `grammar`, returning the syntax tree (or `None`).
fn parse_tree(src: &str, grammar: Language) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    parser.parse(src, None)
}

// =============================== definitions ===============================

pub fn parse_definitions(content: &str, file_path: &str) -> Vec<Definition> {
    let (lang, grammar) = match language_for_path(Path::new(file_path)) {
        Some(x) => x,
        None => return Vec::new(),
    };
    let tree = match parse_tree(content, grammar) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    visit_defs(
        &mut tree.root_node().walk(),
        content,
        file_path,
        lang,
        &mut out,
    );
    out
}

fn visit_defs(
    cursor: &mut tree_sitter::TreeCursor,
    content: &str,
    file_path: &str,
    lang: SupportedLanguage,
    out: &mut Vec<Definition>,
) {
    let node = cursor.node();
    if let Some(kind) = def_kind(lang, node.kind()) {
        if let Some(name) = decl_name(&node, kind, lang, content) {
            let start = node.start_position().row as u32 + 1;
            let end = node.end_position().row as u32 + 1;
            let signature = if matches!(kind, DefKind::Function | DefKind::Method) {
                extract_signature(&node, content)
            } else {
                None
            };
            out.push(Definition {
                name,
                kind,
                file_path: file_path.to_string(),
                start_line: start,
                end_line: end,
                signature,
                lang: lang.name().to_string(),
                parent: parent_of(&node, lang, content),
            });
        }
    }

    // Always recurse so nested definitions (methods in impl, fns in module) are found.
    if cursor.goto_first_child() {
        loop {
            visit_defs(cursor, content, file_path, lang, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn def_kind(lang: SupportedLanguage, kind: &str) -> Option<DefKind> {
    // An `impl` block is a container, not a symbol — but we record it so refs
    // can render a type's implementations and the methods within them.
    if matches!(lang, SupportedLanguage::Rust) && kind == "impl_item" {
        return Some(DefKind::Impl);
    }
    let is_fn = lang.function_node_types().contains(&kind);
    let is_cls = lang.class_node_types().contains(&kind);
    if !is_fn && !is_cls {
        return None;
    }
    Some(match (lang, kind) {
        (SupportedLanguage::Rust, "function_item") => DefKind::Function,
        (SupportedLanguage::Rust, "struct_item") => DefKind::Struct,
        (SupportedLanguage::Rust, "enum_item") => DefKind::Enum,
        (SupportedLanguage::Rust, "trait_item") => DefKind::Trait,
        (SupportedLanguage::Python, "function_definition") => DefKind::Function,
        (SupportedLanguage::Python, "class_definition") => DefKind::Class,
        (SupportedLanguage::Go, "function_declaration") => DefKind::Function,
        (SupportedLanguage::Go, "method_declaration") => DefKind::Method,
        (SupportedLanguage::Go, "type_declaration") => DefKind::Type,
        (_, "function_declaration") => DefKind::Function,
        (_, "method_definition") => DefKind::Method,
        (_, "class_declaration") => DefKind::Class,
        _ => return None,
    })
}

/// Name of a definition node. For an `impl` block, the name is the implemented
/// type (the `type` field), so `impl Foo {…}` and `impl T for Foo {…}` are both
/// attributed to `Foo`.
fn decl_name(node: &Node, kind: DefKind, lang: SupportedLanguage, content: &str) -> Option<String> {
    let field = if kind == DefKind::Impl {
        "type"
    } else {
        lang.name_field()
    };
    name_of(node, content, field)
}

/// Name of the nearest enclosing container (struct/enum/trait/impl for Rust,
/// class for Python/TS/JS), if any. Used to set a method's `parent`.
fn parent_of(node: &Node, lang: SupportedLanguage, content: &str) -> Option<String> {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if let Some(name) = container_name(&p, lang, content) {
            return Some(name);
        }
        cur = p.parent();
    }
    None
}

fn container_name(node: &Node, lang: SupportedLanguage, content: &str) -> Option<String> {
    let kind = node.kind();
    let is_container = match lang {
        SupportedLanguage::Rust => {
            matches!(
                kind,
                "struct_item" | "enum_item" | "trait_item" | "impl_item"
            )
        }
        SupportedLanguage::Python => kind == "class_definition",
        SupportedLanguage::TypeScript | SupportedLanguage::JavaScript => {
            kind == "class_declaration"
        }
        SupportedLanguage::Go => false,
    };
    if !is_container {
        return None;
    }
    let field = if kind == "impl_item" {
        "type"
    } else {
        lang.name_field()
    };
    name_of(node, content, field)
}

fn name_of(node: &Node, content: &str, field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(content.as_bytes()).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Declaration text up to (not including) the body; falls back to first line.
/// Ported from minni's `extract_signature`.
fn extract_signature(node: &Node, content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let body_start = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .or_else(|| {
            (0..node.child_count())
                .filter_map(|i| node.child(i as u32))
                .find(|c| {
                    matches!(
                        c.kind(),
                        "body"
                            | "block"
                            | "declaration_list"
                            | "field_declaration_list"
                            | "statement_block"
                            | "compound_statement"
                            | "function_body"
                            | "class_body"
                    )
                })
                .map(|b| b.start_byte())
        });

    let sig = match body_start {
        Some(end) if end > node.start_byte() => {
            std::str::from_utf8(&bytes[node.start_byte()..end]).unwrap_or("")
        }
        _ => {
            let text =
                std::str::from_utf8(&bytes[node.start_byte()..node.end_byte()]).unwrap_or("");
            text.lines().next().unwrap_or("")
        }
    };
    let sig = sig.trim();
    if sig.is_empty() {
        None
    } else {
        Some(sig.to_string())
    }
}

// =============================== references ===============================

/// Whole-word usages of `symbol` that are NOT inside a comment or string.
pub fn parse_references(content: &str, file_path: &str, symbol: &str) -> Vec<Reference> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let (lang, grammar) = match language_for_path(Path::new(file_path)) {
        Some(x) => x,
        None => return Vec::new(),
    };
    let _ = lang;
    let tree = match parse_tree(content, grammar) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    collect_refs(
        &tree.root_node(),
        content.as_bytes(),
        trimmed,
        file_path,
        &lines,
        &mut out,
    );
    out
}

fn collect_refs<'a>(
    node: &Node<'a>,
    bytes: &[u8],
    symbol: &str,
    file_path: &str,
    lines: &[&'a str],
    out: &mut Vec<Reference>,
) {
    let kind = node.kind();
    if is_noise_kind(kind) {
        return; // never descend into comments / strings
    }
    if is_identifier_kind(kind) {
        if let Ok(text) = node.utf8_text(bytes) {
            if text == symbol {
                let pos = node.start_position();
                let line_text = lines.get(pos.row).copied().unwrap_or("").trim();
                out.push(Reference {
                    file_path: file_path.to_string(),
                    line: pos.row as u32 + 1,
                    column: pos.column as u32,
                    text: line_text.to_string(),
                });
            }
        }
        return; // identifiers are leaves
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_refs(&child, bytes, symbol, file_path, lines, out);
        }
    }
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "field_identifier"
            | "shorthand_property_identifier"
    )
}

/// Comment and string-literal node kinds — skipping these subtrees is what makes
/// refs precise vs. a regex scan.
fn is_noise_kind(kind: &str) -> bool {
    kind.contains("comment")
        || kind.contains("string")
        || matches!(kind, "regex" | "regex_pattern" | "rune_literal")
}

// ================================= imports =================================

pub fn parse_imports(content: &str, file_path: &str) -> Vec<ImportEdge> {
    let (lang, grammar) = match language_for_path(Path::new(file_path)) {
        Some(x) => x,
        None => return Vec::new(),
    };
    let tree = match parse_tree(content, grammar) {
        Some(t) => t,
        None => return Vec::new(),
    };
    match lang {
        SupportedLanguage::Rust => rust_imports(&tree, content, file_path),
        SupportedLanguage::Python => python_imports(&tree, content, file_path),
        SupportedLanguage::Go => string_imports(&tree, content, file_path, &["import_declaration"]),
        SupportedLanguage::TypeScript | SupportedLanguage::JavaScript => string_imports(
            &tree,
            content,
            file_path,
            &["import_statement", "export_statement"],
        ),
    }
}

/// Collect string-literal targets from the given top-level node kinds. Used for
/// Go (`import "pkg"`) and JS/TS (`import ... from "mod"`).
fn string_imports(
    tree: &Tree,
    content: &str,
    file_path: &str,
    top_kinds: &[&str],
) -> Vec<ImportEdge> {
    let bytes = content.as_bytes();
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for i in 0..root.child_count() {
        let Some(node) = root.child(i as u32) else {
            continue;
        };
        if !top_kinds.contains(&node.kind()) {
            continue;
        }
        buf.clear();
        collect_string_literals(&node, bytes, &mut buf);
        for target in buf.drain(..) {
            if !target.is_empty() {
                out.push(ImportEdge {
                    source_file: file_path.to_string(),
                    target,
                });
            }
        }
    }
    out
}

fn collect_string_literals(node: &Node, bytes: &[u8], out: &mut Vec<String>) {
    let kind = node.kind();
    if kind.contains("string") {
        if let Ok(text) = node.utf8_text(bytes) {
            out.push(strip_quotes(text));
        }
        return;
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i as u32) {
            collect_string_literals(&c, bytes, out);
        }
    }
}

/// Strip surrounding quotes / backticks and common Python/Rust prefixes.
fn strip_quotes(raw: &str) -> String {
    let s = raw.trim();
    let s = s.trim_start_matches(['r', 'b', 'f', 'R', 'B', 'F']);
    let s = s.trim_start_matches(['r', 'b', 'f', 'R', 'B', 'F']);
    let s = s
        .trim_start_matches('"')
        .trim_start_matches('\'')
        .trim_start_matches('`')
        .trim_end_matches('"')
        .trim_end_matches('\'')
        .trim_end_matches('`');
    s.to_string()
}

// ---- Rust imports (ported from minni) ----

fn rust_imports(tree: &Tree, content: &str, file_path: &str) -> Vec<ImportEdge> {
    let root = tree.root_node();
    let mut out = Vec::new();
    for i in 0..root.child_count() {
        let Some(node) = root.child(i as u32) else {
            continue;
        };
        if node.kind() != "use_declaration" {
            continue;
        }
        let mut targets = Vec::new();
        collect_rust_use_targets(&node, content, &mut Vec::new(), &mut targets);
        for t in targets {
            if !t.is_empty() {
                out.push(ImportEdge {
                    source_file: file_path.to_string(),
                    target: t,
                });
            }
        }
    }
    out
}

fn collect_rust_use_targets(
    node: &Node,
    content: &str,
    prefix: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    let bytes = content.as_bytes();
    match node.kind() {
        "use_declaration" => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32) {
                    let ck = c.kind();
                    if ck != "use" && ck != ";" && ck != "pub" && ck != "visibility_modifier" {
                        collect_rust_use_targets(&c, content, prefix, out);
                    }
                }
            }
        }
        "scoped_identifier" => {
            let mut parts = Vec::new();
            flatten_scoped(node, bytes, &mut parts);
            let combined = if prefix.is_empty() {
                parts.join("::")
            } else {
                format!("{}::{}", prefix.join("::"), parts.join("::"))
            };
            if !combined.is_empty() {
                out.push(combined);
            }
        }
        "scoped_use_list" => {
            let mut new_prefix = prefix.clone();
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32) {
                    let ck = c.kind();
                    if ck == "::" || ck == "{" || ck == "}" {
                        continue;
                    }
                    if ck == "use_list" {
                        collect_rust_use_targets(&c, content, &mut new_prefix, out);
                    } else {
                        let mut parts = Vec::new();
                        flatten_scoped(&c, bytes, &mut parts);
                        if !parts.is_empty() {
                            let seg = parts.join("::");
                            if new_prefix.is_empty() {
                                new_prefix = parts;
                            } else {
                                new_prefix.push(seg);
                            }
                        }
                    }
                }
            }
        }
        "use_list" => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32) {
                    if !matches!(c.kind(), "{" | "}" | ",") {
                        collect_rust_use_targets(&c, content, prefix, out);
                    }
                }
            }
        }
        "use_as_clause" => {
            if let Some(orig) = node.child(0) {
                collect_rust_use_targets(&orig, content, prefix, out);
            }
        }
        "use_wildcard" => {
            let raw = node.utf8_text(bytes).unwrap_or("").to_string();
            let path = if raw.contains("::") || prefix.is_empty() {
                raw
            } else {
                format!("{}::*", prefix.join("::"))
            };
            if !path.is_empty() {
                out.push(path);
            }
        }
        "identifier" | "self" | "super" | "crate" => {
            let text = node.utf8_text(bytes).unwrap_or("").to_string();
            if !text.is_empty() {
                let path = if text == "self" && !prefix.is_empty() {
                    prefix.join("::")
                } else if prefix.is_empty() {
                    text
                } else {
                    format!("{}::{}", prefix.join("::"), text)
                };
                out.push(path);
            }
        }
        _ => {}
    }
}

fn flatten_scoped(node: &Node, bytes: &[u8], parts: &mut Vec<String>) {
    match node.kind() {
        "scoped_identifier" => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32) {
                    if c.kind() != "::" {
                        flatten_scoped(&c, bytes, parts);
                    }
                }
            }
        }
        "identifier" | "self" | "super" | "crate" => {
            if let Ok(t) = node.utf8_text(bytes) {
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        _ => {
            if let Ok(t) = node.utf8_text(bytes) {
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
    }
}

// ---- Python imports (ported from minni) ----

fn python_imports(tree: &Tree, content: &str, file_path: &str) -> Vec<ImportEdge> {
    let bytes = content.as_bytes();
    let root = tree.root_node();
    let mut out = Vec::new();
    for i in 0..root.child_count() {
        let Some(node) = root.child(i as u32) else {
            continue;
        };
        match node.kind() {
            "import_statement" => {
                for j in 0..node.child_count() {
                    if let Some(c) = node.child(j as u32) {
                        match c.kind() {
                            "dotted_name" => {
                                push_py(&mut out, file_path, c.utf8_text(bytes).unwrap_or(""))
                            }
                            "aliased_import" => {
                                if let Some(orig) = c.child(0) {
                                    push_py(
                                        &mut out,
                                        file_path,
                                        orig.utf8_text(bytes).unwrap_or(""),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "import_from_statement" => {
                let module = py_from_module(&node, bytes);
                let mut past_import = false;
                for j in 0..node.child_count() {
                    if let Some(c) = node.child(j as u32) {
                        let ck = c.kind();
                        if ck == "import" {
                            past_import = true;
                            continue;
                        }
                        if !past_import {
                            continue;
                        }
                        py_collect_names(&c, bytes, &module, file_path, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn py_collect_names(
    node: &Node,
    bytes: &[u8],
    module: &str,
    file_path: &str,
    out: &mut Vec<ImportEdge>,
) {
    match node.kind() {
        "wildcard_import" => out.push(ImportEdge {
            source_file: file_path.to_string(),
            target: join_py(module, "*"),
        }),
        "dotted_name" | "identifier" => {
            let n = node.utf8_text(bytes).unwrap_or("");
            if !n.is_empty() {
                out.push(ImportEdge {
                    source_file: file_path.to_string(),
                    target: join_py(module, n),
                });
            }
        }
        "aliased_import" => {
            if let Some(orig) = node.child(0) {
                let n = orig.utf8_text(bytes).unwrap_or("");
                if !n.is_empty() {
                    out.push(ImportEdge {
                        source_file: file_path.to_string(),
                        target: join_py(module, n),
                    });
                }
            }
        }
        _ if !matches!(node.kind(), "," | "(" | ")") => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i as u32) {
                    py_collect_names(&c, bytes, module, file_path, out);
                }
            }
        }
        _ => {}
    }
}

fn py_from_module(node: &Node, bytes: &[u8]) -> String {
    let mut dots = String::new();
    let mut module = String::new();
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i as u32) {
            match c.kind() {
                "import" => break,
                "from" => {}
                "relative_import" => {
                    for j in 0..c.child_count() {
                        if let Some(rc) = c.child(j as u32) {
                            match rc.kind() {
                                "import_prefix" => {
                                    dots = rc.utf8_text(bytes).unwrap_or("").to_string()
                                }
                                "dotted_name" => {
                                    module = rc.utf8_text(bytes).unwrap_or("").to_string()
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "dotted_name" => module = c.utf8_text(bytes).unwrap_or("").to_string(),
                _ => {}
            }
        }
    }
    if dots.is_empty() {
        module
    } else if module.is_empty() {
        dots
    } else {
        format!("{dots}{module}")
    }
}

fn join_py(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else if prefix.ends_with('.') {
        format!("{prefix}{name}")
    } else {
        format!("{prefix}.{name}")
    }
}

fn push_py(out: &mut Vec<ImportEdge>, file_path: &str, raw: &str) {
    let t = raw.trim();
    if !t.is_empty() {
        out.push(ImportEdge {
            source_file: file_path.to_string(),
            target: t.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions_and_signature() {
        let src =
            "pub struct Foo { x: i32 }\nfn helper() -> u32 { 1 }\nimpl Foo { fn bar(&self){} }\n";
        let defs = parse_definitions(src, "src/a.rs");
        let names: Vec<_> = defs
            .iter()
            .map(|d| (d.name.as_str(), d.kind.label()))
            .collect();
        // struct Foo, fn helper, fn bar (method inside impl). No def for impl itself.
        assert!(names.contains(&("Foo", "struct")));
        assert!(names.contains(&("helper", "function")));
        assert!(
            names.contains(&("bar", "function")),
            "method inside impl must be found"
        );
        assert!(!names.iter().any(|(n, _)| *n == "impl" || *n == "Foo_bar"));

        let foo = defs.iter().find(|d| d.name == "helper").unwrap();
        assert!(foo
            .signature
            .as_deref()
            .is_some_and(|s| s.contains("helper")));
    }

    #[test]
    fn references_skip_comments_and_strings() {
        let src = "fn foo() {}\n// foo in a comment\nlet s = \"foo\";\nfoo();\n";
        let refs = parse_references(src, "src/a.rs", "foo");
        // Only the call `foo();` on line 4 should match — not the comment or string.
        let lines: Vec<u32> = refs.iter().map(|r| r.line).collect();
        assert!(lines.contains(&4), "call site must match: {refs:?}");
        assert!(!lines.contains(&2), "comment must be skipped");
        assert!(!lines.contains(&3), "string literal must be skipped");
    }

    #[test]
    fn python_class_and_imports() {
        let src = "import os.path\nfrom .utils import helper\n\nclass Thing:\n    pass\n";
        let defs = parse_definitions(src, "a.py");
        assert!(defs
            .iter()
            .any(|d| d.name == "Thing" && d.kind == DefKind::Class));

        let imps = parse_imports(src, "a.py");
        let targets: Vec<&str> = imps.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"os.path"), "{imps:?}");
        assert!(targets.contains(&".utils.helper"), "{imps:?}");
    }

    #[test]
    fn rust_use_grouping() {
        let src = "use std::collections::{HashMap, BTreeMap};\nfn main() {}\n";
        let imps = parse_imports(src, "src/lib.rs");
        let targets: Vec<&str> = imps.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"std::collections::HashMap"));
        assert!(targets.contains(&"std::collections::BTreeMap"));
    }

    #[test]
    fn go_and_ts_import_strings() {
        let go = "package main\n\nimport (\n\t\"fmt\"\n\t\"net/http\"\n)\n";
        let imps = parse_imports(go, "main.go");
        let targets: Vec<&str> = imps.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"fmt"), "{imps:?}");
        assert!(targets.contains(&"net/http"));

        let ts = "import { foo } from \"./foo\";\nimport bar from \"../bar\";\n";
        let imps = parse_imports(ts, "a.ts");
        let targets: Vec<&str> = imps.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"./foo"), "{imps:?}");
        assert!(targets.contains(&"../bar"));
    }

    #[test]
    fn unsupported_file_yields_nothing() {
        assert!(parse_definitions("anything", "a.md").is_empty());
        assert!(parse_references("foo", "a.md", "foo").is_empty());
        assert!(parse_imports("foo", "a.md").is_empty());
    }
}
