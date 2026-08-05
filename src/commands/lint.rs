//! Lint command.

use clap::Args;
use colored::Colorize;

use crate::commands::common::{execute, Mode, Plan};
use crate::detect::{Detector, Kind};
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

    if !args.fix && detected.is_empty() {
        println!("{}", "No known project types detected".yellow());
        return 0;
    }

    if g.verbose {
        let mut h = String::from("🔍 Linting");
        if let Some(t) = &args.r#type {
            h.push(' ');
            h.push_str(t);
        }
        if g.cost > 0 {
            h.push_str(&format!(" (cost ≤ {})", g.cost));
        }
        println!("{}", h.blue());
    }

    let plan = Plan {
        kind: Kind::Lint,
        include_universal: args.r#type.is_none(),
        cost_filter: true,
        continue_on_error: args.fix,
        mode: Mode::Fix(args.fix),
    };
    execute(d, g, &detected, &plan).0
}
