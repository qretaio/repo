---
name: repo-cli
description: Rust CLI for repository operations - lint, format, build, test across Node.js/Python/Rust/Go/JVM with auto-detection. Use when the user asks to "lint", "format"/"fmt", "build", or "test" a codebase, or wants unified quality checks across a polyglot project. (Note: `repo context` is stubbed in the Rust port — Phase 2.)
---

# repo CLI

Rust binary (`cargo build -s`). Auto-detects project types and runs the right
tool for each. Exit code 0 = pass, non-zero = fail.

## Commands

```bash
repo lint              # run light linters for all detected projects
repo --full lint       # include heavy checks (audit, semgrep, gitleaks, …)
repo lint --fix        # auto-fix where available
repo lint --list       # list what would run (no execution)
repo lint -t rust      # one project type only

repo fmt               # apply formatting
repo fmt --check       # check only (CI)

repo build             # build all detected projects
repo build --check     # (cosmetic header; build always runs `cmd`)

repo test              # run all tests (always "full")
repo test --coverage   # / --watch if the runner supports it

repo mix               # repomix passthrough
```

## Global options

- `-v, --verbose` — show headers + live command output
- `--full` — include heavy (>5s) commands

## Supported project types

| Type    | Detection                    | Linters             | Formatters     |
| ------- | ---------------------------- | ------------------- | -------------- |
| Node.js | `package.json`               | ESLint, tsc         | Prettier       |
| Python  | `pyproject.toml`, req.txt    | Ruff, Pylint        | Ruff, Black    |
| Rust    | `Cargo.toml`                 | Clippy              | rustfmt        |
| Go      | `go.mod`                     | go vet, staticcheck | gofmt          |
| JVM     | `build.gradle*`, `pom.xml`   | Checkstyle          | Spotless       |

Universal checks (when configured): Semgrep, Knip, Gitleaks, Trivy, Trufflehog,
shellcheck, markdownlint, prettier, dprint.

## `repo context` — NOT YET PORTED

```bash
repo build             # Build all
repo build --check     # Fast check (tsc --noEmit, cargo check)
repo build --list      # List build commands
```

### `repo test` - Run Tests

```bash
repo test              # Run all tests
repo test --watch      # Watch mode
repo test --coverage   # With coverage
```

## Supported Languages

| Type    | Detection                  | Linters             | Formatters  |
| ------- | -------------------------- | ------------------- | ----------- |
| Node.js | `package.json`             | ESLint, tsc         | Prettier    |
| Python  | `pyproject.toml`, req.txt  | Ruff, Pylint        | Ruff, Black |
| Rust    | `Cargo.toml`               | Clippy              | rustfmt     |
| Go      | `go.mod`                   | go vet, staticcheck | gofmt       |
| JVM     | `build.gradle*`, `pom.xml` | Checkstyle          | Spotless    |

## Workflow

```bash
repo fmt --check && repo lint && repo build --check && repo test   # CI gate
```
