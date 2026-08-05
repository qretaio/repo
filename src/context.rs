//! Repository context gathering for AI/LLM consumption.
//!
//! Ports the TS `gatherFullContext`: a set of independent gatherers run in
//! parallel (thread scope) and their markdown sections are concatenated into a
//! single document. Tool-backed gatherers shell out to fd/rg/tokei/semgrep/audit
//! — degrading gracefully (a "Skipped:" note) when a tool is missing or fails.

use crate::detect::Detector;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ContextOptions {
    pub stats: bool,
    pub analysis: bool,
    pub audit: bool,
    pub todos: bool,
    pub docs: bool,
    pub tests: bool,
    pub graph: bool,
    pub metadata: bool,
    pub patterns: bool,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            stats: false,
            analysis: false,
            audit: false,
            todos: true,
            docs: true,
            tests: true,
            graph: true,
            metadata: true,
            patterns: true,
        }
    }
}

/// Detected-language flags, built from the Detector's project types plus a
/// tsconfig probe. Mirrors the TS `ProjectTypeFlags` (the subset we need).
#[derive(Clone, Copy)]
struct Flags {
    rust: bool,
    go: bool,
    node: bool,
    python: bool,
    java: bool,
    typescript: bool,
}

impl Flags {
    fn from(detector: &Detector, base: &Path) -> Self {
        let ids: HashSet<&str> = detector
            .detect_project_types()
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        Self {
            rust: ids.contains("rust"),
            go: ids.contains("go"),
            node: ids.contains("nodejs"),
            python: ids.contains("python"),
            java: ids.contains("jvm"),
            typescript: base.join("tsconfig.json").exists(),
        }
    }

    fn project_type(self) -> &'static str {
        if self.rust {
            "rust"
        } else if self.go {
            "go"
        } else if self.python {
            "python"
        } else if self.typescript {
            "typescript"
        } else if self.node {
            "javascript"
        } else if self.java {
            "java"
        } else {
            "unknown"
        }
    }
}

// ============ helpers ============

fn read_rel(base: &Path, rel: &str) -> Option<String> {
    fs::read_to_string(base.join(rel)).ok()
}

fn read_json(base: &Path, rel: &str) -> Option<Value> {
    read_rel(base, rel).and_then(|s| serde_json::from_str(&s).ok())
}

fn basename(base: &Path) -> String {
    base.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string())
}

/// Run a command silently, returning (success, stdout). Never panics.
fn capture(program: &str, args: &[&str], cwd: &Path) -> (bool, String) {
    match duct::cmd(program, args)
        .dir(cwd)
        .stdin_null()
        .stdout_capture()
        .stderr_capture()
        .unchecked()
        .run()
    {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// Run a command, returning trimmed stdout only on success.
fn capture_ok(program: &str, args: &[&str], cwd: &Path) -> Option<String> {
    let (ok, out) = capture(program, args, cwd);
    let trimmed = out.trim().to_string();
    (ok && !trimmed.is_empty()).then_some(trimmed)
}

fn bin_exists(name: &str) -> bool {
    which::which(name).is_ok()
}

/// Brace-expand a list of sibling names: `["a.rs","b.rs"]` → "{a,b}.rs".
fn brace(files: &[String]) -> String {
    if files.len() == 1 {
        return files[0].clone();
    }
    let exts: HashSet<&str> = files
        .iter()
        .filter_map(|f| f.rsplit_once('.').map(|(_, e)| e))
        .collect();
    if exts.len() == 1 && files.iter().all(|f| f.contains('.')) {
        let ext = exts.iter().next().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f[..f.len() - ext.len() - 1].to_string())
            .collect();
        format!("{{{}}}.{}", names.join(","), ext)
    } else {
        format!("{{{}}}", files.join(","))
    }
}

#[derive(Serialize)]
pub struct GitContext {
    pub branch: String,
    pub remote_url: Option<String>,
    pub last_commit: Option<String>,
    pub recent_commits: Vec<String>,
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

/// Strip embedded credentials (`scheme://user:pass@host` → `scheme://host`)
/// so a remote URL with a token never leaks into context output / LLM prompts.
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        if let Some(at) = url[scheme_end + 3..].find('@') {
            let at = scheme_end + 3 + at;
            return format!("{}{}", &url[..scheme_end + 3], &url[at + 1..]);
        }
    }
    url.to_string()
}

