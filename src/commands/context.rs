//! Context command — gather repository context for AI/LLM consumption.

use std::io::Write;

use clap::Args;
use colored::Colorize;

use crate::context;
use crate::detect::Detector;
use crate::Globals;

#[derive(Args)]
pub struct ContextArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Include file tree (legacy unicode format)
    #[arg(long)]
    pub tree: bool,

    /// Use simple context mode (less verbose)
    #[arg(long)]
    pub simple: bool,

    /// Only show git information
    #[arg(long = "git-only")]
    pub git_only: bool,

    /// Include code statistics (tokei)
    #[arg(long)]
    pub stats: bool,

    /// Include static analysis (semgrep)
    #[arg(long)]
    pub analysis: bool,

    /// Include dependency vulnerability audit
    #[arg(long)]
    pub audit: bool,

    /// Include everything (stats + analysis + audit)
    #[arg(long)]
    pub full: bool,

    /// Exclude TODO/FIXME comments
    #[arg(long = "no-todos")]
    pub no_todos: bool,

    /// Exclude documentation (README)
    #[arg(long = "no-docs")]
    pub no_docs: bool,

    /// Exclude test patterns
    #[arg(long = "no-tests")]
    pub no_tests: bool,

    /// Exclude import dependency graph
    #[arg(long = "no-graph")]
    pub no_graph: bool,

    /// Exclude project metadata
    #[arg(long = "no-metadata")]
    pub no_metadata: bool,

    /// Write output to file
    #[arg(short = 'o', long = "output")]
    pub output: Option<String>,
}

pub fn run(d: &Detector, _g: &Globals, args: &ContextArgs) -> i32 {
    let base = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {}", e).red());
            return 1;
        }
    };

    let output = match compute(d, &base, args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{}", format!("Error: {}", e).red());
            return 1;
        }
    };

    if let Some(file) = &args.output {
        match std::fs::write(file, &output) {
            Ok(_) => println!("{}", format!("Context written to {}", file).green()),
            Err(e) => {
                eprintln!("{}", format!("Error writing file: {}", e).red());
                return 1;
            }
        }
    } else {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if lock.write_all(output.as_bytes()).is_err() {
            return 1;
        }
    }
    0
}

fn compute(d: &Detector, base: &std::path::Path, args: &ContextArgs) -> anyhow::Result<String> {
    if args.git_only {
        return Ok(match context::gather_git(base) {
            Some(git) => serde_json::to_string_pretty(&git)
                .unwrap_or_else(|_| "Not a git repository".to_string()),
            None => "Not a git repository".to_string(),
        });
    }

    if args.simple {
        let mut out = if args.json {
            serde_json::to_string_pretty(&context::gather_repo_context_json(base))
                .unwrap_or_default()
        } else {
            context::format_simple(base)
        };
        if args.tree && !args.json {
            out.push_str("\n\n## File Tree\n\n```\n");
            out.push_str(&context::generate_tree(base, 3));
            out.push_str("\n```\n");
        }
        return Ok(out);
    }

    let opts = context::ContextOptions {
        stats: args.stats || args.full,
        analysis: args.analysis || args.full,
        audit: args.audit || args.full,
        todos: !args.no_todos,
        docs: !args.no_docs,
        tests: !args.no_tests,
        graph: !args.no_graph,
        metadata: !args.no_metadata,
        patterns: true,
    };

    let mut out = context::gather(d, base, &opts)?;
    if args.json {
        out = serde_json::to_string_pretty(&context::gather_repo_context_json(base))
            .unwrap_or_default();
    } else if args.tree {
        out.push_str("\n\n## File Tree\n\n```\n");
        out.push_str(&context::generate_tree(base, 3));
        out.push_str("\n```\n");
    }
    Ok(out)
}
