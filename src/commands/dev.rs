//! Dev command — start development servers / watchers.
//! Always streams output live (verbose) since dev servers are interactive.

use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
use crate::Globals;
use clap::Args;
use colored::Colorize;

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

    // Dev servers stream output live — always verbose.
    let opts = RunOptions {
        verbose: true,
        ..Default::default()
    };

    println!("{}", "🚀 Starting dev servers".bold());

    for project in &detected {
        let commands = d.get_applicable(project.commands.get(Kind::Dev));
        if commands.is_empty() {
            continue;
        }
        println!("{}", format!("\n{}:", project.name).blue());
        let tasks: Vec<Task> = commands
            .iter()
            .map(|c| Task {
                name: c.name.clone(),
                cmd: c.cmd.clone(),
                cost: c.cost,
            })
            .collect();
        run_commands(&tasks, &opts);
    }

    0
}
