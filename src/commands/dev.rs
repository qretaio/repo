//! Dev command — start development servers / watchers.
//! Always streams output live since dev servers are interactive.

use clap::Args;
use colored::Colorize;

use crate::commands::common::live;
use crate::detect::{Detector, Kind};
use crate::Globals;

#[derive(Args)]
pub struct DevArgs {
    /// List dev commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, _g: &Globals, args: &DevArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Dev, "dev commands", false);
        return 0;
    }

    let detected = d.detect_project_types();
    if detected.is_empty() {
        println!("{}", "No projects detected".yellow());
        return 0;
    }

    live(d, &detected, Kind::Dev, "🚀 Starting dev servers")
}
