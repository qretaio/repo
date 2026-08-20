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

    // Ensure the store is current before querying. `build` is a no-op when the
    // store is fresh; otherwise (missing, schema-bumped/empty, or source
    // changed) it rebuilds — so refs never silently serve stale/empty data.
    if let Err(e) = ensure_store(&root) {
        eprintln!("{}", format!("Error: {e}").red());
        return 1;
    }

    // No symbol (or blank) → list the symbols available for lookup.
    let symbol = match args.symbol.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s,
        _ => return list_symbols(&root, args.json),
    };

    let data = match refs_data(&root, symbol, args.defs_only) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    if !data.found() {
        if !args.json {
            eprintln!("{}", format!("No occurrences of {symbol:?}.").yellow());
        }
        return 0;
    }

    if args.json {
        println!("{}", data.to_json(symbol));
        return 0;
    }

    data.render(symbol);
    0
}

/// Ensure the tree-sitter symbol store is current; prints a stderr note when
/// a rebuild happened (stderr is safe for the MCP stdio transport). Errors
/// propagate to the caller for display.
pub(crate) fn ensure_store(root: &std::path::Path) -> anyhow::Result<()> {
    match symbols::build(root, false) {
        Ok(stats) if stats.rebuilt => {
            eprintln!(
                "{}",
                format!("Indexed {} symbols in {} files.", stats.defs, stats.files).cyan()
            );
        }
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Everything known about one symbol, split for rendering. Shared by the CLI
/// and the MCP `refs` tool.
pub(crate) struct RefsData {
    /// Definitions of the type itself (kind != impl).
    pub type_defs: Vec<Definition>,
    /// `impl` blocks for the type (also named after it).
    pub impls: Vec<Definition>,
    /// Methods whose enclosing type is this symbol (empty when defs_only).
    pub methods: Vec<Definition>,
    /// Files importing the symbol (empty when defs_only).
    pub importers: Vec<String>,
    /// Non-definition usage sites (empty when defs_only).
    pub refs: Vec<symbols::Reference>,
}

impl RefsData {
    pub fn found(&self) -> bool {
        self.type_defs.is_empty()
            && self.impls.is_empty()
            && self.methods.is_empty()
            && self.importers.is_empty()
            && self.refs.is_empty()
    }

    /// The CLI `--json` payload (byte-identical): impls nest their contained
    /// methods; `methods` carries those not inside any recorded impl.
    pub fn to_json(&self, symbol: &str) -> serde_json::Value {
        let mut pool = self.methods.clone();
        let impls_json: Vec<_> = self
            .impls
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
        json!({
            "symbol": symbol,
            "definitions": self.type_defs.iter().map(def_json).collect::<Vec<_>>(),
            "impls": impls_json,
            "methods": pool.iter().map(def_json).collect::<Vec<_>>(),
            "importers": self.importers,
            "references": self.refs.iter().map(|r| json!({
                "path": r.file_path, "line": r.line, "column": r.column, "text": r.text,
            })).collect::<Vec<_>>(),
        })
    }

    /// Human rendering (the CLI default output).
    fn render(&self, symbol: &str) {
        println!(
            "{}",
            format!(
                "{symbol:?}: {} def(s), {} impl(s), {} method(s), {} importer(s), {} reference(s)",
                self.type_defs.len(),
                self.impls.len(),
                self.methods.len(),
                self.importers.len(),
                self.refs.len(),
            )
            .bold()
        );

        if !self.type_defs.is_empty() || !self.impls.is_empty() {
            println!("\n{}", "Definitions:".green().bold());
            for d in &self.type_defs {
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
            let mut pool = self.methods.clone();
            for imp in &self.impls {
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
        if !self.importers.is_empty() {
            println!("\n{}", "Imported by:".magenta().bold());
            for f in &self.importers {
                println!("  {}", f.cyan());
            }
        }
        if !self.refs.is_empty() {
            println!("\n{}", "References:".blue().bold());
            for r in &self.refs {
                println!(
                    "  {} │ {}",
                    format!("{}:{}", r.file_path, r.line).cyan(),
                    r.text
                );
            }
        }
    }
}

/// Gather definitions, impls, methods, importers, and references for `symbol`.
pub(crate) fn refs_data(
    root: &std::path::Path,
    symbol: &str,
    defs_only: bool,
) -> anyhow::Result<RefsData> {
    let defs = symbols::definitions(root, symbol)?;

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

    let methods = if defs_only {
        Vec::new()
    } else {
        symbols::children(root, symbol).unwrap_or_default()
    };

    let importers = if defs_only {
        Vec::new()
    } else {
        symbols::importers(root, symbol).unwrap_or_default()
    };

    let mut refs = if defs_only {
        Vec::new()
    } else {
        symbols::references(root, symbol)
    };

    // Drop reference lines that are themselves a definition site (avoids dupes).
    let def_sites: HashSet<(String, u32)> = defs
        .iter()
        .map(|d| (d.file_path.clone(), d.start_line))
        .collect();
    refs.retain(|r| !def_sites.contains(&(r.file_path.clone(), r.line)));

    Ok(RefsData {
        type_defs,
        impls,
        methods,
        importers,
        refs,
    })
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

    if all.is_empty() {
        if !json {
            eprintln!("{}", "No symbols indexed — is the store built?".yellow());
        }
        return 0;
    }

    if json {
        println!("{}", group_list_json(&all));
        return 0;
    }

    render_outline(&all);
    0
}

/// Group every definition by name: `[{name, kinds, definitions}, …]`, sorted.
/// Shared by the CLI `--json` list mode and the MCP `refs` tool.
pub(crate) fn group_list_json(all: &[Definition]) -> serde_json::Value {
    // name -> (distinct kinds, site count)
    let mut groups: std::collections::BTreeMap<&str, (std::collections::BTreeSet<&str>, usize)> =
        std::collections::BTreeMap::new();
    for d in all {
        let g = groups.entry(d.name.as_str()).or_default();
        g.0.insert(d.kind.label());
        g.1 += 1;
    }
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
    serde_json::Value::Array(arr)
}

/// Render a repo-wide hierarchical outline: every type expanded with its
/// `impl` blocks and the methods within them, followed by free functions and
/// methods of external types. The same breakdown `repo refs <symbol>` shows for
/// one symbol, but for all of them.
fn render_outline(all: &[Definition]) {
    use std::collections::HashSet;

    let is_type = |d: &&Definition| {
        matches!(
            d.kind,
            DefKind::Struct | DefKind::Enum | DefKind::Trait | DefKind::Class | DefKind::Type
        )
    };
    let type_names: HashSet<&str> = all
        .iter()
        .filter(|d| is_type(d))
        .map(|d| d.name.as_str())
        .collect();

    let mut types: Vec<&Definition> = all.iter().filter(|d| is_type(d)).collect();
    types.sort_by(|a, b| (&a.file_path, a.start_line).cmp(&(&b.file_path, b.start_line)));

    let impls: Vec<&Definition> = all.iter().filter(|d| d.kind == DefKind::Impl).collect();
    let belongs_to_type =
        |m: &&Definition| m.parent.as_deref().is_some_and(|p| type_names.contains(p));
    let type_methods: Vec<&Definition> = all
        .iter()
        .filter(|d| matches!(d.kind, DefKind::Function | DefKind::Method) && belongs_to_type(d))
        .collect();
    let mut free_fns: Vec<&Definition> = all
        .iter()
        .filter(|d| matches!(d.kind, DefKind::Function | DefKind::Method) && d.parent.is_none())
        .collect();
    let mut orphans: Vec<&Definition> = all
        .iter()
        .filter(|d| {
            matches!(d.kind, DefKind::Function | DefKind::Method)
                && d.parent.is_some()
                && !belongs_to_type(d)
        })
        .collect();

    println!(
        "{}",
        format!("Symbol outline ({} definitions)", all.len()).bold()
    );

    for ty in &types {
        println!(
            "\n  {} {}  {}:{}",
            ty.kind.keyword().bright_black(),
            ty.name.bold(),
            ty.file_path.cyan(),
            ty.start_line
        );
        // Methods of this type, nested into its impl blocks by containment.
        let mut pool: Vec<&Definition> = type_methods
            .iter()
            .copied()
            .filter(|m| m.parent.as_deref() == Some(ty.name.as_str()))
            .collect();
        for imp in impls.iter().filter(|i| i.name == ty.name) {
            println!(
                "    {} {}  {}:{}-{}",
                "impl".bright_black(),
                ty.name,
                imp.file_path.cyan(),
                imp.start_line,
                imp.end_line
            );
            let mut i = 0;
            while i < pool.len() {
                let m = pool[i];
                if m.file_path == imp.file_path
                    && imp.start_line <= m.start_line
                    && m.start_line <= imp.end_line
                {
                    println!("      {} ({})", decl_head(m), m.start_line);
                    pool.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        for m in pool {
            println!(
                "    {}  {}:{}",
                decl_head(m),
                m.file_path.cyan(),
                m.start_line
            );
        }
    }

    let by_loc = |a: &&Definition, b: &&Definition| {
        (&a.file_path, a.start_line).cmp(&(&b.file_path, b.start_line))
    };
    free_fns.sort_by(by_loc);
    if !free_fns.is_empty() {
        println!("\n  {}", "free functions".bright_black());
        for f in free_fns {
            println!(
                "    {}  {}:{}",
                decl_head(f),
                f.file_path.cyan(),
                f.start_line
            );
        }
    }
    orphans.sort_by(by_loc);
    if !orphans.is_empty() {
        println!("\n  {}", "methods on external types".bright_black());
        for m in orphans {
            println!(
                "    {}  {}:{}  (in {})",
                decl_head(m),
                m.file_path.cyan(),
                m.start_line,
                m.parent.as_deref().unwrap_or("?")
            );
        }
    }
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
