//! Command execution module.
//!
//! `run_commands` runs every command concurrently (one thread each) and, after
//! all finish, prints the ✓/✗ summary in input order — mirroring the TS
//! `Promise.allSettled`-then-iterate flow. Built on the `duct` crate.

use std::path::PathBuf;
use std::time::Instant;

use colored::Colorize;
use duct::Expression;

#[derive(Clone, Default)]
pub struct RunOptions {
    pub verbose: bool,
    pub cwd: Option<PathBuf>,
}

/// A named command to execute.
pub struct Task {
    pub name: String,
    pub cmd: Vec<String>,
    pub cost: u32,
}

impl Task {
    pub fn new(name: impl Into<String>, cmd: &[String]) -> Self {
        Self {
            name: name.into(),
            cmd: cmd.to_vec(),
            cost: 0,
        }
    }
}

pub struct RunResult {
    pub name: String,
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub cmd: Vec<String>,
    pub duration_ms: u128,
}

impl RunResult {
    fn failed(task: &Task, reason: String, duration_ms: u128) -> Self {
        Self {
            name: task.name.clone(),
            success: false,
            exit_code: 1,
            stdout: String::new(),
            stderr: reason,
            cmd: task.cmd.clone(),
            duration_ms,
        }
    }

    /// Print the ✓ / ✗+details line for this result.
    fn report(&self) {
        if self.success {
            let timing = format!(" ({:.2}s)", self.duration_ms as f64 / 1000.0).dimmed();
            println!("{} {}{}", "✓".green(), self.name, timing);
            return;
        }
        eprintln!();
        eprintln!("{}", format!("✗ {} failed", self.name).red());
        if self.duration_ms > 0 {
            eprintln!(
                "{}",
                format!("Duration: {:.2}s", self.duration_ms as f64 / 1000.0).dimmed()
            );
        }
        eprintln!("{}", format!("Command: {}", self.cmd.join(" ")).dimmed());
        if self.exit_code != 0 {
            eprintln!("{}", format!("Exit code: {}", self.exit_code).dimmed());
        }
        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            eprintln!("{}", "\nStdout:".dimmed());
            eprintln!("{stdout}");
        }
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            eprintln!("{}", "\nStderr:".dimmed());
            eprintln!("{stderr}");
        }
    }
}

/// Force color in the child even when its stdout is piped.
fn with_color_env(expr: Expression) -> Expression {
    expr.env("FORCE_COLOR", "1")
        .env("COLORTERM", "truecolor")
        .env("CARGO_TERM_COLOR", "always")
}

pub fn run_command(task: &Task, opts: &RunOptions) -> RunResult {
    let start = Instant::now();
    let mut expr = with_color_env(duct::cmd(&task.cmd[0], &task.cmd[1..])).stdin_null();
    if let Some(dir) = &opts.cwd {
        expr = expr.dir(dir);
    }
    // Expensive commands (cost ≥ 10) stream live; cheap ones capture unless --verbose.
    let stream = opts.verbose || task.cost >= 10;
    let expr = if stream {
        expr
    } else {
        expr.stdout_capture().stderr_capture()
    };

    match expr.unchecked().run() {
        Ok(out) => RunResult {
            name: task.name.clone(),
            success: out.status.success(),
            exit_code: out.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            cmd: task.cmd.clone(),
            duration_ms: start.elapsed().as_millis(),
        },
        Err(e) => RunResult::failed(task, e.to_string(), start.elapsed().as_millis()),
    }
}

/// Run many commands concurrently, then print the summary in input order.
pub fn run_commands(tasks: &[Task], opts: &RunOptions) -> Vec<RunResult> {
    let results = std::thread::scope(|s| {
        let handles: Vec<_> = tasks
            .iter()
            .map(|task| s.spawn(|| run_command(task, opts)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| RunResult {
                    name: String::from("unknown"),
                    success: false,
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::from("worker thread panicked"),
                    cmd: vec![],
                    duration_ms: 0,
                })
            })
            .collect::<Vec<_>>()
    });

    for res in &results {
        res.report();
    }
    results
}
