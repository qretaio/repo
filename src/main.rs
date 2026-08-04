//! repo — Universal repository operations CLI.
//! Phase 1: detect + run + lint/fmt/build/test/mix. (context stubbed; Phase 2.)

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use std::process;
mod commands;
mod detect;
mod run;

use commands::{build::BuildArgs, fmt::FmtArgs, lint::LintArgs, mix::MixArgs, test::TestArgs};
use detect::Detector;

/// Global flags propagated to every subcommand.
pub struct Globals {
    pub verbose: bool,
    pub full: bool,
}

const EXAMPLES: &str = "\
Examples:
    repo lint --list              show what would run, run nothing
    repo lint                     run light linters
    repo --full lint              include heavy checks (audit, semgrep, gitleaks)
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
/// any repo. Commands marked heavy (> 5 s) are skipped unless --full is given.
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

    /// Run all commands, including heavy ones (> 5 s).
    ///
    /// Include slow checks skipped by default: security audits (npm/cargo/pip
    /// audit, govulncheck), Semgrep, Knip, Gitleaks/Trivy/Trufflehog, full builds.
    #[arg(long, global = true)]
    full: bool,

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
    /// Full builds are heavy and skipped without --full.
    Build(BuildArgs),

    /// Run tests for detected project types.
    ///
    /// npm test, pytest, cargo test, go test, gradle/maven. Tests always run
    /// in full mode (the heavy filter is ignored).
    Test(TestArgs),

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
        full: cli.full,
    };

    let code = match command {
        Cmd::Lint(a) => commands::lint::run(&detector, &globals, &a),
        Cmd::Fmt(a) => commands::fmt::run(&detector, &globals, &a),
        Cmd::Build(a) => commands::build::run(&detector, &globals, &a),
        Cmd::Test(a) => commands::test::run(&detector, &globals, &a),
        Cmd::Mix(a) => commands::mix::run(&a),
        Cmd::Context => {
            eprintln!("repo context: not yet ported to Rust (planned for Phase 2).");
            1
        }
    };

    process::exit(code);
}
