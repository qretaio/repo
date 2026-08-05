//! Install command — install/sync project dependencies.

use crate::commands::common::{execute, Mode, Plan};
use crate::detect::{Detector, Kind};
use crate::Globals;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct InstallArgs {
    /// List install commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, g: &Globals, args: &InstallArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Install, "install commands", false);
        return 0;
    }

    let detected = d.detect_project_types();
    if detected.is_empty() {
        println!("{}", "No projects detected".yellow());
        return 0;
    }

    let plan = Plan {
        kind: Kind::Install,
        include_universal: false,
        cost_filter: false,
        continue_on_error: false,
        mode: Mode::Normal,
    };
    let (code, ran) = execute(d, g, &detected, &plan);
    if code == 0 && !ran {
        println!("{}", "No install commands for detected projects".yellow());
    }
    code
}
