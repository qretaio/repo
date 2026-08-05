//! Build command.
//! Note: `--check` is cosmetic only (changes the header text); build never
//! selects check_cmd variants — it always runs `cmd`. This mirrors the TS.

use crate::commands::common::{execute, Mode, Plan};
use crate::detect::{Detector, Kind};
use crate::Globals;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct BuildArgs {
    /// Run check/verify commands instead of full build (cosmetic; runs `cmd`)
    #[arg(long)]
    pub check: bool,
    /// List build commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, g: &Globals, args: &BuildArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Build, "build commands", false);
        return 0;
    }

    let detected = d.detect_project_types();
    if detected.is_empty() {
        println!("{}", "No buildable projects detected".yellow());
        return 0;
    }

    if g.verbose {
        let h = format!(
            "{} projects{}",
            if args.check {
                "🔍 Checking"
            } else {
                "🔨 Building"
            },
            if g.cost > 0 {
                format!(" (cost ≤ {})", g.cost)
            } else {
                String::new()
            }
        );
        println!("{}", h.bold());
    }

    let plan = Plan {
        kind: Kind::Build,
        include_universal: false,
        cost_filter: true,
        continue_on_error: false,
        mode: Mode::Normal,
    };
    execute(d, g, &detected, &plan).0
}