pub fn gather_git(base: &Path) -> Option<GitContext> {
    if !base.join(".git").exists() {
        return None;
    }
    let branch = capture_ok("git", &["branch", "--show-current"], base)?;
    let remote_url =
        capture_ok("git", &["remote", "get-url", "origin"], base).map(|u| redact_url(&u));
    let last_commit = capture_ok("git", &["log", "-1", "--format=%s"], base);
    let recent = capture_ok(
        "git",
        &[
            "log",
            "-10",
            "--format=%h %s",
            "--abbrev=4",
            "--no-decorate",
        ],
        base,
    )
    .map(|s| {
        s.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    let (mut staged, mut modified, mut untracked) = (0usize, 0usize, 0usize);
    if let Some(status) = capture_ok("git", &["status", "--short"], base) {
        for line in status.lines() {
            if line.starts_with("M ") || line.starts_with("A ") || line.starts_with("D ") {
                staged += 1;
            } else if line.starts_with("??") {
                untracked += 1;
            } else if !line.trim().is_empty() {
                modified += 1;
            }
        }
    }

    Some(GitContext {
        branch,
        remote_url,
        last_commit,
        recent_commits: recent,
        staged,
        modified,
        untracked,
    })
}

fn format_git(git: &GitContext) -> String {
    let mut lines = vec!["## Git Context".to_string()];
    lines.push(format!("// Branch: {}", git.branch));
    if let Some(c) = &git.last_commit {
        lines.push(format!("// Last commit: {}", c));
    }
    if git.staged + git.modified > 0 {
        lines.push(format!(
            "// Active changes: {} staged, {} modified",
            git.staged, git.modified
        ));
    }
    if !git.recent_commits.is_empty() {
        lines.push(String::new());
        lines.push("$ git log -10 --oneline --no-decorate".to_string());
        lines.extend(git.recent_commits.clone());
    }
    lines.join("\n")
}

fn py_runtime(base: &Path) -> Option<String> {
    if let Some(v) = read_rel(base, ".python-version") {
        return Some(format!("Python {}", v.trim()));
    }
    if let Some(p) = read_rel(base, "pyproject.toml") {
        if let Some(caps) = Regex::new(r#"python\s*=\s*["']([^"']+)["']"#)
            .ok()?
            .captures(&p)
        {
            return Some(format!("Python {}", &caps[1]));
        }
    }
    Some("Python".to_string())
}

fn node_runtime(base: &Path) -> Option<String> {
    read_json(base, "package.json")
        .and_then(|p| p.get("engines").and_then(|e| e.get("node")).cloned())
        .map(|n| format!("Node.js {}", n.as_str().unwrap_or("?")))
        .or(Some("Node.js".to_string()))
}

fn detect_arch(base: &Path, flags: Flags) -> String {
    let mut arch: Vec<&str> = vec![];
    let entries = [
        "src/cli.ts",
        "src/main.rs",
        "cmd/main.go",
        "main.py",
        "src/main.py",
    ];
    for e in entries {
        if base.join(e).exists() {
            arch.push("CLI");
            break;
        }
    }
    if base.join("package.json").exists() && !base.join("src/main.rs").exists() {
        if let Some(p) = read_json(base, "package.json") {
            if p.get("bin").is_some() {
                arch.push("CLI");
            } else if p.get("main").is_some() {
                arch.push("Library");
            }
        }
    }
    if arch.is_empty() {
        arch.push("CLI");
    }
    let _ = flags;
    arch.first().copied().unwrap_or("CLI").to_string()
}

fn detect_gates(base: &Path, flags: Flags) -> Vec<String> {
    let mut gates = vec![];
    if flags.typescript {
        if let Some(ts) = read_json(base, "tsconfig.json") {
            if ts
                .get("compilerOptions")
                .and_then(|o| o.get("strict"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                gates.push("strict".to_string());
            }
        }
    }
    let prettier_files = ["prettier.config.js", ".prettierrc", ".prettierrc.json"];
    for f in prettier_files {
        if let Some(c) = read_rel(base, f) {
            if c.contains("singleQuote: true") {
                gates.push("'".to_string());
            }
            if c.contains("semi: true") {
                gates.push("semi".to_string());
            }
            break;
        }
    }
    if let Some(p) = read_json(base, "package.json") {
        let has = |dep: &str| -> bool {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|k| p.get(*k).and_then(|d| d.get(dep)).is_some())
        };
        if has("vitest") {
            gates.push("vitest".to_string());
        } else if has("jest") {
            gates.push("jest".to_string());
        }
    }
    if flags.python {
        if let Some(p) = read_rel(base, "pyproject.toml") {
            if p.contains("pytest") {
                gates.push("pytest".to_string());
            }
        }
    }
    gates
}

fn entry_point(base: &Path) -> Option<(&'static str, &'static str)> {
    let candidates = [
        ("src/cli.ts", "CLI entry point"),
        ("src/index.ts", "Main entry point"),
        ("src/main.rs", "Main entry point"),
        ("src/lib.rs", "Library entry point"),
        ("cmd/main.go", "CLI entry point"),
        ("main.py", "Main entry point"),
        ("src/main.py", "Main entry point"),
    ];
    candidates
        .iter()
        .find(|(p, _)| base.join(p).exists())
        .copied()
}

fn gather_intelligence(base: &Path, flags: Flags) -> String {
    let mut stack: Vec<&str> = vec![];
    if flags.typescript {
        stack.push("TS");
    } else if flags.node {
        stack.push("JS");
    }
    if flags.python {
        stack.push("PY");
    }
    if flags.rust {
        stack.push("RUST");
    }
    if flags.go {
        stack.push("GO");
    }
    if flags.java {
        stack.push("JAVA");
    }

    let runtime = if flags.rust {
        Some("Rust".to_string())
    } else if flags.go {
        Some("Go".to_string())
    } else if flags.python {
        py_runtime(base)
    } else if flags.node {
        node_runtime(base)
    } else {
        None
    };
    let arch = detect_arch(base, flags);
    let gates = detect_gates(base, flags);

    let mut line = stack.join("|");
    line.push_str(&format!("→{}", arch));
    if let Some(r) = runtime {
        line.push_str(&format!("|{}", r));
    }
    if !gates.is_empty() {
        line.push_str(&format!(" {}", gates.join(":")));
    }

    let mut out = vec!["## Intelligence".to_string(), line];
    if let Some((path, purpose)) = entry_point(base) {
        out.push(format!(
            "{}: {}",
            path,
            purpose.replace(' ', "").to_lowercase()
        ));
    }
    out.join("\n")
}

// ============ project metadata ============

fn gather_metadata(base: &Path, flags: Flags) -> String {
    let mut sections: Vec<String> = vec![];

    if flags.node {
        if let Some(pkg) = read_json(base, "package.json") {
            let mut m = json!({});
            for k in ["name", "version", "type", "main"] {
                if let Some(v) = pkg.get(k).cloned() {
                    m[k] = v;
                }
            }
            if let Some(e) = pkg.get("engines").cloned() {
                m["engines"] = e;
            }
            if let Some(b) = pkg.get("bin").cloned() {
                m["bin"] = b;
            }
            if let Some(x) = pkg.get("exports").cloned() {
                m["exports"] = x;
            }
            if let Some(s) = pkg.get("scripts").cloned() {
                m["scripts"] = s;
            }
            for k in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(d) = pkg.get(k).and_then(|v| v.as_object()) {
                    m[k] = json!(d.keys().collect::<Vec<_>>());
                }
            }
            sections.push(
                "$ cat package.json | jq '{name, version, type, engines, bin, main, exports, scripts, dependencies, devDependencies, peerDependencies}'".to_string(),
            );
            sections.push(serde_json::to_string_pretty(&m).unwrap_or_default());
        }

        if flags.typescript {
            if let Some(ts) = read_json(base, "tsconfig.json") {
                let opts = ts.get("compilerOptions").cloned().unwrap_or(json!({}));
                let summary = json!({
                    "compilerOptions": {
                        "target": opts.get("target").cloned().unwrap_or(Value::Null),
                        "module": opts.get("module").cloned().unwrap_or(Value::Null),
                        "moduleResolution": opts.get("moduleResolution").cloned().unwrap_or(Value::Null),
                        "strict": opts.get("strict").cloned().unwrap_or(Value::Null),
                        "esModuleInterop": opts.get("esModuleInterop").cloned().unwrap_or(Value::Null),
                        "skipLibCheck": opts.get("skipLibCheck").cloned().unwrap_or(Value::Null),
                        "outDir": opts.get("outDir").cloned().unwrap_or(Value::Null),
                        "rootDir": opts.get("rootDir").cloned().unwrap_or(Value::Null),
                        "declaration": opts.get("declaration").cloned().unwrap_or(Value::Null),
                    },
                    "include": ts.get("include").cloned().unwrap_or(Value::Null),
                    "exclude": ts.get("exclude").cloned().unwrap_or(Value::Null),
                });
                sections.push(
                    "$ cat tsconfig.json | jq '{compilerOptions: {target, module, moduleResolution, strict, esModuleInterop, skipLibCheck, outDir, rootDir, declaration}, include, exclude}'".to_string(),
                );
                sections.push(serde_json::to_string_pretty(&summary).unwrap_or_default());
            }
        }
    }

    if flags.rust {
        if let Some(c) = read_rel(base, "Cargo.toml") {
            let head: String = c.lines().take(60).collect::<Vec<_>>().join("\n");
            sections.push("$ head -60 Cargo.toml".to_string());
            sections.push(head);
        }
    }

    if flags.python {
        if let Some(c) = read_rel(base, "pyproject.toml") {
            let head: String = c.lines().take(60).collect::<Vec<_>>().join("\n");
            sections.push("$ head -60 pyproject.toml".to_string());
            sections.push(head);
        }
        if let Some(v) = read_rel(base, ".python-version") {
            sections.push("$ cat .python-version".to_string());
            sections.push(v.trim().to_string());
        }
    }

    if flags.go {
        if let Some(c) = read_rel(base, "go.mod") {
            sections.push("$ cat go.mod".to_string());
            sections.push(c);
        }
    }

    if flags.java {
        for (rel, label) in [
            ("build.gradle.kts", "build.gradle.kts"),
            ("build.gradle", "build.gradle"),
            ("pom.xml", "pom.xml"),
        ] {
            if let Some(c) = read_rel(base, rel) {
                let head: String = c.lines().take(60).collect::<Vec<_>>().join("\n");
                sections.push(format!("$ head -60 {}", label));
                sections.push(head);
                break;
            }
        }
    }

    sections.join("\n")
}

// ============ code rules (TS/eslint/prettier) ============

fn gather_code_rules(base: &Path) -> String {
    let mut rules: Vec<String> = vec![];

    if let Some(ts) = read_json(base, "tsconfig.json") {
        if let Some(opts) = ts.get("compilerOptions") {
            let mut ts_rules: Vec<&str> = vec![];
            let mut ts_rules_owned: Vec<String> = vec![];
            if opts
                .get("strict")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                ts_rules.push("- Use strict type checking");
            }
            if opts
                .get("noUncheckedIndexedAccess")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                ts_rules.push("- No unchecked indexed access");
            }
            if opts
                .get("noImplicitReturns")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                ts_rules.push("- Explicit return statements");
            }
            if opts
                .get("noUnusedLocals")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                ts_rules.push("- No unused locals");
            }
            if opts
                .get("noUnusedParameters")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                ts_rules.push("- No unused parameters");
            }
            for key in ["module", "target"] {
                if let Some(v) = opts.get(key).and_then(|v| v.as_str()) {
                    ts_rules_owned.push(format!("- {}: {}", key, v));
                }
            }
            if !ts_rules.is_empty() {
                rules.push("## TypeScript Rules".to_string());
                rules.extend(ts_rules.iter().map(|s| s.to_string()));
            }
            if !ts_rules_owned.is_empty() {
                if !rules.iter().any(|r| r == "## TypeScript Rules") {
                    rules.push("## TypeScript Rules".to_string());
                }
                rules.extend(ts_rules_owned);
            }
        }
    }

    let eslint_files = [
        "eslint.config.js",
        "eslint.config.mjs",
        ".eslintrc.js",
        ".eslintrc.json",
        "eslint.config.ts",
    ];
    for f in eslint_files {
        if let Some(c) = read_rel(base, f) {
            let mut e: Vec<&str> = vec![];
            if c.contains("typescript-eslint") {
                e.push("- Use typescript-eslint parser");
            }
            if c.contains("stylistic") {
                e.push("- Use stylistic rules for formatting");
            }
            if c.contains("no-unused-vars") || c.contains("noUnusedVariables") {
                e.push("- No unused variables");
            }
            if c.contains("no-console") {
                e.push("- No console calls (use logging library instead)");
            }
            if c.contains("prefer-const") {
                e.push("- Prefer const over let when variables aren't reassigned");
            }
            if c.contains("no-var") {
                e.push("- Use const/let, never var");
            }
            if !e.is_empty() {
                rules.push("## ESLint Rules".to_string());
                rules.extend(e.iter().map(|s| s.to_string()));
            }
            break;
        }
    }

    rules.join("\n")
}

