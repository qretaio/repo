//! repo — Universal repository operations CLI.
//! Phase 1: detect + run + lint/fmt/build/test/mix. (context stubbed; Phase 2.)

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use std::process;
mod commands;
mod context;
mod detect;
mod mcp;
mod run;
mod search;
mod symbols;
mod tasks;

use commands::{
    build::BuildArgs, context::ContextArgs, fmt::FmtArgs, index::IndexArgs, lint::LintArgs,
    mcp::McpArgs, mix::MixArgs, refs::RefsArgs, run::RunArgs, search::SearchArgs,
    symbols::SymbolsArgs, test::TestArgs,
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

    /// Run the project's program/binary, falling back to task runners.
    ///
    /// Runs built-in entry commands (cargo run, go run ., npm start, …); if
    /// none apply, falls back to tasks defined by npm scripts, justfile, make,
    /// deno, or gradle — trying names `run → start → dev → serve`.
    /// Output always streams live.
    Run(RunArgs),

    /// Pack the repository into a single AI-friendly file (via repomix).
    ///
    /// All trailing arguments are forwarded verbatim to `repomix`.
    Mix(MixArgs),

    /// Gather repository context for AI/LLM consumption.
    ///
    /// Collects git state, project metadata, code rules, dependency graph,
    /// TODOs, file structure, and (optionally) tokei stats, semgrep analysis,
    /// and vulnerability audits into a single AI-friendly document.
    #[command(alias = "ctx")]
    Context(ContextArgs),

    /// Build (or refresh) the ranked search index for this repository.
    ///
    /// Indexes source files into a Tantivy BM25 index under
    /// `~/.cache/repo/index/`. Incremental by mtime; `--force` rebuilds from
    /// scratch. Required once before `repo search` (which also auto-builds on
    /// first use).
    Index(IndexArgs),

    /// Ranked full-text code search (BM25).
    ///
    /// Searches the repository index for function names, API calls, error
    /// messages, etc. Results are ranked by relevance and filtered with
    /// `--lang` / `--path`. Builds the index automatically on first use.
    Search(SearchArgs),

    /// Find symbol definitions and references (exact match).
    ///
    /// Reports where a function, struct, class, etc. is defined, who imports
    /// it, and where it is referenced. Backed by the tree-sitter symbol store
    /// (`repo symbols`); auto-builds on first use.
    Refs(RefsArgs),

    /// Build (or refresh) the tree-sitter symbol store.
    ///
    /// Parses supported source files (Rust, Python, Go, TypeScript, JavaScript)
    /// into a SQLite cache of definitions and import edges under
    /// `~/.cache/repo/symbols/`. Required once before `repo refs` (which also
    /// auto-builds on first use).
    Symbols(SymbolsArgs),

    /// Run as an MCP server over stdio.
    ///
    /// Exposes context/search/refs/task as MCP tools for AI agents. Configure
    /// a client with: {"mcpServers": {"repo": {"command": "repo", "args":
    /// ["mcp"]}}} — the server resolves the repo root from its working
    /// directory.
    Mcp(McpArgs),
}

fn main() -> anyhow::Result<()> {
    human_panic::setup_panic(|| human_panic::Metadata::new("repo", env!("CARGO_PKG_VERSION")));
    let cli = Cli::parse_from(wild::args());

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
        Cmd::Run(a) => commands::run::run(&detector, &globals, &a),
        Cmd::Mix(a) => commands::mix::run(&a),
        Cmd::Context(a) => commands::context::run(&detector, &globals, &a),
        Cmd::Index(a) => commands::index::run(&detector, &globals, &a),
        Cmd::Search(a) => commands::search::run(&detector, &globals, &a),
        Cmd::Refs(a) => commands::refs::run(&detector, &globals, &a),
        Cmd::Symbols(a) => commands::symbols::run(&detector, &globals, &a),
        Cmd::Mcp(a) => commands::mcp::run(&detector, &globals, &a),
    };

    process::exit(code);
}
