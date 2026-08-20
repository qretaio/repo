//! MCP tool implementations (blocking) + their parameter schemas.
//!
//! Each fn takes the resolved repo `root`, constructs a fresh `Detector`
//! (config validation, detection, task-runner discovery — milliseconds), and
//! calls the same library/command cores the CLI uses. Output goes into
//! `CallToolResult`s; stdout is never written (protocol stream).

use std::path::PathBuf;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use crate::commands;
use crate::detect::Detector;
use crate::run::RunOptions;
use crate::search;
use crate::symbols;
use crate::Globals;

/// Cap captured child output per command in `task` results — build/test logs
/// can be enormous and agents rarely need more than the tail context.
const OUTPUT_CAP: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextParams {
    /// Simple mode: branch, last commit, structure summary only.
    #[serde(default)]
    pub simple: bool,
    /// Only git information (JSON).
    #[serde(default)]
    pub git_only: bool,
    /// Include code statistics (tokei).
    #[serde(default)]
    pub stats: bool,
    /// Include static analysis (semgrep, cached).
    #[serde(default)]
    pub analysis: bool,
    /// Include dependency vulnerability audit (npm/cargo audit).
    #[serde(default)]
    pub audit: bool,
    /// Include everything (stats + analysis + audit + symbols).
    #[serde(default)]
    pub full: bool,
    /// Include a tree-sitter symbol map (definitions outline).
    #[serde(default)]
    pub symbols: bool,
    /// Exclude TODO/FIXME comments.
    #[serde(default)]
    pub no_todos: bool,
    /// Exclude documentation (README).
    #[serde(default)]
    pub no_docs: bool,
    /// Exclude test patterns.
    #[serde(default)]
    pub no_tests: bool,
    /// Exclude the import dependency graph.
    #[serde(default)]
    pub no_graph: bool,
    /// Exclude project metadata.
    #[serde(default)]
    pub no_metadata: bool,
    /// Output the structured JSON form instead of markdown.
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Search query: function names, API calls, error strings, concepts.
    pub query: String,
    /// Maximum number of results.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Filter results by language (rust, python, go, typescript, …). BM25-only.
    #[serde(default)]
    pub lang: Option<String>,
    /// Filter results by file-path substring. BM25-only.
    #[serde(default)]
    pub path: Option<String>,
    /// Force pure BM25 (skip semantic retrieval regardless of config).
    #[serde(default)]
    pub bm25: bool,
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefsParams {
    /// Symbol name to look up. Omit to list all available symbols.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Definitions only (skip importers and references).
    #[serde(default)]
    pub defs_only: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskParams {
    /// Which quality gate to run.
    pub kind: TaskKind,
    /// lint only: run auto-fixes where available.
    #[serde(default)]
    pub fix: bool,
    /// fmt/build only: check instead of applying/building.
    #[serde(default)]
    pub check: bool,
    /// lint only: restrict to one detected project type id (e.g. "rust").
    #[serde(default)]
    pub r#type: Option<String>,
    /// Cost threshold: 0 = lightweight only; 10 includes audits/semgrep.
    #[serde(default)]
    pub cost: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Lint,
    Fmt,
    Build,
    Test,
}

impl TaskKind {
    fn label(self) -> &'static str {
        match self {
            TaskKind::Lint => "lint",
            TaskKind::Fmt => "fmt",
            TaskKind::Build => "build",
            TaskKind::Test => "test",
        }
    }
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

pub fn context(root: PathBuf, p: ContextParams) -> anyhow::Result<CallToolResult> {
    let d = Detector::new(root)?;
    let base = std::env::current_dir()?;
    let args = commands::context::ContextArgs {
        json: p.json,
        tree: false,
        simple: p.simple,
        git_only: p.git_only,
        stats: p.stats,
        analysis: p.analysis,
        audit: p.audit,
        full: p.full,
        no_todos: p.no_todos,
        no_docs: p.no_docs,
        no_tests: p.no_tests,
        no_graph: p.no_graph,
        symbols: p.symbols,
        no_metadata: p.no_metadata,
        output: None,
    };
    let out = commands::context::compute(&d, &base, &args)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

pub fn search(root: PathBuf, p: SearchParams) -> anyhow::Result<CallToolResult> {
    Detector::new(root.clone())?; // config validation + chdir parity with the CLI
    let settings = search::semantic::settings_from_config(&root)?;
    let semantic_on = settings.enabled && !p.bm25;

    // Ensure the BM25 index exists (semantic re-embeds only when stale).
    if search::index::is_stale(&root) || !commands::search::index_exists(&root) {
        search::index::build(&root, true)?;
    }

    let (mode, hits) = if semantic_on {
        search::semantic::build(&root, &settings, false)?;
        let hits = search::semantic::search(&root, &p.query, &settings, p.limit)?;
        ("semantic", hits)
    } else {
        let hits = search::index::search(
            &root,
            &p.query,
            p.limit,
            p.lang.as_deref(),
            p.path.as_deref(),
        )?;
        ("bm25", hits)
    };

    let results: Vec<_> = hits
        .iter()
        .map(|h| {
            json!({
                "path": h.path, "start": h.start, "end": h.end,
                "lang": h.lang, "score": h.score, "source": h.source,
            })
        })
        .collect();
    Ok(CallToolResult::structured(json!({
        "mode": mode,
        "results": results,
    })))
}

pub fn refs(root: PathBuf, p: RefsParams) -> anyhow::Result<CallToolResult> {
    Detector::new(root.clone())?;
    commands::refs::ensure_store(&root)?;

    let payload = match p.symbol.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => commands::refs::refs_data(&root, s, p.defs_only)?.to_json(s),
        _ => {
            let all = symbols::all_definitions(&root)?;
            commands::refs::group_list_json(&all)
        }
    };
    Ok(CallToolResult::structured(payload))
}

pub fn task(root: PathBuf, p: TaskParams) -> anyhow::Result<CallToolResult> {
    let d = Detector::new(root)?;
    let g = Globals {
        verbose: false,
        cost: p.cost,
    };
    let opts = RunOptions {
        quiet: true,
        ..Default::default()
    };

    let outcome = match p.kind {
        TaskKind::Lint => commands::lint::core(
            &d,
            &g,
            &commands::lint::LintArgs {
                fix: p.fix,
                list: false,
                r#type: p.r#type.clone(),
            },
            &opts,
        ),
        TaskKind::Fmt => commands::fmt::core(
            &d,
            &g,
            &commands::fmt::FmtArgs {
                check: p.check,
                list: false,
            },
            &opts,
        ),
        TaskKind::Build => commands::build::core(
            &d,
            &g,
            &commands::build::BuildArgs {
                check: p.check,
                list: false,
            },
            &opts,
        ),
        TaskKind::Test => commands::test::core(
            &d,
            &g,
            &commands::test::TestArgs {
                watch: false,
                coverage: false,
                list: false,
            },
            &opts,
        ),
    };

    let payload = json!({
        "kind": p.kind.label(),
        "ok": outcome.exit_code == 0,
        "exit_code": outcome.exit_code,
        "error": outcome.error,
        "commands": outcome.results.iter().map(cmd_json).collect::<Vec<_>>(),
    });
    if outcome.exit_code == 0 {
        Ok(CallToolResult::structured(payload))
    } else {
        Ok(CallToolResult::structured_error(payload))
    }
}

fn cmd_json(r: &crate::run::RunResult) -> serde_json::Value {
    json!({
        "name": r.name,
        "ok": r.success,
        "exit_code": r.exit_code,
        "cmd": r.cmd.join(" "),
        "duration_ms": r.duration_ms,
        "stdout": truncate(&r.stdout),
        "stderr": truncate(&r.stderr),
    })
}

/// Cap a captured output at [`OUTPUT_CAP`] chars (on a char boundary).
fn truncate(s: &str) -> String {
    if s.len() <= OUTPUT_CAP {
        return s.to_string();
    }
    let mut cut = OUTPUT_CAP;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… [truncated, {} of {} chars]",
        &s[..cut],
        s.len() - cut,
        s.len()
    )
}