// ============ file listing (fd/rg) ============

pub fn gather_file_listing(base: &Path, max_depth: u8) -> String {
    let depth = max_depth.to_string();
    let (ok, out) = capture(
        "fd",
        &["--max-depth", &depth, "--type", "f", "--type", "d"],
        base,
    );
    if ok && !out.trim().is_empty() {
        let mut raw: Vec<String> = out
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| {
                if l.starts_with("./") {
                    l.to_string()
                } else {
                    format!("./{}", l)
                }
            })
            .filter(|l| l != "./" && l != "./.." && !l.ends_with('/'))
            .collect();
        raw.sort();

        let mut groups: Vec<(String, Vec<String>)> = vec![];
        for line in &raw {
            let (dir, file) = match line.rsplit_once('/') {
                Some((d, f)) => (format!("{}/", d), f.to_string()),
                None => (String::new(), line.clone()),
            };
            if let Some((_, files)) = groups.iter_mut().find(|(d, _)| d == &dir) {
                files.push(file);
            } else {
                groups.push((dir, vec![file]));
            }
        }

        let compact: Vec<String> = groups
            .iter()
            .map(|(dir, files)| {
                if files.len() == 1 {
                    format!("{}{}", dir, files[0])
                } else {
                    format!("{}{}", dir, brace(files))
                }
            })
            .take(100)
            .collect();
        return format!(
            "$ fd --max-depth {} --type f --type d .\n{}",
            max_depth,
            compact.join("\n")
        );
    }

    let (ok, out) = capture("rg", &["--files", "--max-depth", &depth], base);
    if ok && !out.trim().is_empty() {
        let mut lines: Vec<String> = out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort();
        let truncated: Vec<String> = lines.into_iter().take(200).collect();
        return format!(
            "$ rg --files --max-depth {}\n{}",
            max_depth,
            truncated.join("\n")
        );
    }

    "$ (fd/rg unavailable)\nInstall with: brew install fd ripgrep".to_string()
}

