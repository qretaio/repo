//! MCP server (`repo mcp`) — expose repo's features as MCP tools over stdio.
//!
//! Four tools: `context`, `search`, `refs`, `task`. Every handler builds a
//! fresh `Detector` inside `spawn_blocking` (detection is cheap file reads +
//! CEL evals, and per-call construction means marker-file changes are picked
//! up mid-session). Blocking work never touches the async runtime threads.
//!
//! Protocol safety: nothing in the tool paths may write to stdout — the
//! command cores run with `RunOptions { quiet: true }` and the library layers
//! (`search`, `symbols`, `context`) are print-free. Stderr is allowed and used
//! for notes ("Building index…", "Indexed N symbols…").

pub mod tools;

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::tool_router;
use rmcp::{tool, ServerHandler, ServiceExt};

use crate::mcp::tools::{ContextParams, RefsParams, SearchParams, TaskParams};

pub struct RepoServer {
    /// Repository root (resolved once at startup by `Detector::new`'s chdir).
    root: PathBuf,
}

#[tool_router]
impl RepoServer {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[tool(
        name = "context",
        description = "Gather repository context for AI/LLM consumption: git state, project metadata, code rules, import graph, TODOs, file structure, and optionally stats (tokei), analysis (semgrep), audit (vulnerabilities), and a tree-sitter symbol outline. Use for a repo overview before exploring code."
    )]
    async fn context(
        &self,
        Parameters(p): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |root| tools::context(root, p)).await
    }

    #[tool(
        name = "search",
        description = "Ranked code search: function names, API calls, error strings, or concepts. Hybrid 3-stage retrieval (BM25 + dense embeddings + cross-encoder rerank) via local llama.cpp when semantic is enabled (the default), pure BM25 with bm25=true or when disabled. Auto-builds the index on first use. Prefer this over grep for exploratory questions."
    )]
    async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |root| tools::search(root, p)).await
    }

    #[tool(
        name = "refs",
        description = "Symbol navigation: given a symbol name, its definitions, impl blocks (with nested methods), methods, importers, and references (comment/string occurrences excluded). Omit the symbol to list every available symbol with kinds. Backed by the tree-sitter symbol store; auto-built on first use."
    )]
    async fn refs(
        &self,
        Parameters(p): Parameters<RefsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |root| tools::refs(root, p)).await
    }

    #[tool(
        name = "task",
        description = "Run a quality gate across detected project types: kind=lint (with optional fix=true, type=<project-id>), kind=fmt (check=true to check only), kind=build (check=true for the faster variant), kind=test. Returns per-command results with captured stdout/stderr. cost>0 includes expensive checks (audits, semgrep). Project-defined tasks (npm scripts, justfile) take preference over built-ins, mirroring the repo CLI."
    )]
    async fn task(
        &self,
        Parameters(p): Parameters<TaskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |root| tools::task(root, p)).await
    }
}

impl RepoServer {
    /// Run a blocking tool implementation on the blocking pool; map panics and
    /// infrastructure errors to protocol errors.
    async fn run<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(PathBuf) -> anyhow::Result<CallToolResult> + Send + 'static,
    {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || f(root))
            .await
            .map_err(|e| ErrorData::internal_error(format!("tool worker panicked: {e}"), None))?
            .map_err(|e| ErrorData::internal_error(format!("{e}"), None))
    }
}

/// Serve on stdio until the client disconnects.
pub async fn serve(root: PathBuf) -> anyhow::Result<()> {
    let service = RepoServer::new(root)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("failed to start MCP server: {e:?}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server terminated: {e:?}"))?;
    Ok(())
}

// `server_handler` is not used on the tool_router above so we can set our own
// server name here (the auto-generated get_info reports "rmcp" otherwise).
#[rmcp::tool_handler(name = "repo")]
impl ServerHandler for RepoServer {}
