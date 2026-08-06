//! Index command — build the ranked search index for this repository.

use clap::Args;
use colored::Colorize;

use crate::detect::Detector;
use crate::search;
use crate::Globals;

#[derive(Args)]
pub struct IndexArgs {
    /// Force a full rebuild even if the index is already current.
    #[arg(short, long)]
    pub force: bool,
}

pub fn run(_d: &Detector, _g: &Globals, args: &IndexArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    match search::index::build(&root, args.force) {
        Ok(stats) => {
            if !stats.rebuilt {
                println!(
                    "{}",
                    format!("Index up to date ({} files).", stats.files).green()
                );
            } else {
                println!(
                    "{}",
                    format!("Indexed {} files, {} chunks.", stats.files, stats.chunks).green()
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