// ============ code statistics (tokei) ============

fn gather_stats(base: &Path) -> String {
    if !bin_exists("tokei") {
        return "$ tokei --output json\nSkipped: tokei not installed (brew install tokei)"
            .to_string();
    }
    let (ok, out) = capture("tokei", &["--output", "json"], base);
    if !ok {
        return "$ tokei --output json\nSkipped: tokei failed".to_string();
    }
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => return format!("$ tokei --output json\nSkipped: {}", e),
    };
    let Some(obj) = data.as_object() else {
        return "$ tokei --output json\nSkipped: unexpected output".to_string();
    };

    let mut langs: Vec<(&String, u64, u64, usize)> = vec![];
    for (key, val) in obj {
        if key == "Total" || key == "total" {
            continue;
        }
        if let Some(code) = val.get("code").and_then(|c| c.as_u64()) {
            let comments = val.get("comments").and_then(|c| c.as_u64()).unwrap_or(0);
            let files = val
                .get("reports")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            langs.push((key, code, comments, files));
        }
    }
    if langs.is_empty() {
        return String::new();
    }
    langs.sort_by_key(|&(_, code, _, _)| std::cmp::Reverse(code));

    let mut sections = vec!["$ tokei --output json".to_string()];
    let (mut tc, mut tcm, mut tf) = (0u64, 0u64, 0usize);
    for (name, code, comments, files) in &langs {
        sections.push(format!(
            "{}: {} code, {} comments ({} files)",
            name, code, comments, files
        ));
        tc += code;
        tcm += comments;
        tf += files;
    }
    sections.push(format!(
        "Total: {} code, {} comments ({} files)",
        tc, tcm, tf
    ));
    sections.join("\n")
}

// ============ static analysis (semgrep) ============

fn codebase_fingerprint(base: &Path) -> String {
    let hash = capture_ok("git", &["rev-parse", "--short=4", "HEAD"], base);
    let status = capture_ok("git", &["status", "--porcelain"], base);
    if let Some(h) = hash {
        return format!(
            "{}:{}{}",
            base.display(),
            h,
            status.map(|s| format!("-{}", s)).unwrap_or_default()
        );
    }
    if base.join("package.json").exists() {
        if let Ok(meta) = fs::metadata(base.join("package.json")) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                    return format!("{}:mtime-{}", base.display(), d.as_millis());
                }
            }
        }
    }
    format!(
        "{}:fallback-{}",
        base.display(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

fn semgrep_cache_file() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("semgrep")
            .join("repo-cache.json"),
    )
}

