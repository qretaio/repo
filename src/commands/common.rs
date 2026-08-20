//! Shared command runner — kills the per-project run-loop boilerplate
//! duplicated across lint/fmt/build/install.
//!
//! `test` and `dev` have genuinely different shapes (flat list + flag injection;
//! forced-live streaming) and intentionally do not use this.

use colored::Colorize;

use crate::detect::{command_for_mode, CommandDef, Detector, Kind, ProjectType};
use crate::run::{run_commands, RunOptions, RunResult, Task};
use crate::Globals;

/// How to resolve a `CommandDef` into the argv to actually run.
pub enum Mode {
    /// Use `cmd` verbatim.
    Normal,
    /// Lint-style: `fix_cmd` when fixing, else `cmd`.
    Fix(bool),
    /// Fmt-style: `check_cmd` when checking, else `fix_cmd`/`cmd`.
    Check(bool),
}

impl Mode {
    fn resolve(&self, c: &CommandDef) -> Vec<String> {
        match self {
            Mode::Normal => c.cmd.clone(),
            Mode::Fix(fix) => c.resolve_fix(*fix).to_vec(),
            Mode::Check(check) => command_for_mode(c, *check).to_vec(),
        }
    }
}

/// Knobs describing how to run one command kind.
pub struct Plan {
    pub kind: Kind,
    pub include_universal: bool,
    pub cost_filter: bool,
    pub continue_on_error: bool,
    pub mode: Mode,
}

/// Result of a command core: process exit code plus every executed command's
/// captured result (empty stdout/stderr when not quiet — `run_commands`
/// captures for cheap tasks regardless).
pub struct CoreResult {
    pub exit_code: i32,
    /// Human-readable failure reason for pre-execution errors (e.g. unknown
    /// project-type filter); `None` once commands actually ran.
    pub error: Option<String>,
    pub results: Vec<crate::run::RunResult>,
}

impl CoreResult {
    /// Derive the exit code from the results (1 when any command failed).
    pub fn from_results(results: Vec<RunResult>) -> Self {
        let exit_code = if all_ok(&results) { 0 } else { 1 };
        Self {
            exit_code,
            error: None,
            results,
        }
    }
}

/// True when every command in the group succeeded.
pub fn all_ok(results: &[crate::run::RunResult]) -> bool {
    results.iter().all(|r| r.success)
}

/// Run universal (optional) + per-project commands for `plan.kind`.
///
/// `detected` is the (possibly filtered) project list, owned by the caller so
/// commands like `lint -t rust` can pre-filter. `opts` controls quiet/capture
/// behavior (CLI passes `opts(g)`, MCP passes a quiet variant).
pub fn execute(
    d: &Detector,
    g: &Globals,
    detected: &[&ProjectType],
    plan: &Plan,
    opts: &RunOptions,
) -> CoreResult {
    let mut ran = false;
    let mut results = Vec::new();

    if plan.include_universal {
        let universal: Vec<CommandDef> = select(
            d.get_applicable(d.universal_commands().get(plan.kind)),
            g,
            plan,
        );
        if !universal.is_empty() {
            ran = true;
            if g.verbose {
                println!("{}", "\nUniversal:".blue());
            }
            let r = run_group(&universal, opts, &plan.mode);
            if !all_ok(&r) && !plan.continue_on_error {
                results.extend(r);
                return CoreResult {
                    exit_code: 1,
                    error: None,
                    results,
                };
            }
            results.extend(r);
        }
    }

    for project in detected {
        let commands: Vec<CommandDef> =
            select(d.get_applicable(project.commands.get(plan.kind)), g, plan);
        if commands.is_empty() {
            continue;
        }
        ran = true;
        if g.verbose {
            println!("{}", format!("\n{}:", project.name).blue());
        }
        let r = run_group(&commands, opts, &plan.mode);
        if !all_ok(&r) && !plan.continue_on_error {
            results.extend(r);
            return CoreResult {
                exit_code: 1,
                error: None,
                results,
            };
        }
        results.extend(r);
    }

    if !ran {
        CoreResult {
            exit_code: 0,
            error: None,
            results,
        }
    } else {
        CoreResult::from_results(results)
    }
}

/// Apply the cost filter (if enabled) to a command list.
fn select(cmds: Vec<CommandDef>, g: &Globals, plan: &Plan) -> Vec<CommandDef> {
    if plan.cost_filter {
        cmds.into_iter().filter(|c| c.cost <= g.cost).collect()
    } else {
        cmds
    }
}

/// Drop commands whose `cost` exceeds the global threshold. For handlers that
/// build their own command lists (lint/fmt preference).
pub fn cost_filter(cmds: Vec<CommandDef>, g: &Globals) -> Vec<CommandDef> {
    cmds.into_iter().filter(|c| c.cost <= g.cost).collect()
}

/// Build run options matching the rest of the CLI's streaming rules.
pub fn opts(g: &Globals) -> RunOptions {
    RunOptions {
        verbose: g.verbose,
        ..Default::default()
    }
}

/// Run one group of commands, resolving argv via `mode`. Returns the captured
/// results (already reported by `run_commands` unless quiet). Public so
/// lint/fmt can drive their preference-ordered groups.
pub fn run_group(cmds: &[CommandDef], opts: &RunOptions, mode: &Mode) -> Vec<RunResult> {
    let tasks: Vec<Task> = cmds
        .iter()
        .map(|c| Task {
            name: c.name.clone(),
            cmd: mode.resolve(c),
            cost: c.cost,
        })
        .collect();
    run_commands(&tasks, opts)
}

/// Live-run every detected project's commands for `kind` (dev servers / program
/// execution). Output always streams; no cost filter; never fails the exit code.
/// Shared by `dev` and `run` since their only real difference is the kind.
pub fn live(d: &Detector, detected: &[&ProjectType], kind: Kind, header: &str) -> i32 {
    let opts = RunOptions {
        verbose: true,
        ..Default::default()
    };
    println!("{}", header.bold());
    for project in detected {
        let commands = d.get_applicable(project.commands.get(kind));
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
