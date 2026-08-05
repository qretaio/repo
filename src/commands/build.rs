//! Build command.
//! Note: `--check` is cosmetic only (changes the header text); build never
//! selects check_cmd variants — it always runs `cmd`. This mirrors the TS.

use clap::Args;
use colored::Colorize;

use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
use crate::Globals;

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

    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };

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

    for project in &detected {
        let commands: Vec<_> = d
            .get_applicable(project.commands.get(Kind::Build))
            .into_iter()
            .filter(|c| c.cost <= g.cost)
            .collect();
        if commands.is_empty() {
            continue;
        }
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

    0
}
