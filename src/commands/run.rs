//! Run command — execute the project's program/binary.
//!
//! Runs the built-in entry commands (cargo run, go run ., npm start, …). When
//! no built-in command applies for a detected project (or nothing is detected),
//! falls back to tasks defined by the available runners — trying names
//! `run → start → dev → serve` in priority order `just → make → deno → npm →
//! gradle`. Output always streams live.

use clap::Args;
use colored::Colorize;

use crate::commands::common::live;
use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
use crate::Globals;

/// Candidate task names for the runner fallback, in preference order.
const FALLBACK_NAMES: &[&str] = &["run", "start", "dev", "serve"];

#[derive(Args)]
pub struct RunArgs {
    /// List run commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, _g: &Globals, args: &RunArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Run, "run commands", false);
        d.list_runners(FALLBACK_NAMES);
        return 0;
    }

    let detected = d.detect_project_types();

    // Built-in run commands: run them live (one stream per project).
    let has_builtin = detected
        .iter()
        .any(|p| !d.get_applicable(p.commands.get(Kind::Run)).is_empty());

    if has_builtin {
        return live(d, &detected, Kind::Run, "▶ Running");
    }

    // No built-in command applies — fall back to task runners.
    let Some(found) = d.task_runners().find_any(FALLBACK_NAMES) else {
        if detected.is_empty() {
            println!("{}", "No projects detected".yellow());
        }
        println!(
            "{}",
            "No run task found. Define one via npm scripts, justfile, make, deno, or gradle."
                .yellow()
        );
        return 0;
    };

    println!("{}", "▶ Running".bold());
    println!("{}", format!("\n{}:", found.runner.label()).blue());
    let task = Task {
        name: found.display(),
        cmd: found.argv,
        cost: 0,
    };
    let opts = RunOptions {
        verbose: true,
        ..Default::default()
    };
    run_commands(std::slice::from_ref(&task), &opts);
    0
}
