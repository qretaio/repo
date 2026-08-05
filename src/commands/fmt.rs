//! Format command.

use clap::Args;
use colored::Colorize;

use crate::commands::common::{execute, Mode, Plan};
use crate::detect::{Detector, Kind};
use crate::Globals;

#[derive(Args)]
pub struct FmtArgs {
    /// Check formatting without applying fixes
    #[arg(long)]
    pub check: bool,
    /// List formatting commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, g: &Globals, args: &FmtArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Fmt, "formatters", true);
        return 0;
    }

    let detected = d.detect_project_types();

    if g.verbose {
        let h = format!(
            "{} across projects{}",
            if args.check {
                "🔍 Checking formatting"
            } else {
                "✨ Formatting"
            },
            if g.cost > 0 {
                format!(" (cost ≤ {})", g.cost)
            } else {
                String::new()
            }
        );
        println!("{}", h.bold());
    }

    // Formatting is best-effort: never fail the exit code.
    let plan = Plan {
        kind: Kind::Fmt,
        include_universal: true,
        cost_filter: true,
        continue_on_error: true,
        mode: Mode::Check(args.check),
    };
    let (_, ran) = execute(d, g, &detected, &plan);
    if !ran {
        println!("{}", "No formatters configured".yellow());
    }
    0
}
