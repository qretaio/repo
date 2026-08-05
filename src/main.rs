//! repo — Universal repository operations CLI.
//! Phase 1: detect + run + lint/fmt/build/test/mix. (context stubbed; Phase 2.)

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use std::process;
mod commands;
mod detect;
mod run;

use commands::{
    build::BuildArgs, dev::DevArgs, fmt::FmtArgs, install::InstallArgs, lint::LintArgs,
    mix::MixArgs, run::RunArgs, test::TestArgs,
};
use detect::Detector;

/// Global flags propagated to every subcommand.
pub struct Globals {
    pub verbose: bool,
    pub cost: u32,
}

const EXAMPLES: &str = "\
Examples:
    repo lint --list              show what would run, run nothing
    repo lint                     run lightweight linters (cost 0)
    repo --cost 10 lint           include expensive checks (audit, semgrep, gitleaks)
    repo lint -t rust             one project type only
    repo lint --fix               auto-fix where possible
    repo fmt --check && repo build --check && repo test    CI gate";

/// Universal CLI for repository operations.
///
/// Runs the correct linter / formatter / builder / test runner for every project
/// type detected in the current directory — Node.js, Python, Rust, Go, and JVM —
/// plus universal checks (Semgrep, Knip, Gitleaks, Trivy, …).
///
/// Detection is marker-file based (package.json, Cargo.toml, pyproject.toml, …)
/// and each command only runs when its config is present, so it is safe to run in
/// any repo. Commands with cost > 0 are skipped unless --cost is given.
#[derive(Parser)]
#[command(
    name = "repo",
    version,
    after_long_help = EXAMPLES
)]
struct Cli {
    /// Global verbose mode - show detailed output.
    ///
    /// Stream each command's output live to the terminal and print section
    /// headers. Without it, output is captured and only shown on failure.
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Maximum cost threshold for command selection (default 0).
    ///
    /// Commands with cost ≤ this value run. Cost 0 (default) selects only
    /// lightweight checks. Use higher values to include expensive ones:
    /// security audits (cost 10), Semgrep (cost 10), full builds, etc.
    #[arg(long, global = true, default_value = "0")]
    cost: u32,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run linters for detected project types.
    ///
    /// Runs each project's linters (ESLint/tsc, Ruff/Mypy/Pylint, Clippy,
    /// go vet/staticcheck, Checkstyle/Detekt) plus universal checks. Exits
    /// non-zero if any check fails; with --fix it auto-fixes and keeps going.
    Lint(LintArgs),

    /// Format source files across detected project types.
    ///
    /// Applies — or checks, with --check — Prettier, Ruff format/Black, rustfmt,
    /// gofmt, Spotless. Never fails the exit code; formatting is best-effort.
    Fmt(FmtArgs),

    /// Run build commands for detected project types.
    ///
    /// npm run build, uv build, cargo build/check, go build, gradle/maven.
    /// Full builds have cost 10 and are skipped without --cost.
    Build(BuildArgs),

    /// Run tests for detected project types.
    ///
    /// npm test, pytest, cargo test, go test, gradle/maven. Tests always run
    /// in full mode (the cost filter is ignored).
    Test(TestArgs),

    /// Install dependencies for detected project types.
    ///
    /// npm install, uv sync, cargo fetch, go mod download, gradle/maven resolve.
    Install(InstallArgs),

    /// Start development servers / watchers.
    ///
    /// npm run dev, cargo run, go run ., etc. Output is always streamed live.
    Dev(DevArgs),

    /// Install dependencies, then start dev servers.
    ///
    /// Runs install commands for each detected project (if any), then starts
    /// dev servers. Shortcut for `repo install && repo dev`.
    Run(RunArgs),

    /// Pack the repository into a single AI-friendly file (via repomix).
    ///
    /// All trailing arguments are forwarded verbatim to `repomix`.
    Mix(MixArgs),

    /// Gather repository context for AI/LLM consumption.
    ///
    /// NOT YET PORTED in the Rust rewrite (Phase 2); exits non-zero. Roll back
    /// to the TS global to use it.
    #[command(alias = "ctx")]
    Context,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        // No subcommand: show help and succeed (help is not an error; avoids
        // tripping shell error traps like zsh TRAPZERR / `set -e` chains).
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let detector = Detector::new(cwd)?;
    let globals = Globals {
        verbose: cli.verbose,
        cost: cli.cost,
    };

    let code = match command {
        Cmd::Lint(a) => commands::lint::run(&detector, &globals, &a),
        Cmd::Fmt(a) => commands::fmt::run(&detector, &globals, &a),
        Cmd::Build(a) => commands::build::run(&detector, &globals, &a),
        Cmd::Test(a) => commands::test::run(&detector, &globals, &a),
        Cmd::Install(a) => commands::install::run(&detector, &globals, &a),
        Cmd::Dev(a) => commands::dev::run(&detector, &globals, &a),
        Cmd::Run(a) => commands::run::run(&detector, &globals, &a),
        Cmd::Mix(a) => commands::mix::run(&a),
        Cmd::Context => {
            eprintln!("repo context: not yet ported to Rust (planned for Phase 2).");
            1
        }
    };

    process::exit(code);
}
