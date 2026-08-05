//! Install command — install/sync project dependencies.

use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
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

    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };

    let mut ran = false;
    for project in &detected {
        let commands = d.get_applicable(project.commands.get(Kind::Install));
        if commands.is_empty() {
            continue;
        }
        ran = true;
        if g.verbose {
            println!("{}", format!("\n{}:", project.name).blue());
        }
        let tasks: Vec<Task> = commands
            .iter()
            .map(|c| Task {
                name: c.name.clone(),
                cmd: c.cmd.clone(),
                cost: c.cost,
            })
            .collect();
        let results = run_commands(&tasks, &opts);
        if !results.iter().all(|r| r.success) {
            return 1;
        }
    }

    if !ran {
        println!("{}", "No install commands for detected projects".yellow());
    }

    0
}
