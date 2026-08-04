//! Test command.

use clap::Args;
use colored::Colorize;

use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
use crate::Globals;

#[derive(Args)]
pub struct TestArgs {
    /// Run tests in watch mode if supported
    #[arg(long)]
    pub watch: bool,
    /// Run tests with coverage if supported
    #[arg(long)]
    pub coverage: bool,
    /// List test commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, g: &Globals, args: &TestArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Test, "test commands", true);
        return 0;
    }

    // Always run all tests (heavy filter ignored — mirrors getCommandsByType).
    let commands = d.get_commands_by_type(Kind::Test);
    if commands.is_empty() {
        println!("{}", "No testable projects detected".yellow());
        return 0;
    }

    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };

    if g.verbose {
        println!("{}", "🧪 Running tests".bold());
    }

    let tasks: Vec<Task> = commands
        .iter()
        .map(|c| {
            let mut cmd = c.cmd.clone();
            if args.coverage && !cmd.iter().any(|a| a == "--coverage") {
                cmd.push("--coverage".into());
            }
            if args.watch && !cmd.iter().any(|a| a == "--watch") {
                cmd.push("--watch".into());
            }
            Task::new(&c.name, &cmd)
        })
        .collect();

    let results = run_commands(&tasks, &opts);
    if !results.iter().all(|r| r.success) {
        return 1;
    }
    0
}
