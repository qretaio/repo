//! Lint command.
//!
//! Preference: if the project defines a `lint` task in any runner (mise task,
//! npm script, justfile recipe, …), run THAT and drop our redundant
//! project-specific
//! linters (ESLint/Clippy/Ruff) — they're what the project's script covers.
//! Security checks (`security: true`: audit/govulncheck/pip-audit) and the
//! universal security linters (Semgrep/Gitleaks/Trivy) always run regardless.

use clap::Args;
use colored::Colorize;

use crate::commands::common::{all_ok, cost_filter, opts, run_group, CoreResult, Mode};
use crate::detect::{Detector, Kind};
use crate::run::RunOptions;
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
        d.list_runners(&["lint"]);
        return 0;
    }

    let r = core(d, g, args, &opts(g));
    if let Some(e) = &r.error {
        eprintln!("{}", format!("Error: {e}").red());
    }
    r.exit_code
}

/// Lint orchestration shared by the CLI and the MCP `task` tool. `opts`
/// controls printing/capture (`quiet` for MCP).
pub fn core(d: &Detector, g: &Globals, args: &LintArgs, opts: &RunOptions) -> CoreResult {
    let mut detected = d.detect_project_types();
    if let Some(t) = &args.r#type {
        detected.retain(|p| p.id.eq_ignore_ascii_case(t));
        if detected.is_empty() {
            return CoreResult {
                exit_code: 1,
                error: Some(format!("Project type '{t}' not detected")),
                results: Vec::new(),
            };
        }
    }

    if !args.fix && detected.is_empty() {
        if !opts.quiet {
            println!("{}", "No known project types detected".yellow());
        }
        return CoreResult {
            exit_code: 0,
            error: None,
            results: Vec::new(),
        };
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

    let mode = Mode::Fix(args.fix);
    // A project-defined `lint` task (mise/npm/just/make/deno/gradle). When present,
    // it replaces the redundant project-specific linters but NOT security.
    let repo_lint = d.runner_cmd("lint");
    let mut results = Vec::new();

    // 1. The project's own lint script first (if any).
    if let Some(cmd) = &repo_lint {
        if g.verbose {
            println!();
            println!("{}", "Project-defined lint:".blue());
        }
        let r = run_group(std::slice::from_ref(cmd), opts, &mode);
        if !all_ok(&r) && !args.fix {
            results.extend(r);
            return CoreResult {
                exit_code: 1,
                error: None,
                results,
            };
        }
        results.extend(r);
    }

    // 2. Universal linters (Semgrep, Gitleaks, Trivy, Knip, …) — always run.
    let universal = cost_filter(d.get_applicable(d.universal_commands().get(Kind::Lint)), g);
    if !universal.is_empty() {
        if g.verbose {
            println!();
            println!("{}", "Universal:".blue());
        }
        let r = run_group(&universal, opts, &mode);
        if !all_ok(&r) && !args.fix {
            results.extend(r);
            return CoreResult {
                exit_code: 1,
                error: None,
                results,
            };
        }
        results.extend(r);
    }

    // 3. Per-project linters. When a project lint task took over, keep only its
    //    security checks (audit/govulncheck); otherwise all applicable linters.
    for project in &detected {
        let mut cmds = d.get_applicable(project.commands.get(Kind::Lint));
        if repo_lint.is_some() {
            cmds.retain(|c| c.security);
        }
        let cmds = cost_filter(cmds, g);
        if cmds.is_empty() {
            continue;
        }
        if g.verbose {
            println!();
            println!("{}", format!("{}:", project.name).blue());
        }
        let r = run_group(&cmds, opts, &mode);
        if !all_ok(&r) && !args.fix {
            results.extend(r);
            return CoreResult {
                exit_code: 1,
                error: None,
                results,
            };
        }
        results.extend(r);
    }

    CoreResult::from_results(results)
}
