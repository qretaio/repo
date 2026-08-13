//! Format command.
//!
//! Preference: if the project defines a `fmt`/`format` task in any runner, run
//! ONLY that (the project knows how it wants to be formatted). Otherwise fall
//! back to the built-in formatters as before. Never fails the exit code.

use clap::Args;
use colored::Colorize;

use crate::commands::common::{cost_filter, opts, run_group, Mode};
use crate::detect::{Detector, Kind};
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
        d.list_runners(&["fmt", "format"]);
        return 0;
    }

    let detected = d.detect_project_types();

    if g.verbose {
        let h = format!(
            "{} across projects{}",
            if args.check {
                "🔍 Checking formatting"
            } else {
                "✨ Formatting"
            },
            if g.cost > 0 {
                format!(" (cost ≤ {})", g.cost)
            } else {
                String::new()
            }
        );
        println!("{}", h.bold());
    }

    let opts = opts(g);
    let mode = Mode::Check(args.check);

    // A project-defined `fmt` (or `format`) task wins outright — run only it.
    let repo_fmt = d.runner_cmd("fmt").or_else(|| d.runner_cmd("format"));
    if let Some(cmd) = &repo_fmt {
        if g.verbose {
            println!();
            println!("{}", "Project-defined format:".blue());
        }
        run_group(std::slice::from_ref(cmd), &opts, &mode);
        return 0;
    }

    // No project formatter: built-in universal + per-project (best-effort).
    let universal = cost_filter(d.get_applicable(d.universal_commands().get(Kind::Fmt)), g);
    let mut ran = false;
    if !universal.is_empty() {
        ran = true;
        if g.verbose {
            println!();
            println!("{}", "Universal:".blue());
        }
        run_group(&universal, &opts, &mode);
    }

    for project in &detected {
        let cmds = cost_filter(d.get_applicable(project.commands.get(Kind::Fmt)), g);
        if cmds.is_empty() {
            continue;
        }
        ran = true;
        if g.verbose {
            println!();
            println!("{}", format!("{}:", project.name).blue());
        }
        run_group(&cmds, &opts, &mode);
    }

    if !ran {
        println!("{}", "No formatters configured".yellow());
    }
    0
}
