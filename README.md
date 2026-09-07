# repo

Universal CLI for repository operations — lint, format, build, and test across
multiple programming languages, with auto-detection.

## Features

- **Auto-detection**: detects project types (Node.js, Python, Rust, Go, JVM)
- **Cross-language commands**: `lint`, `fmt`, `build`, `test` work across all detected projects
- **Context gathering**: `context` — AI-ready repo overview (git, structure, symbols, TODOs, optional analysis/audit)
- **Mix**: pack a repo into one AI-friendly file via `repomix`

## Usage

```bash
repo lint              # check light linters
repo --full lint       # all linters (including heavy ones)
repo lint --fix        # auto-fix where available
repo lint --list       # list detected linters without running
repo lint -t rust      # only a specific project type

repo fmt               # apply formatting
repo fmt --check       # check only (CI)

repo build             # build all detected projects
repo build --check     # faster checks (cosmetic header only)

repo test              # run all tests (always "full" mode)
repo test --coverage   # with coverage (if supported)
repo test --watch      # watch mode (if supported)

repo mix               # repomix passthrough
```

## Search

`repo index` / `repo search` build and query a local, definition-aware code index:

- File discovery respects `.gitignore`/`.ignore` rules, skips hidden entries, and
  caps file size at 1 MiB (the `ignore` walker — no `rg` subprocess). The same
  walker feeds the search index, the symbol store, and `context`.
- Chunks are definition-aligned (tree-sitter): one chunk per top-level symbol,
  with the symbol breadcrumb ranked into the token stream. Languages without a
  grammar fall back to overlapping line windows.
- Indexing is incremental: an mtime diff re-chunks and re-embeds only changed,
  added, or removed files. `repo index --force` rebuilds from scratch.
- Retrieval fuses BM25 and dense embeddings (local llama.cpp) with reciprocal
  rank fusion, then cross-encoder reranks. `--lang` / `--path` filter inside
  both stages. `--bm25` forces lexical-only; semantic mode hard-fails when its
  local server is unreachable — never a silent fallback.

## Evaluation & telemetry

Retrieval quality and behavior are measurable and reviewable out of the box:

- `repo eval` — runs the golden queries in `eval.yaml` (this repo ships one)
  through both retrieval modes and reports recall@k, MRR, nDCG@k, and latency
  percentiles per mode. Full reports persist under `~/.cache/repo/evals/`;
  `repo eval --init` writes a starter spec for another repository.
- `repo metrics` — aggregates the local JSONL telemetry log
  (`~/.cache/repo/metrics/`): per-mode query counts and latency percentiles,
  zero-result queries, rerank fallbacks, errors, index-build history, and
  eval trends. `--tail N` shows raw events; `--json` emits the summary.
- `repo search -v` prints each query's stage trace (bm25 / embed / dense /
  rerank timings and candidate counts).

Recording is local-only and on by default; disable with
`observability.enabled: false` in repo.yaml (or `log_queries: false` to keep
only a query hash). Logs rotate at 10 MB.

## MCP server

Run repo as an [MCP](https://modelcontextprotocol.io) server over stdio,
exposing its features directly to AI agents:

```bash
repo mcp
```

Client configuration (opencode, Claude Desktop, Cursor, …) — spawn one server
per workspace; the server resolves the repo root from its working directory:

```json
{
  "mcpServers": {
    "repo": { "command": "repo", "args": ["mcp"] }
  }
}
```

Four tools:

| Tool    | Maps to            | Notes                                              |
| ------- | ------------------ | -------------------------------------------------- |
| `context` | `repo context`   | repo overview; optional stats/analysis/audit/symbols |
| `search`  | `repo search`    | hybrid BM25 + semantic; auto-builds the index       |
| `refs`    | `repo refs`      | symbol defs/impls/methods/importers/references; omit the symbol to list all |
| `task`    | `repo lint/fmt/build/test` | one tool, `kind` parameter; captured per-command results, `isError` on failure |

## Global options

- `--full`: run all commands, including heavy ones (> 5s).
- `-v, --verbose`: show detailed output (headers, live command output).

## Supported project types

| Type    | Detection files                      | Linters             | Formatters             |
| ------- | ------------------------------------ | ------------------- | ---------------------- |
| Node.js | `package.json`                       | ESLint, TypeScript  | Prettier, ESLint --fix |
| Python  | `pyproject.toml`, `requirements.txt` | Ruff, Pylint        | Ruff format, Black     |
| Rust    | `Cargo.toml`                         | Clippy              | rustfmt                |
| Go      | `go.mod`                             | go vet, staticcheck | gofmt                  |
| JVM     | `build.gradle*`, `pom.xml`           | Checkstyle          | Spotless               |

Universal checks (when configured): Semgrep, Knip, Gitleaks, Trivy, Trufflehog,
shellcheck, markdownlint, prettier, dprint.

## Development

```bash
cargo build              # debug build
cargo run -- --help      # run via cargo
cargo fmt                # format
cargo clippy --all-targets -- -D warnings
```

## License

MIT
