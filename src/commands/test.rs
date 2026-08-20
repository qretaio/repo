//! Test command.

use clap::Args;
use colored::Colorize;

use crate::commands::common::{all_ok, CoreResult};
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

    core(
        d,
        g,
        args,
        &RunOptions {
            verbose: g.verbose,
            ..Default::default()
        },
    )
    .exit_code
}

/// Test orchestration shared by the CLI and the MCP `task` tool. Always runs
/// all tests (cost filter ignored — mirrors getCommandsByType).
pub fn core(d: &Detector, g: &Globals, args: &TestArgs, opts: &RunOptions) -> CoreResult {
    let commands = d.get_commands_by_type(Kind::Test);
    if commands.is_empty() {
        if !opts.quiet {
            println!("{}", "No testable projects detected".yellow());
        }
        return CoreResult {
            exit_code: 0,
            error: None,
            results: Vec::new(),
        };
    }

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
            Task {
                name: c.name.clone(),
                cmd,
                cost: c.cost,
            }
        })
        .collect();

    let results = run_commands(&tasks, opts);
    let exit_code = if all_ok(&results) { 0 } else { 1 };
    CoreResult {
        exit_code,
        error: None,
        results,
    }
}
