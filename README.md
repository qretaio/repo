# repo

Universal CLI for repository operations — lint, format, build, and test across
multiple programming languages, with auto-detection.

## Features

- **Auto-detection**: detects project types (Node.js, Python, Rust, Go, JVM)
- **Cross-language commands**: `lint`, `fmt`, `build`, `test` work across all detected projects
- **Context gathering**: `context` (Phase 2 — currently stubbed)
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
