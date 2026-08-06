//! Refs command — symbol definitions, importers, and references.
//!
//! Definitions and importers come from the persisted tree-sitter symbol store
//! (built by `repo symbols`; auto-built on first use); references are scanned
//! on demand with the parser, so comment/string occurrences are excluded.

use clap::Args;
use colored::Colorize;
use serde_json::json;
use std::collections::HashSet;

use crate::detect::Detector;
use crate::symbols::{self, DefKind, Definition};
use crate::Globals;

#[derive(Args)]
pub struct RefsArgs {
    /// Symbol name to look up. Omit to list all available symbols.
    pub symbol: Option<String>,

    /// Show definitions only (skip importers and references).
    #[arg(long)]
    pub defs_only: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(_d: &Detector, _g: &Globals, args: &RefsArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    // Auto-build the store on first use; warn (don't block) when stale.
    let store_exists = symbols::db::db_path(&root)
        .map(|p| p.exists())
        .unwrap_or(false);
    if !store_exists {
        eprintln!("{}", "No symbol store yet — building…".cyan());
        if let Err(e) = symbols::build(&root, true) {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    } else if symbols::is_stale(&root) {
        eprintln!(
            "{}",
            "Symbol store is stale — run `repo symbols` to refresh.".yellow()
        );
    }

    // No symbol (or blank) → list the symbols available for lookup.
    let symbol = match args.symbol.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s,
        _ => return list_symbols(&root, args.json),
    };

    let defs = match symbols::definitions(&root, symbol) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    // A symbol's definitions include both the type itself and its `impl` blocks
    // (both are named after the type). Split them; `methods` are the defs whose
    // enclosing type is this symbol.
    let type_defs: Vec<Definition> = defs
        .iter()
        .filter(|d| d.kind != DefKind::Impl)
        .cloned()
        .collect();
    let impls: Vec<Definition> = defs
        .iter()
        .filter(|d| d.kind == DefKind::Impl)
        .cloned()
        .collect();

    let methods = if args.defs_only {
        Vec::new()
    } else {
        symbols::children(&root, symbol).unwrap_or_default()
    };

    let importers = if args.defs_only {
        Vec::new()
    } else {
        symbols::importers(&root, symbol).unwrap_or_default()
    };

    let mut refs = if args.defs_only {
        Vec::new()
    } else {
        symbols::references(&root, symbol)
    };

    // Drop reference lines that are themselves a definition site (avoids dupes).
    let def_sites: HashSet<(String, u32)> = defs
        .iter()
        .map(|d| (d.file_path.clone(), d.start_line))
        .collect();
    refs.retain(|r| !def_sites.contains(&(r.file_path.clone(), r.line)));

    if defs.is_empty() && methods.is_empty() && importers.is_empty() && refs.is_empty() {
        if !args.json {
            eprintln!("{}", format!("No occurrences of {symbol:?}.").yellow());
        }
        return 0;
    }

    if args.json {
        let mut pool = methods.clone();
        let impls_json: Vec<_> = impls
            .iter()
            .map(|imp| {
                let mut nested = Vec::new();
                pool.retain(|m| {
                    let inside = m.file_path == imp.file_path
                        && imp.start_line <= m.start_line
                        && m.start_line <= imp.end_line;
                    if inside {
                        nested.push(m.clone());
                    }
                    !inside
                });
                json!({
                    "path": imp.file_path, "start": imp.start_line, "end": imp.end_line,
                    "methods": nested.iter().map(def_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        let payload = json!({
            "symbol": symbol,
            "definitions": type_defs.iter().map(def_json).collect::<Vec<_>>(),
            "impls": impls_json,
            "methods": pool.iter().map(def_json).collect::<Vec<_>>(),
            "importers": importers,
            "references": refs.iter().map(|r| json!({
                "path": r.file_path, "line": r.line, "column": r.column, "text": r.text,
            })).collect::<Vec<_>>(),
        });
        println!("{payload}");
        return 0;
    }

    println!(
        "{}",
        format!(
            "{symbol:?}: {} def(s), {} impl(s), {} method(s), {} importer(s), {} reference(s)",
            type_defs.len(),
            impls.len(),
            methods.len(),
            importers.len(),
            refs.len(),
        )
        .bold()
    );

    if !type_defs.is_empty() || !impls.is_empty() {
        println!("\n{}", "Definitions:".green().bold());
        for d in &type_defs {
            let parent_note = d
                .parent
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default();
            println!(
                "  {} {}{}",
                decl_head(d),
                format!("{}:{}", d.file_path, d.start_line).cyan(),
                parent_note.bright_black(),
            );
        }
        // Each impl block with its methods nested by source containment.
        let mut pool = methods.clone();
        for imp in &impls {
            println!(
                "  {} {} {}",
                "impl".bright_black(),
                imp.name.bold(),
                format!("{}:{}-{}", imp.file_path, imp.start_line, imp.end_line).cyan(),
            );
            let mut i = 0;
            while i < pool.len() {
                let m = &pool[i];
                if m.file_path == imp.file_path
                    && imp.start_line <= m.start_line
                    && m.start_line <= imp.end_line
                {
                    println!("    {} ({})", decl_head(m), m.start_line);
                    pool.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        // Methods not inside any recorded impl (e.g. Python/TS class methods).
        for m in &pool {
            println!(
                "  {} {}",
                decl_head(m),
                format!("{}:{}", m.file_path, m.start_line).cyan(),
            );
        }
    }
    if !importers.is_empty() {
        println!("\n{}", "Imported by:".magenta().bold());
        for f in &importers {
            println!("  {}", f.cyan());
        }
    }
    if !refs.is_empty() {
        println!("\n{}", "References:".blue().bold());
        for r in &refs {
            println!(
                "  {} │ {}",
                format!("{}:{}", r.file_path, r.line).cyan(),
                r.text
            );
        }
    }
    0
}

/// With no symbol argument, list the symbols available for lookup: every
/// distinct definition name in the store, with its kinds and occurrence count.
fn list_symbols(root: &std::path::Path, json: bool) -> i32 {
    let all = match symbols::all_definitions(root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    // name -> (distinct kinds, site count)
    let mut groups: std::collections::BTreeMap<&str, (std::collections::BTreeSet<&str>, usize)> =
        std::collections::BTreeMap::new();
    for d in &all {
        let g = groups.entry(d.name.as_str()).or_default();
        g.0.insert(d.kind.label());
        g.1 += 1;
    }

    if groups.is_empty() {
        if !json {
            eprintln!("{}", "No symbols indexed — is the store built?".yellow());
        }
        return 0;
    }

    if json {
        let arr: Vec<_> = groups
            .iter()
            .map(|(name, (kinds, count))| {
                json!({
                    "name": name,
                    "kinds": kinds.iter().copied().collect::<Vec<_>>(),
                    "definitions": count,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return 0;
    }

    println!(
        "{}",
        format!(
            "Available symbols ({} unique, {} definitions):",
            groups.len(),
            all.len()
        )
        .bold()
    );
    for (name, (kinds, count)) in &groups {
        let keywords = kinds
            .iter()
            .filter_map(|l| DefKind::from_label(l).map(|k| k.keyword()))
            .collect::<Vec<_>>()
            .join("/");
        let tail = if *count > 1 {
            format!(" ×{count}").bright_black().to_string()
        } else {
            String::new()
        };
        println!("  {} {}{}", keywords.bright_black(), name.bold(), tail);
    }
    0
}

fn def_json(d: &Definition) -> serde_json::Value {
    json!({
        "name": d.name,
        "kind": d.kind.label(),
        "path": d.file_path,
        "start": d.start_line,
        "end": d.end_line,
        "signature": d.signature,
        "lang": d.lang,
        "parent": d.parent,
    })
}

/// Compact one-line declaration: the signature for functions/methods, else the
/// `keyword name` form (struct/enum/trait/…).
fn decl_head(d: &Definition) -> String {
    d.signature
        .clone()
        .unwrap_or_else(|| format!("{} {}", d.kind.keyword(), d.name))
}
