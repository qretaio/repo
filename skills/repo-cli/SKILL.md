---
name: repo-cli
description: Rust CLI that auto-detects project types in the current directory and runs the right lint/fmt/build/test for each, across all major language ecosystems (built-in detection is marker-file based; rich boolean detection — AND/OR, source-layout, marker-content — is user-configurable via CEL expressions in `repo.toml`, so new languages need no code change), plus universal checks (Semgrep, Gitleaks, …). Use when the user asks to "lint", "fmt"/"format", "build", or "test" a codebase, or wants unified quality checks across a polyglot project. Note: `repo context` is stubbed (Phase 2).
---

# repo CLI

Auto-detects project types in the current directory and runs the correct tool for
each. Safe in any repo — each command only runs when its own config is present.

**Call `repo help` for the full reference** — it is the single source of truth
(updated there, not here). Drill into a subcommand with `repo help <cmd>` or
`repo <cmd> --help`.

Orientation: `repo <cmd> --list` previews what would run without running it;
`-v`/`--verbose` streams live output; `--full` includes heavy (>5 s) checks;
exit 0 = pass, non-zero = fail.

Detection config: optional `repo.toml` `[[detect]]` rules (CEL `expr`, keyed by
project `id`) override built-in detection or add new project types — full AND/OR
via CEL. See `repo help`.
