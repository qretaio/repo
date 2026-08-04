//! Format command.

use clap::Args;
use colored::Colorize;

use crate::detect::{command_for_mode, Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
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
    let universal: Vec<_> = d
        .get_applicable(d.universal_commands().get(Kind::Fmt))
        .into_iter()
        .filter(|c| g.full || !c.heavy)
        .collect();

    if detected.is_empty() && universal.is_empty() {
        println!("{}", "No formatters configured".yellow());
        return 0;
    }

    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };

    if g.verbose {
        let h = format!(
            "{} across projects{}",
            if args.check {
                "🔍 Checking formatting"
            } else {
                "✨ Formatting"
            },
            if g.full { " (full)" } else { "" }
        );
        println!("{}", h.bold());
    }

    if !universal.is_empty() {
        if g.verbose {
            println!("{}", "\nUniversal:".blue());
        }
        let tasks: Vec<Task> = universal
            .iter()
            .map(|c| Task::new(&c.name, command_for_mode(c, args.check)))
            .collect();
        run_commands(&tasks, &opts);
    }

    for project in &detected {
        let commands: Vec<_> = d
            .get_applicable(project.commands.get(Kind::Fmt))
            .into_iter()
            .filter(|c| g.full || !c.heavy)
            .collect();
        if commands.is_empty() {
            continue;
        }
        if g.verbose {
            println!("{}", format!("\n{}:", project.name).blue());
        }
        let tasks: Vec<Task> = commands
            .iter()
            .map(|c| Task::new(&c.name, command_for_mode(c, args.check)))
            .collect();
        run_commands(&tasks, &opts);
    }

    0
}
