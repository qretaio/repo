//! Lint command.

use clap::Args;
use colored::Colorize;

use crate::detect::{Detector, Kind};
use crate::run::{run_commands, RunOptions, Task};
use crate::Globals;

#[derive(Args)]
pub struct LintArgs {
    /// Run auto-fixes where available
    #[arg(long)]
    pub fix: bool,
    /// List detected project types and linters without running
    #[arg(long)]
    pub list: bool,
    /// Run linters for specific project type only
    #[arg(short = 't', long = "type")]
    pub r#type: Option<String>,
}

pub fn run(d: &Detector, g: &Globals, args: &LintArgs) -> i32 {
    if args.list {
        d.list_commands(Kind::Lint, "project types and linters", true);
        return 0;
    }

    let mut detected = d.detect_project_types();
    if let Some(t) = &args.r#type {
        detected.retain(|p| p.id.eq_ignore_ascii_case(t));
        if detected.is_empty() {
            eprintln!(
                "{}",
                format!("Error: Project type '{t}' not detected").red()
            );
            return 1;
        }
    }

    let continue_on_error = args.fix;
    if !args.fix && detected.is_empty() {
        println!("{}", "No known project types detected".yellow());
        return 0;
    }

    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };

    if g.verbose {
        let mut h = String::from("🔍 Linting");
        if let Some(t) = &args.r#type {
            h.push(' ');
            h.push_str(t);
        }
        if g.full {
            h.push_str(" (full scan)");
        }
        println!("{}", h.blue());
    }

    if args.r#type.is_none() {
        let universal: Vec<_> = d
            .get_applicable(d.universal_commands().get(Kind::Lint))
            .into_iter()
            .filter(|c| g.full || !c.heavy)
            .collect();
        if !universal.is_empty() {
            if g.verbose {
                println!("{}", "\nUniversal checks:".blue());
            }
            let tasks: Vec<Task> = universal
                .iter()
                .map(|c| Task::new(&c.name, c.resolve_fix(args.fix)))
                .collect();
            let results = run_commands(&tasks, &opts);
            if !continue_on_error && !results.iter().all(|r| r.success) {
                return 1;
            }
        }
    }

    for project in &detected {
        let commands: Vec<_> = d
            .get_applicable(project.commands.get(Kind::Lint))
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
            .map(|c| Task::new(&c.name, c.resolve_fix(args.fix)))
            .collect();
        let results = run_commands(&tasks, &opts);
        if !continue_on_error && !results.iter().all(|r| r.success) {
            return 1;
        }
    }

    0
}
