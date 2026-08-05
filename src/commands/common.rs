//! Shared command runner — kills the per-project run-loop boilerplate
//! duplicated across lint/fmt/build/install.
//!
//! `test` and `dev` have genuinely different shapes (flat list + flag injection;
//! forced-live streaming) and intentionally do not use this.

use colored::Colorize;

use crate::detect::{command_for_mode, CommandDef, Detector, Kind, ProjectType};
use crate::run::{run_commands, RunOptions, Task};
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

/// Run universal (optional) + per-project commands for `plan.kind`.
///
/// `detected` is the (possibly filtered) project list, owned by the caller so
/// commands like `lint -t rust` can pre-filter. Returns `(exit_code, ran)`
/// where `ran` is true if at least one command group actually executed.
pub fn execute(d: &Detector, g: &Globals, detected: &[&ProjectType], plan: &Plan) -> (i32, bool) {
    let opts = RunOptions {
        verbose: g.verbose,
        ..Default::default()
    };
    let mut ran = false;

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
            if !run_group(&universal, &opts, &plan.mode) && !plan.continue_on_error {
                return (1, ran);
            }
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
        if !run_group(&commands, &opts, &plan.mode) && !plan.continue_on_error {
            return (1, ran);
        }
    }

    (0, ran)
}

/// Apply the cost filter (if enabled) to a command list.
fn select(cmds: Vec<CommandDef>, g: &Globals, plan: &Plan) -> Vec<CommandDef> {
    if plan.cost_filter {
        cmds.into_iter().filter(|c| c.cost <= g.cost).collect()
    } else {
        cmds
    }
}

fn run_group(cmds: &[CommandDef], opts: &RunOptions, mode: &Mode) -> bool {
    let tasks: Vec<Task> = cmds
        .iter()
        .map(|c| Task {
            name: c.name.clone(),
            cmd: mode.resolve(c),
            cost: c.cost,
        })
        .collect();
    run_commands(&tasks, opts).iter().all(|r| r.success)
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
