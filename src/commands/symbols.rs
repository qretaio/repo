//! Symbols command — build/refresh the tree-sitter symbol store.

use crate::detect::Detector;
use crate::symbols;
use crate::Globals;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct SymbolsArgs {
    /// Force a full rebuild even if the store is current.
    #[arg(short, long)]
    pub force: bool,
}

pub fn run(_d: &Detector, _g: &Globals, args: &SymbolsArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    match symbols::build(&root, args.force) {
        Ok(stats) => {
            if !stats.rebuilt {
                println!(
                    "{}",
                    format!("Symbol store up to date ({} definitions).", stats.defs).green()
                );
            } else {
                println!(
                    "{}",
                    format!(
                        "Indexed {} files: {} definitions, {} import edges.",
                        stats.files, stats.defs, stats.imports
                    )
                    .green()
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            1
        }
    }
}
