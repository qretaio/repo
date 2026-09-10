---
name: repo-cli
description: Rust CLI that auto-detects project types and runs the right lint/fmt/build/test per language; plus `repo context` (AI-ready snapshot), `repo search` (ranked BM25 + semantic), `repo refs`/`repo symbols` (tree-sitter definitions, references, and repo outline), and `repo mcp` (MCP server exposing all of it to agents). Use for polyglot quality gates, exploring a codebase, finding symbol defs/usages, or gathering LLM context.
---

Auto-detects project types and runs the correct tool per language. Safe in any
repo — a command only runs when its own config is present.

**Command groups:**

- **quality** — `lint` `fmt` `build` `test` `run`: per-project, auto-detected.
  Project-defined tasks (mise, npm scripts, justfile, …) take preference;
  security checks and audits with `--cost 10`.
- **understand** — `context` (`ctx`): AI-ready snapshot (git, metadata, deps,
  TODOs, structure). `--full` adds stats/analysis/audit + a tree-sitter symbol map.
- **search** — `index` then `search <q>`: hybrid BM25 + semantic (local
  llama.cpp) ranked search (`--lang` / `--path` / `--json` / `--bm25`).
  Auto-indexes on first use.
- **symbols** — `symbols` builds a tree-sitter store (Rust/Python/Go/TS/JS);
  `refs [symbol]` shows definitions + impl/method hierarchy + importers +
  references (exact; skips comments/strings). No symbol → full repo outline.
- **pack** — `mix`: forward args to `repomix`.
- **mcp** — `repo mcp` runs an MCP server over stdio exposing `context`,
  `search`, `refs`, and `task` (lint/fmt/build/test with captured results) as
  tools. Client config: `{"mcpServers": {"repo": {"command": "repo",
  "args": ["mcp"]}}}` — spawn per workspace; the server resolves the repo
  root from its working directory.

**`repo help` is the source of truth** ; drill in
with `repo help <cmd>` or `repo <cmd> --help`. Orientation: `--list` previews
without running; `-v` streams live output; `--cost 10` includes heavy checks;
exit 0 = pass, non-zero = fail.

Detection is data-driven (embedded `defaults.yaml`); override globally via
`~/.config/repo/repo.yaml` or locally via `./repo.yaml`.