fn gather_analysis(base: &Path) -> String {
    if !bin_exists("semgrep") {
        return "$ semgrep scan --config auto --json\nSkipped: semgrep not installed (brew install semgrep)".to_string();
    }

    let fingerprint = codebase_fingerprint(base);

    if let Some(path) = semgrep_cache_file() {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(cache) = serde_json::from_str::<Value>(&content) {
                if let Some(hit) = cache.get(&fingerprint).and_then(|v| v.get("output")) {
                    if let Some(s) = hit.as_str() {
                        return s.to_string();
                    }
                }
            }
        }
    }

    let (ok, out) = capture(
        "semgrep",
        &["scan", "--config", "auto", "--json", "--quiet"],
        base,
    );
    if !ok {
        return "$ semgrep scan --config auto --json\nSkipped: semgrep failed".to_string();
    }
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => return format!("$ semgrep scan --config auto --json\nSkipped: {}", e),
    };

    let output = match data.get("results").and_then(|r| r.as_array()) {
        None => "$ semgrep scan --config auto --json\nNo issues found.".to_string(),
        Some(findings) if findings.is_empty() => {
            "$ semgrep scan --config auto --json\nNo issues found.".to_string()
        }
        Some(findings) => {
            let sev = |f: &Value| -> u8 {
                match f
                    .get("extra")
                    .and_then(|e| e.get("severity"))
                    .and_then(|s| s.as_str())
                {
                    Some("ERROR") => 0,
                    Some("WARNING") => 1,
                    Some("INFO") => 2,
                    _ => 3,
                }
            };
            let mut sorted: Vec<&Value> = findings.iter().collect();
            sorted.sort_by_key(|f| sev(f));
            sorted.truncate(10);

            let mut sections = vec![format!(
                "$ semgrep scan --config auto --json\nFound {} issues (showing top {}):",
                findings.len(),
                sorted.len()
            )];
            for f in &sorted {
                let rel = f
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("?")
                    .trim_start_matches(&format!("{}/", base.display()))
                    .to_string();
                let line = f
                    .get("start")
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0);
                let severity = f
                    .get("extra")
                    .and_then(|e| e.get("severity"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("INFO")
                    .to_uppercase();
                let rule = f.get("check_id").and_then(|c| c.as_str()).unwrap_or("?");
                sections.push(format!("- {}:{} [{}] {}", rel, line, severity, rule));
            }
            sections.join("\n")
        }
    };

    if let Some(path) = semgrep_cache_file() {
        let mut cache: Value = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(json!({}));
        if !cache.is_object() {
            cache = json!({});
        }
        let map = cache.as_object_mut().unwrap();
        map.insert(
            fingerprint,
            json!({ "output": output, "timestamp": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }),
        );
        let _ = fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = fs::write(
            &path,
            serde_json::to_string_pretty(&cache).unwrap_or_default(),
        );
    }

    output
}

// ============ dependency audit ============

fn parse_npm_audit(stdout: &str, pm: &str) -> Option<String> {
    let data: Value = serde_json::from_str(stdout).ok()?;
    let vulns = data.get("vulnerabilities").and_then(|v| v.as_object())?;
    if vulns.is_empty() {
        return Some(format!("$ {} audit --json\nNo known vulnerabilities.", pm));
    }
    let mut entries: Vec<(&String, &Value)> = vulns.iter().collect();
    entries.sort_by_key(|(_, v)| match v.get("severity").and_then(|s| s.as_str()) {
        Some("critical") => 0,
        Some("high") => 1,
        Some("moderate") => 2,
        Some("low") => 3,
        _ => 4,
    });
    entries.truncate(15);
    let mut lines = vec![format!(
        "$ {} audit --json\nFound {} vulnerabilities (showing top {}):",
        pm,
        vulns.len(),
        entries.len()
    )];
    for (name, info) in &entries {
        let severity = info
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_uppercase();
        let range = info.get("range").and_then(|r| r.as_str()).unwrap_or("");
        lines.push(format!("- [{}] {} {}", severity, name, range));
    }
    Some(lines.join("\n"))
}

