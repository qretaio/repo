//! MCP command — run repo as an MCP server over stdio.
//!
//! Client config:
//!   { "mcpServers": { "repo": { "command": "repo", "args": ["mcp"] } } }
//!
//! Spawn the server with the workspace directory as cwd; `Detector::new`
//! (already run by main) resolves the repo root and chdirs there. All logs go
//! to stderr — stdout is reserved for the JSON-RPC protocol stream.

use clap::Args;

use crate::detect::Detector;
use crate::Globals;

#[derive(Args)]
pub struct McpArgs {}

pub fn run(_d: &Detector, _g: &Globals, _args: &McpArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: failed to start async runtime: {e}");
            return 1;
        }
    };

    match rt.block_on(crate::mcp::serve(root)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}
