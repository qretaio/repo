//! Run command — execute the project's program/binary (cargo run, go run ., …).
//! Always streams output live since running a program is interactive.

use clap::Args;
use colored::Colorize;

use crate::commands::common::live;
use crate::detect::{Detector, Kind};
use crate::Globals;

#[derive(Args)]
pub struct RunArgs {
    /// List run commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, _g: &Globals, args: &RunArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Run, "run commands", false);
        return 0;
    }

    let detected = d.detect_project_types();
    if detected.is_empty() {
        println!("{}", "No projects detected".yellow());
        return 0;
    }

    live(d, &detected, Kind::Run, "▶ Running")
}
