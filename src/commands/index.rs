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

    let settings = match search::semantic::settings_from_config(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("Error loading semantic config: {e}").red());
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
        }
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    }

    // Build the semantic sidecar (embeddings + model fingerprint) when enabled.
    // `build` re-embeds only when stale or the stored model differs. When
    // semantic is disabled (`enabled: false` in config) this is a no-op.
    if settings.enabled {
        match search::semantic::build(&root, &settings, args.force) {
            Ok(stats) => {
                let msg = if stats.rebuilt {
                    format!("Embedded {} chunks ({} files).", stats.chunks, stats.files)
                } else {
                    format!("Embeddings up to date ({} chunks).", stats.chunks)
                };
                println!("{}", msg.green());
            }
            Err(e) => {
                eprintln!("{}", format!("Error: {e}").red());
                return 1;
            }
        }
    }
    0
}