fn parse_cargo_audit(stdout: &str) -> Option<String> {
    let data: Value = serde_json::from_str(stdout).ok()?;
    let advisories = data
        .get("vulnerabilities")
        .and_then(|v| v.get("list"))
        .and_then(|l| l.as_array())?;
    if advisories.is_empty() {
        return Some("$ cargo audit --json\nNo known vulnerabilities.".to_string());
    }
    let mut lines = vec![format!(
        "$ cargo audit --json\nFound {} vulnerabilities (showing top {}):",
        advisories.len(),
        advisories.len().min(15)
    )];
    for adv in advisories.iter().take(15) {
        let severity = adv
            .get("advisory")
            .and_then(|a| a.get("severity"))
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_uppercase();
        let id = adv
            .get("advisory")
            .and_then(|a| a.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("?");
        let title = adv
            .get("advisory")
            .and_then(|a| a.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let pkg = adv
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("?");
        lines.push(format!("- [{}] {}: {} ({})", severity, pkg, title, id));
    }
    Some(lines.join("\n"))
}

fn gather_audit(base: &Path, flags: Flags) -> String {
    let mut parts: Vec<String> = vec![];

    if flags.node {
        // bun's audit target is `npm audit`; all Node PMs funnel through npm here.
        let pm = "npm";
        let (ok, out) = capture(pm, &["audit", "--json"], base);
        if ok || !out.trim().is_empty() {
            if let Some(s) = parse_npm_audit(&out, pm) {
                parts.push(s);
            }
        }
    }

    if flags.rust {
        let (ok, out) = capture("cargo", &["audit", "--json"], base);
        if ok || !out.trim().is_empty() {
            if let Some(s) = parse_cargo_audit(&out) {
                parts.push(s);
            }
        }
    }

    parts.join("\n")
}

// ============ import dependency graph ============

use std::sync::OnceLock;
static RE_RUST_MOD: OnceLock<Regex> = OnceLock::new();
static RE_RUST_USE: OnceLock<Regex> = OnceLock::new();
static RE_PY_FROM: OnceLock<Regex> = OnceLock::new();
static RE_GO_BLOCK: OnceLock<Regex> = OnceLock::new();
static RE_GO_QUOTED: OnceLock<Regex> = OnceLock::new();
static RE_GO_SINGLE: OnceLock<Regex> = OnceLock::new();
static RE_TS_IMPORT: OnceLock<Regex> = OnceLock::new();
static RE_TS_REQUIRE: OnceLock<Regex> = OnceLock::new();

fn gather_import_graph(base: &Path, flags: Flags) -> String {
    let pattern = if flags.typescript || flags.node {
        Some("src/**/*.{ts,tsx,js,jsx}")
    } else if flags.python {
        Some("**/*.py")
    } else if flags.rust {
        Some("src/**/*.rs")
    } else if flags.go {
        Some("**/*.go")
    } else {
        None
    };
    let pattern = match pattern {
        Some(p) => p,
        None => return String::new(),
    };

    let (ok, out) = capture("rg", &["--files", "-g", pattern], base);
    if !ok {
        return String::new();
    }
    let mut files: Vec<String> = out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    files.sort();
    files.truncate(40);

    let mut graph: Vec<(String, Vec<String>)> = vec![];
    let prefix = format!("{}/", base.display());

    // Warm per-language regexes once (never compile a regex inside the file loop).
    if flags.rust {
        let _ = RE_RUST_MOD.get_or_init(|| Regex::new(r"(?m)^mod\s+(\w+)").unwrap());
        let _ = RE_RUST_USE.get_or_init(|| Regex::new(r"use\s+(crate|super)::([\w:]+)").unwrap());
    } else if flags.python {
        let _ = RE_PY_FROM.get_or_init(|| Regex::new(r"from\s+(\.[\w.]*)\s+import").unwrap());
    } else if flags.go {
        let _ = RE_GO_BLOCK.get_or_init(|| Regex::new(r"import\s*\(([\s\S]*?)\)").unwrap());
        let _ = RE_GO_QUOTED.get_or_init(|| Regex::new(r#""([^"]+)""#).unwrap());
        let _ = RE_GO_SINGLE.get_or_init(|| Regex::new(r#"import\s+"([^"]+)""#).unwrap());
    } else {
        let _ = RE_TS_IMPORT.get_or_init(|| {
            Regex::new(
                r#"import\s+(?:type\s+)?(?:\{[^}]+\}|\*\s+as\s+\w+|\w+(?:\s*,\s*\{[^}]+\})?)\s+from\s+['"]([^'"]+)['"]"#,
            )
            .unwrap()
        });
        let _ = RE_TS_REQUIRE
            .get_or_init(|| Regex::new(r#"require\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap());
    }

    for file in &files {
        let Some(content) = read_rel(base, file) else {
            continue;
        };
        let rel = file.trim_start_matches(&prefix).to_string();
        let mut imports: Vec<String> = vec![];

        if flags.rust {
            let re_mod = RE_RUST_MOD.get().unwrap();
            let re_use = RE_RUST_USE.get().unwrap();
            for cap in re_mod.captures_iter(&content) {
                imports.push(cap[1].to_string());
            }
            for cap in re_use.captures_iter(&content) {
                imports.push(format!("{}::{}", &cap[1], cap[2].trim_end_matches(':')));
            }
        } else if flags.python {
            let re = RE_PY_FROM.get().unwrap();
            for cap in re.captures_iter(&content) {
                imports.push(cap[1].to_string());
            }
        } else if flags.go {
            let re_block = RE_GO_BLOCK.get().unwrap();
            let re_quoted = RE_GO_QUOTED.get().unwrap();
            let re_single = RE_GO_SINGLE.get().unwrap();
            if let Some(caps) = re_block.captures(&content) {
                for cap in re_quoted.captures_iter(&caps[1]) {
                    imports.push(cap[1].to_string());
                }
            }
            for cap in re_single.captures_iter(&content) {
                if !imports.contains(&cap[1].to_string()) {
                    imports.push(cap[1].to_string());
                }
            }
        } else {
            // TS/JS: strip // comments, then match import/require of relative paths.
            let code: String = content
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let re_import = RE_TS_IMPORT.get().unwrap();
            let re_require = RE_TS_REQUIRE.get().unwrap();
            for cap in re_import.captures_iter(&code) {
                if cap[1].starts_with('.') {
                    imports.push(cap[1].to_string());
                }
            }
            for cap in re_require.captures_iter(&code) {
                if cap[1].starts_with('.') {
                    imports.push(cap[1].to_string());
                }
            }
        }

        imports.dedup();
        if !imports.is_empty() {
            graph.push((rel, imports));
        }
    }

    if graph.is_empty() {
        return String::new();
    }
    let mut lines = vec!["## Module Dependencies".to_string()];
    for (file, deps) in &graph {
        lines.push(format!("{} → {}", file, brace(deps)));
    }
    lines.join("\n")
}

// ============ todos (rg) ============

fn gather_todos(base: &Path) -> Vec<String> {
    let (ok, out) = capture(
        "rg",
        &[
            "-i",
            "-n",
            "--no-heading",
            r"(?://|#|<!--|/\*)\s*(TODO|FIXME|HACK|XXX)\b:?",
            "src/",
        ],
        base,
    );
    if !ok {
        return vec![];
    }
    let matches: Vec<&str> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(20)
        .collect();
    if matches.is_empty() {
        return vec![];
    }

    let clean_re = Regex::new(r"(?i)^(?:\/\/|#|<!--|\/\*)\s*(?:TODO|FIXME|HACK|XXX):?\s*").unwrap();
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    for line in matches {
        let mut parts = line.splitn(3, ':');
        let Some(file) = parts.next() else {
            continue;
        };
        let Some(num) = parts.next() else {
            continue;
        };
        let Some(content) = parts.next() else {
            continue;
        };
        let content = content.trim();
        let content = clean_re.replace(content, "").to_string();
        let entry = format!("{}: {}", num, content);
        if let Some((_, items)) = groups.iter_mut().find(|(f, _)| f == file) {
            items.push(entry);
        } else {
            groups.push((file.to_string(), vec![entry]));
        }
    }

    let mut out: Vec<String> = vec![];
    for (file, items) in &groups {
        if items.len() == 1 {
            out.push(format!("- {}:{}", file, items[0]));
        } else {
            out.push(format!("- {}:", file));
            for item in items {
                out.push(format!("  - {}", item));
            }
        }
    }
    out.into_iter().take(15).collect()
}

// ============ test patterns ============

fn gather_test_patterns(base: &Path) -> String {
    let (ok, out) = capture("rg", &["--files", "-g", "**/*.test.ts"], base);
    if !ok {
        return String::new();
    }
    let file = out.lines().map(|l| l.trim()).find(|l| !l.is_empty());
    let Some(file) = file else {
        return String::new();
    };
    let Some(content) = read_rel(base, file) else {
        return String::new();
    };
    let head: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
    format!("## Test Patterns\n```typescript\n{}\n```", head)
}

// ============ code patterns ============

fn gather_code_patterns(base: &Path) -> String {
    let (ok, out) = capture("rg", &["--files", "-g", "src/**/*.ts"], base);
    if !ok {
        return String::new();
    }
    let files: Vec<&str> = out
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if files.is_empty() {
        return String::new();
    }

    let cli = files.iter().find(|f| f.contains("/cli.ts"));
    let cmd = files.iter().find(|f| f.contains("/commands/"));
    let mut patterns: Vec<String> = vec![];
    if let Some(f) = cli {
        if let Some(c) = read_rel(base, f) {
            let head: String = c.lines().take(8).collect::<Vec<_>>().join("\n");
            patterns.push("### CLI Entry Point Pattern".to_string());
            patterns.push("```typescript".to_string());
            patterns.push(head);
            patterns.push("```".to_string());
        }
    }
    if let Some(f) = cmd {
        if let Some(c) = read_rel(base, f) {
            let head: String = c.lines().take(15).collect::<Vec<_>>().join("\n");
            patterns.push("### Command Handler Pattern".to_string());
            patterns.push("```typescript".to_string());
            patterns.push(head);
            patterns.push("```".to_string());
        }
    }
    if patterns.is_empty() {
        return String::new();
    }
    format!("## Code Patterns\n{}", patterns.join("\n"))
}

// ============ README ============

fn gather_readme(base: &Path) -> Option<String> {
    let content = read_rel(base, "README.md")?;
    let lines: Vec<&str> = content.lines().collect();
    let mut included: Vec<&str> = vec![];
    let mut saw_heading = false;
    let mut truncated = false;
    for line in &lines {
        if line.starts_with("## ") && saw_heading {
            truncated = true;
            break;
        }
        if line.starts_with("## ") {
            saw_heading = true;
        }
        included.push(line);
        if included.len() >= 40 {
            truncated = true;
            break;
        }
    }
    let suffix = if truncated {
        format!(" ({} more lines)", lines.len() - included.len())
    } else {
        String::new()
    };
    let body = included.join("\n");
    Some(format!("## README{}\n{}", suffix, body))
}

// ============ structure ============

fn gather_structure(base: &Path) -> (Vec<String>, Vec<String>, usize) {
    let mut dirs: Vec<String> = vec![];
    let mut entries: Vec<String> = vec![];
    if let Ok(rd) = fs::read_dir(base) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                dirs.push(name.clone());
            }
            if matches!(name.as_str(), "cli.ts" | "index.ts" | "main.ts") {
                entries.push(name);
            }
        }
    }
    if let Ok(rd) = fs::read_dir(base.join("src")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                dirs.push(format!("src/{}", name));
            }
            if matches!(name.as_str(), "cli.ts" | "index.ts" | "main.rs" | "lib.rs") {
                entries.push(format!("src/{}", name));
            }
        }
    }
    dirs.sort();
    let (ok, out) = capture("rg", &["--files", "-g", "**/*.test.ts"], base);
    let test_count = if ok {
        out.lines().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };
    (dirs, entries, test_count)
}

// ============ file tree (legacy unicode) ============

pub fn generate_tree(base: &Path, max_depth: u8) -> String {
    let mut lines: Vec<String> = vec![basename(base)];
    build_tree(base, "", 0, max_depth, &mut lines);
    lines.join("\n")
}

fn build_tree(dir: &Path, prefix: &str, depth: u8, max_depth: u8, lines: &mut Vec<String>) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            !n.starts_with('.')
                && !matches!(
                    n.as_ref(),
                    "node_modules" | "target" | "dist" | "build" | ".git"
                )
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let total = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == total - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        lines.push(format!(
            "{}{}{}{}",
            prefix,
            connector,
            name,
            if is_dir { "/" } else { "" }
        ));
        if is_dir {
            build_tree(&entry.path(), &child_prefix, depth + 1, max_depth, lines);
        }
    }
}

// ============ top-level gather ============

fn check_dependencies(opts: &ContextOptions) -> anyhow::Result<()> {
    if !bin_exists("fd") && !bin_exists("rg") {
        anyhow::bail!("fd or rg is required. Install with: brew install fd ripgrep");
    }
    if opts.stats && !bin_exists("tokei") {
        // soft: stats gatherer will emit a Skipped note
    }
    if opts.analysis && !bin_exists("semgrep") {
        // soft: analysis gatherer will emit a Skipped note
    }
    Ok(())
}

pub fn gather(detector: &Detector, base: &Path, opts: &ContextOptions) -> anyhow::Result<String> {
    let flags = Flags::from(detector, base);
    check_dependencies(opts)?;

    let (git, intel, metadata, rules, readme, stats, analysis, audit, graph, tests, todos, listing) =
        std::thread::scope(|s| {
            let h_git = s.spawn(|| gather_git(base));
            let h_intel = s.spawn(|| gather_intelligence(base, flags));
            let h_meta = if opts.metadata {
                Some(s.spawn(|| gather_metadata(base, flags)))
            } else {
                None
            };
            let h_rules = if opts.metadata {
                Some(s.spawn(|| gather_code_rules(base)))
            } else {
                None
            };
            let h_readme = if opts.docs {
                Some(s.spawn(|| gather_readme(base)))
            } else {
                None
            };
            let h_stats = opts.stats.then(|| s.spawn(|| gather_stats(base)));
            let h_analysis = opts.analysis.then(|| s.spawn(|| gather_analysis(base)));
            let h_audit = opts.audit.then(|| s.spawn(|| gather_audit(base, flags)));
            let h_graph = if opts.graph {
                Some(s.spawn(|| gather_import_graph(base, flags)))
            } else {
                None
            };
            let h_tests = opts.tests.then(|| s.spawn(|| gather_test_patterns(base)));
            let h_todos = if opts.todos {
                Some(s.spawn(|| gather_todos(base)))
            } else {
                None
            };
            let h_listing = s.spawn(|| gather_file_listing(base, 3));

            (
                h_git.join().ok().flatten(),
                h_intel.join().unwrap_or_default(),
                h_meta.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_rules.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_readme.and_then(|h| h.join().ok()).flatten(),
                h_stats.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_analysis.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_audit.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_graph.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_tests.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_todos.and_then(|h| h.join().ok()).unwrap_or_default(),
                h_listing.join().unwrap_or_default(),
            )
        });

    let (dirs, entry_points, test_count) = gather_structure(base);

    let mut lines: Vec<String> = vec![];
    let name = git
        .as_ref()
        .and_then(|g| g.remote_url.clone())
        .unwrap_or_else(|| basename(base));
    lines.push(format!("// Repository: {}", name));
    lines.push(String::new());

    lines.push(intel);
    lines.push(format!("// Project Type: {}", flags.project_type()));
    lines.push(String::new());

    if !metadata.is_empty() || !rules.is_empty() {
        lines.push("## Project Metadata".to_string());
        if !metadata.is_empty() {
            lines.push(metadata);
        }
        if !rules.is_empty() {
            lines.push(rules);
        }
        lines.push(String::new());
    }

    if let Some(r) = readme {
        lines.push(r);
        lines.push(String::new());
    }

    if let Some(g) = &git {
        lines.push(format_git(g));
        lines.push(String::new());
    }

    if !stats.is_empty() {
        lines.push("## Code Statistics".to_string());
        lines.push(stats);
        lines.push(String::new());
    }
    if !analysis.is_empty() {
        lines.push("## Static Analysis".to_string());
        lines.push(analysis);
        lines.push(String::new());
    }
    if !audit.is_empty() {
        lines.push("## Dependency Audit".to_string());
        lines.push(audit);
        lines.push(String::new());
    }
    if opts.patterns {
        let pats = gather_code_patterns(base);
        if !pats.is_empty() {
            lines.push(pats);
        }
    }
    if !graph.is_empty() {
        lines.push(graph);
        lines.push(String::new());
    }
    if !tests.is_empty() {
        lines.push(tests);
    }
    if !todos.is_empty() {
        lines.push("## Active Work (TODO/FIXME)".to_string());
        for t in todos.iter().take(10) {
            lines.push(t.clone());
        }
        lines.push(String::new());
    }

    lines.push("## File Structure".to_string());
    if !entry_points.is_empty() {
        lines.push(format!("Entry points: {}", entry_points.join(", ")));
    }
    if test_count > 0 {
        lines.push(format!("Test files: {} found", test_count));
    }
    if !dirs.is_empty() {
        lines.push(format!("Directories: {}", dirs.join(", ")));
    }
    lines.push(listing);
    lines.push(String::new());

    Ok(lines.join("\n"))
}

/// Simple context: a compact JSON-able snapshot (git + structure + deps).
pub fn gather_repo_context_json(base: &Path) -> Value {
    let (dirs, entry_points, test_count) = gather_structure(base);
    let deps = read_json(base, "package.json")
        .and_then(|p| {
            let mut deps = serde_json::Map::new();
            for k in ["dependencies", "devDependencies"] {
                if let Some(d) = p.get(k).and_then(|v| v.as_object()) {
                    for (name, val) in d {
                        deps.insert(name.clone(), val.clone());
                    }
                }
            }
            (!deps.is_empty()).then_some(Value::Object(deps))
        })
        .unwrap_or(Value::Null);

    json!({
        "path": base.display().to_string(),
        "name": basename(base),
        "git": gather_git(base),
        "structure": {
            "directories": dirs,
            "entryPoints": entry_points,
            "testCount": test_count,
        },
        "dependencies": deps,
    })
}

pub fn format_simple(base: &Path) -> String {
    let ctx = gather_repo_context_json(base);
    let mut lines = vec![
        format!("# Repository Context: {}", basename(base)),
        String::new(),
        format!("**Path:** `{}`", base.display()),
        String::new(),
    ];
    if let Some(git) = ctx.get("git") {
        if !git.is_null() {
            lines.push("## Git".to_string());
            if let Some(b) = git.get("branch").and_then(|v| v.as_str()) {
                lines.push(format!("- **Branch:** `{}`", b));
            }
            if let Some(c) = git.get("last_commit").and_then(|v| v.as_str()) {
                lines.push(format!("- **Last Commit:** {}", c));
            }
            lines.push(String::new());
        }
    }
    if let Some(s) = ctx.get("structure") {
        lines.push("## Structure".to_string());
        if let Some(dirs) = s.get("directories").and_then(|v| v.as_array()) {
            let names: Vec<String> = dirs
                .iter()
                .filter_map(|d| d.as_str().map(String::from))
                .collect();
            lines.push(format!("Directories: {}", names.join(", ")));
        }
        if let Some(tc) = s.get("testCount").and_then(|v| v.as_u64()) {
            if tc > 0 {
                lines.push(format!("Tests: {} files", tc));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_strips_credentials() {
        assert_eq!(
            redact_url("https://user:token@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            redact_url("https://ghp_secret@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            redact_url("https://github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            redact_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }
}
