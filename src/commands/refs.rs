//! Refs command — find symbol definitions and references (exact match).

use clap::Args;
use colored::Colorize;
use serde_json::json;

use crate::detect::Detector;
use crate::search::symbols::{self, Kind};
use crate::Globals;

#[derive(Args)]
pub struct RefsArgs {
    /// Symbol name to look up (function, struct, class, …).
    pub symbol: String,

    /// Show definitions only (skip plain references).
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

    let mut refs = symbols::find(&root, &args.symbol);
    if args.defs_only {
        refs.retain(|r| r.kind == Kind::Definition);
    }

    if refs.is_empty() {
        if !args.json {
            eprintln!(
                "{}",
                format!("No occurrences of {:?}.", args.symbol).yellow()
            );
        }
        return 0;
    }

    if args.json {
        let arr: Vec<_> = refs
            .iter()
            .map(|r| {
                json!({
                    "path": r.path,
                    "line": r.line,
                    "kind": match r.kind {
                        Kind::Definition => "definition",
                        Kind::Reference => "reference",
                    },
                    "text": r.text,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return 0;
    }

    let defs: Vec<_> = refs.iter().filter(|r| r.kind == Kind::Definition).collect();
    let uses: Vec<_> = refs.iter().filter(|r| r.kind == Kind::Reference).collect();

    println!(
        "{}",
        format!(
            "{} {:?} — {} definition(s), {} reference(s)",
            refs.len(),
            args.symbol,
            defs.len(),
            uses.len()
        )
        .bold()
    );

    if !defs.is_empty() {
        println!("\n{}", "Definitions:".green().bold());
        for r in defs {
            println!("  {} │ {}", format!("{}:{}", r.path, r.line).cyan(), r.text);
        }
    }
    if !uses.is_empty() {
        println!("\n{}", "References:".blue().bold());
        for r in uses {
            println!("  {} │ {}", format!("{}:{}", r.path, r.line).cyan(), r.text);
        }
    }
    0
}
