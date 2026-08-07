---
name: repo-cli
description: Rust CLI that auto-detects project types and runs the right lint/fmt/build/test per language; plus `repo context` (AI-ready snapshot), `repo search` (ranked BM25), and `repo refs`/`repo symbols` (tree-sitter definitions, references, and repo outline). Use for polyglot quality gates, exploring a codebase, finding symbol defs/usages, or gathering LLM context.
---

Auto-detects project types and runs the correct tool per language. Safe in any
repo — a command only runs when its own config is present.

**Command groups:**

- **quality** — `lint` `fmt` `build` `test` `install` `dev` `run`: per-project,
  auto-detected. Universal checks (Semgrep, Gitleaks, …) with `--full`.
- **understand** — `context` (`ctx`): AI-ready snapshot (git, metadata, deps,
  TODOs, structure). `--full` adds stats/analysis/audit + a tree-sitter symbol map.
- **search** — `index` then `search <q>`: ranked BM25 over the codebase
  (`--lang` / `--path` / `--json`). Auto-indexes on first use.
- **symbols** — `symbols` builds a tree-sitter store (Rust/Python/Go/TS/JS);
  `refs [symbol]` shows definitions + impl/method hierarchy + importers +
  references (exact; skips comments/strings). No symbol → full repo outline.
- **pack** — `mix`: forward args to `repomix`.

**`repo help` is the source of truth** ;
