//! Task-runner discovery — the fallback/preference backbone.
//!
//! Enumerates tasks defined by common runners so `repo run` can fall back to
//! them and `repo lint`/`repo fmt` can *prefer* them over our built-in guesses:
//!
//! - **mise** — `mise.toml`/`⁠.mise.toml` (+ `.local` variants, grouped
//!   `mise/config.toml` et al.) `[tasks.<name>]` tables and executable file
//!   tasks in `mise-tasks/` etc. (argv: `mise run <name>`)
//! - **npm/pnpm/yarn/bun** — `package.json` `scripts` (argv: `{pm} run <name>`)
//! - **deno** — `deno.json`/`deno.jsonc` `tasks` (argv: `deno task <name>`)
//! - **just** — `justfile`/`Justfile` recipes (argv: `just <name>`)
//! - **make** — `Makefile`/`makefile`/`GNUmakefile` targets (argv: `make <name>`)
//! - **gradle** — `./gradlew tasks --all`, **lazy + disk-cached** by mtime
//!   (argv: `./gradlew <name>`). Only queried when no cheaper runner has the
//!   task, since it spins a JVM (1-5s).
//!
//! Priority when several runners define the same task name:
//! `mise → just → make → deno → npm → gradle`. mise leads because its tasks
//! run with the project's declared tools + env on PATH — the environment the
//! other runners' commands silently assume. Gradle is the expensive last
//! resort.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use indexmap::IndexMap;
use regex::Regex;

use crate::search::index::fnv1a64;

/// Which runner owns a discovered task. `Npm` carries the package manager
/// (npm/pnpm/yarn/bun) so the argv uses the right invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runner {
    Npm(String),
    Deno,
    Mise,
    Just,
    Make,
    Gradle,
}

impl Runner {
    /// Short human label for headers/`--list`.
    pub fn label(&self) -> &'static str {
        match self {
            Runner::Npm(_) => "npm",
            Runner::Deno => "deno",
            Runner::Mise => "mise",
            Runner::Just => "just",
            Runner::Make => "make",
            Runner::Gradle => "gradle",
        }
    }
}

/// A task located in some runner, plus the argv to invoke it.
#[derive(Clone, Debug)]
pub struct Found {
    pub runner: Runner,
    pub argv: Vec<String>,
}

impl Found {
    /// `just dev`, `npm run build`, …
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// All discovered runners for the current repo. Cheap runners are parsed up
/// front; gradle is deferred (marker only) and resolved on first need via
/// [`TaskRunners::gradle`] (disk-cached + memoized in-process).
pub struct TaskRunners {
    npm: Option<(String, IndexMap<String, String>)>,
    deno: Option<IndexMap<String, String>>,
    mise: Option<HashSet<String>>,
    just: Option<HashSet<String>>,
    make: Option<HashSet<String>>,
    gradle_marker: Option<PathBuf>,
    gradle: OnceLock<Option<HashSet<String>>>,
}

impl TaskRunners {
    /// Discover runners in the current directory (CWD must already be the
    /// project root — `Detector::new` arranges this).
    pub fn discover(pkg: &Option<serde_json::Value>, pm: &str) -> Self {
        Self {
            npm: npm_scripts(pkg, pm),
            deno: deno_tasks(),
            mise: mise_tasks(),
            just: just_recipes(),
            make: make_targets(),
            gradle_marker: gradle_marker(),
            gradle: OnceLock::new(),
        }
    }

    /// First runner (by priority) defining `name`, else `None`. Gradle is
    /// queried lazily — only if the cheaper runners all lack `name`.
    pub fn find(&self, name: &str) -> Option<Found> {
        if let Some(t) = &self.mise {
            if t.contains(name) {
                return Some(Found {
                    runner: Runner::Mise,
                    argv: vec!["mise".into(), "run".into(), name.into()],
                });
            }
        }
        if let Some(t) = &self.just {
            if t.contains(name) {
                return Some(Found {
                    runner: Runner::Just,
                    argv: vec!["just".into(), name.into()],
                });
            }
        }
        if let Some(t) = &self.make {
            if t.contains(name) {
                return Some(Found {
                    runner: Runner::Make,
                    argv: vec!["make".into(), name.into()],
                });
            }
        }
        if let Some(t) = &self.deno {
            if t.contains_key(name) {
                return Some(Found {
                    runner: Runner::Deno,
                    argv: vec!["deno".into(), "task".into(), name.into()],
                });
            }
        }
        if let Some((pm, t)) = &self.npm {
            if t.contains_key(name) {
                return Some(Found {
                    runner: Runner::Npm(pm.clone()),
                    argv: vec![pm.clone(), "run".into(), name.into()],
                });
            }
        }
        if let Some(t) = self.gradle() {
            if t.contains(name) {
                return Some(Found {
                    runner: Runner::Gradle,
                    argv: vec!["./gradlew".into(), name.into()],
                });
            }
        }
        None
    }

    /// `true` if any runner defines `name`. Drives the CEL `task()` host fn.
    pub fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// First runner defining any of `names` (in `names` order), else `None`.
    /// Used by `run`, which tries `run → start → dev → serve`.
    pub fn find_any(&self, names: &[&str]) -> Option<Found> {
        names.iter().find_map(|n| self.find(n))
    }

    /// Every runner that defines `name`, in priority order. For `--list` display.
    pub fn list(&self, name: &str) -> Vec<Found> {
        let mut out = Vec::new();
        if self.mise.as_ref().is_some_and(|t| t.contains(name)) {
            out.push(Found {
                runner: Runner::Mise,
                argv: vec!["mise".into(), "run".into(), name.into()],
            });
        }
        if self.just.as_ref().is_some_and(|t| t.contains(name)) {
            out.push(Found {
                runner: Runner::Just,
                argv: vec!["just".into(), name.into()],
            });
        }
        if self.make.as_ref().is_some_and(|t| t.contains(name)) {
            out.push(Found {
                runner: Runner::Make,
                argv: vec!["make".into(), name.into()],
            });
        }
        if self.deno.as_ref().is_some_and(|t| t.contains_key(name)) {
            out.push(Found {
                runner: Runner::Deno,
                argv: vec!["deno".into(), "task".into(), name.into()],
            });
        }
        if let Some((pm, t)) = &self.npm {
            if t.contains_key(name) {
                out.push(Found {
                    runner: Runner::Npm(pm.clone()),
                    argv: vec![pm.clone(), "run".into(), name.into()],
                });
            }
        }
        if let Some(g) = self.gradle() {
            if g.contains(name) {
                out.push(Found {
                    runner: Runner::Gradle,
                    argv: vec!["./gradlew".into(), name.into()],
                });
            }
        }
        out
    }

    /// Lazy, disk-cached, memoized gradle task set. Returns `None` if gradle is
    /// absent or `./gradlew tasks --all` fails.
    fn gradle(&self) -> Option<&HashSet<String>> {
        let cached = self.gradle.get_or_init(|| {
            self.gradle_marker.as_ref()?;
            gradle_tasks().map(HashSet::from_iter)
        });
        cached.as_ref()
    }
}

// ============ per-runner discovery (cheap, eager) ============

/// npm/pnpm/yarn/bun scripts from `package.json`. Returns `(pm, scripts)`.
fn npm_scripts(
    pkg: &Option<serde_json::Value>,
    pm: &str,
) -> Option<(String, IndexMap<String, String>)> {
    let scripts = pkg.as_ref()?.get("scripts")?.as_object()?;
    let map: IndexMap<String, String> = scripts
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
        .collect();
    if map.is_empty() {
        None
    } else {
        Some((pm.to_string(), map))
    }
}

/// `deno.json`/`deno.jsonc` `tasks`. `.jsonc` gets a naive `//` comment strip.
fn deno_tasks() -> Option<IndexMap<String, String>> {
    let (raw, _is_jsonc) =
        read_strip_comments("deno.json").or_else(|| read_strip_comments("deno.jsonc"))?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let tasks = v.get("tasks")?.as_object()?;
    let map: IndexMap<String, String> = tasks
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
        .collect();
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// `(content, is_jsonc)` if the file exists.
fn read_strip_comments(path: &str) -> Option<(String, bool)> {
    let raw = fs::read_to_string(path).ok()?;
    let is_jsonc = path.ends_with(".jsonc");
    if is_jsonc {
        // Strip `//` line comments and `/* */` blocks — sufficient for deno.jsonc.
        let stripped = strip_jsonc(&raw);
        Some((stripped, true))
    } else {
        Some((raw, false))
    }
}

fn strip_jsonc(src: &str) -> String {
    let block_re = Regex::new(r"/\*[\s\S]*?\*/").unwrap();
    let line_re = Regex::new(r"//[^\n]*").unwrap();
    let no_blocks = block_re.replace_all(src, "");
    line_re.replace_all(&no_blocks, "").into_owned()
}

/// mise tasks from config `[tasks.<name>]` tables plus executable file tasks.
/// Prefer `mise tasks --local --name-only` (authoritative: handles quoting,
/// `test:group` naming, hidden filtering, parent-dir merging) when the binary
/// is on PATH; otherwise parse the config + task files directly.
fn mise_tasks() -> Option<HashSet<String>> {
    mise_tasks_in(Path::new("."))
}

fn mise_tasks_in(dir: &Path) -> Option<HashSet<String>> {
    if which::which("mise").is_ok() {
        if let Ok(out) = duct::cmd("mise", ["tasks", "--local", "--name-only"])
            .dir(dir)
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()
        {
            if out.status.success() {
                let set: HashSet<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .collect();
                if !set.is_empty() {
                    return Some(set);
                }
            }
        }
    }
    parse_mise_configs(dir)
}

/// Config files searched for `[tasks.<name>]` tables. Env-suffixed variants
/// (`mise.<env>.toml`) only load under `MISE_ENV`, so they are not listed —
/// the authoritative `mise tasks` path covers them when the binary exists.
const MISE_CONFIG_FILES: &[&str] = &[
    "mise.local.toml",
    ".mise.local.toml",
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".mise/config.toml",
    ".config/mise.toml",
    ".config/mise/config.toml",
];

/// Directories holding executable file tasks (`hello.sh` → `hello`,
/// `test/units` → `test:units`, `test/_default` → `test`).
const MISE_TASK_DIRS: &[&str] = &[
    "mise-tasks",
    ".mise-tasks",
    "mise/tasks",
    ".mise/tasks",
    ".config/mise/tasks",
];

fn parse_mise_configs(dir: &Path) -> Option<HashSet<String>> {
    let mut set: HashSet<String> = HashSet::new();
    for name in MISE_CONFIG_FILES {
        if let Ok(raw) = fs::read_to_string(dir.join(name)) {
            set.extend(parse_mise_toml(&raw));
        }
    }
    // `conf.d` fragments (non-hidden `*.toml`, alphabetical — order is
    // irrelevant here since task names merge).
    for frag_dir in ["mise/conf.d", ".mise/conf.d", ".config/mise/conf.d"] {
        if let Ok(rd) = fs::read_dir(dir.join(frag_dir)) {
            for entry in rd.flatten() {
                let p = entry.path();
                let is_frag = p.extension().is_some_and(|e| e == "toml")
                    && p.file_name()
                        .is_some_and(|n| !n.to_string_lossy().starts_with('.'));
                if is_frag {
                    if let Ok(raw) = fs::read_to_string(&p) {
                        set.extend(parse_mise_toml(&raw));
                    }
                }
            }
        }
    }
    for task_dir in MISE_TASK_DIRS {
        set.extend(mise_file_tasks_in(&dir.join(task_dir)));
    }
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// Extract `[tasks.<name>]` table names from TOML source. A dotted suffix is a
/// sub-table of the task (`[tasks.build.env]` → `build`); quoted names keep
/// their dots and spaces (`[tasks."my task"]`). Commented lines can't match
/// (the regex anchors on `[`), and bare `[tasks]` yields no name.
fn parse_mise_toml(raw: &str) -> HashSet<String> {
    let re = Regex::new(r"(?m)^\s*\[tasks\.([^\]]*)\]").unwrap();
    let mut set = HashSet::new();
    for caps in re.captures_iter(raw) {
        if let Some(name) = mise_task_header_name(caps.get(1).unwrap().as_str()) {
            set.insert(name);
        }
    }
    set
}

/// First dotted component of a `[tasks.…]` header, honoring TOML quoting.
fn mise_task_header_name(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let first = if rest.starts_with('"') || rest.starts_with('\'') {
        let q = rest.as_bytes()[0] as char;
        rest[1..].split(q).next()?.to_string()
    } else {
        rest.split(['.', ' ', '\t']).next()?.trim().to_string()
    };
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

/// Executable scripts under a file-task dir, walked recursively (explicit
/// stack — no `walkdir` dep). `*.toml` files are skipped: they are not a
/// documented file-task format (mise parses their bare keys as task names —
/// a quirk the authoritative binary path reproduces exactly when present).
fn mise_file_tasks_in(dir: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return set;
    };
    let mut stack: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                continue; // hidden dirs hold no tasks
            }
            if let Ok(rd) = fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
            continue;
        }
        if p.extension().is_some_and(|e| e == "toml") {
            continue;
        }
        if p.file_name()
            .is_none_or(|n| n.to_string_lossy().starts_with('.'))
            || !is_executable(&p)
        {
            continue;
        }
        let Ok(rel) = p.strip_prefix(dir) else {
            continue;
        };
        if let Some(name) = mise_file_task_name(rel) {
            set.insert(name);
        }
    }
    set
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // mise requires the exec bit on macOS/Linux; without it the file is invisible.
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// `hello.sh` → `hello` (mise strips one trailing extension, whatever it is),
/// `test/units` → `test:units`, `test/_default` → `test`.
fn mise_file_task_name(rel: &Path) -> Option<String> {
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let file = parts.pop()?;
    let stem = match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file,
    };
    if stem == "_default" {
        if parts.is_empty() {
            return Some("_default".to_string());
        }
    } else {
        parts.push(stem);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(":"))
    }
}

/// justfile recipes. Prefer `just --summary` (authoritative) when the binary is
/// on PATH; otherwise parse the file text for `^name:` recipe headers.
fn just_recipes() -> Option<HashSet<String>> {
    just_recipes_in(Path::new("."))
}

fn just_recipes_in(dir: &Path) -> Option<HashSet<String>> {
    let path = just_file_in(dir)?;
    if which::which("just").is_ok() {
        if let Ok(out) = duct::cmd("just", ["--summary"])
            .dir(dir)
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()
        {
            if out.status.success() {
                let set: HashSet<String> = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .collect();
                if !set.is_empty() {
                    return Some(set);
                }
            }
        }
    }
    parse_justfile(&path)
}

fn just_file_in(dir: &Path) -> Option<PathBuf> {
    ["justfile", "Justfile", ".justfile"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
}

fn parse_justfile(path: &Path) -> Option<HashSet<String>> {
    let raw = fs::read_to_string(path).ok()?;
    // First identifier token of a column-0 line that has a `:` (but not `:=`).
    // The regex crate has no lookahead, so the `:=`/`@`/indent handling is plain
    // line iteration. `@name` (quiet recipe) is handled by stripping the `@`.
    let re = Regex::new(r"^([a-zA-Z_][\w-]*)").unwrap();
    let mut set: HashSet<String> = HashSet::new();
    for line in raw.lines() {
        let line = line.strip_prefix('@').unwrap_or(line);
        if line.is_empty() || line.starts_with(char::is_whitespace) || line.starts_with('#') {
            continue;
        }
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let name = caps.get(1).unwrap().as_str();
        let Some(colon) = line.find(':') else {
            continue;
        };
        if line[colon..].starts_with(":=") {
            continue; // justfile assignment, not a recipe
        }
        set.insert(name.to_string());
    }
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// make targets from Makefile/makefile/GNUmakefile. Skips `.PHONY`, pattern
/// rules (`%:`), and `:=` assignments.
fn make_targets() -> Option<HashSet<String>> {
    make_targets_in(Path::new("."))
}

fn make_targets_in(dir: &Path) -> Option<HashSet<String>> {
    let path = ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())?;
    let raw = fs::read_to_string(&path).ok()?;
    // `name:` at column 0. `:=` assignments are excluded post-match (the regex
    // crate has no lookahead, so we inspect the char after the colon).
    let re = Regex::new(r"^([a-zA-Z0-9_][a-zA-Z0-9_.\-]*)\s*:").unwrap();
    let mut set: HashSet<String> = HashSet::new();
    for line in raw.lines() {
        if line.starts_with(char::is_whitespace) {
            continue; // recipe line
        }
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let full = caps.get(0).unwrap();
        if line[full.end()..].starts_with('=') {
            continue; // `name := value` assignment
        }
        let name = caps.get(1).unwrap().as_str();
        if name.contains('%') {
            continue; // pattern rule (`%.o:`)
        }
        set.insert(name.to_string());
    }
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// Path to the gradle build file, if this is a gradle project.
fn gradle_marker() -> Option<PathBuf> {
    ["build.gradle.kts", "build.gradle"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

// ============ gradle (lazy + disk-cached) ============

#[derive(serde::Serialize, serde::Deserialize)]
struct GradleCache {
    /// Composite mtime signature of the marker + settings file (UNIX secs).
    signature: u64,
    tasks: Vec<String>,
}

/// Run `./gradlew tasks --all` (or `gradle …`), parse task names. Cached to
/// `~/.cache/repo/gradle/<fnv1a(root)>.json`, keyed by the marker+settings
/// mtime signature so an edit invalidates it.
fn gradle_tasks() -> Option<Vec<String>> {
    let root = std::env::current_dir().ok()?;
    let canon = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let key = fnv1a64(&canon.to_string_lossy());
    let signature = gradle_signature();

    let cache_base = cache_dir()?.join("gradle");
    let cache_file = cache_base.join(format!("{key:016x}.json"));

    // Hit?
    if let Ok(bytes) = fs::read(&cache_file) {
        if let Ok(c) = serde_json::from_slice::<GradleCache>(&bytes) {
            if c.signature == signature {
                return Some(c.tasks);
            }
        }
    }

    // Miss — invoke gradle.
    let (exe, args): (&str, Vec<&str>) = if Path::new("./gradlew").exists() {
        ("./gradlew", vec!["tasks", "--all", "--console=plain"])
    } else if which::which("gradle").is_ok() {
        ("gradle", vec!["tasks", "--all", "--console=plain"])
    } else {
        return None;
    };

    let out = duct::cmd(exe, &args)
        .stdout_capture()
        .stderr_capture()
        .stdin_null()
        .unchecked()
        .run()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let tasks = parse_gradle_tasks(&text);

    // Persist cache (best-effort).
    if cache_dir().is_some() {
        let _ = fs::create_dir_all(&cache_base);
        let entry = GradleCache {
            signature,
            tasks: tasks.clone(),
        };
        if let Ok(json) = serde_json::to_vec(&entry) {
            let _ = fs::write(&cache_file, json);
        }
    }
    Some(tasks)
}

/// mtime signature (sum of UNIX secs) of `build.gradle(.kts)` + `settings.gradle*`.
fn gradle_signature() -> u64 {
    let files = [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ];
    files.iter().filter_map(|f| mtime_secs(Path::new(f))).sum()
}

fn mtime_secs(p: &Path) -> Option<u64> {
    let meta = fs::metadata(p).ok()?;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Parse `gradle tasks --all` output: `name - description` or bare `name` lines,
/// skipping section headers (multi-word) and `---` separators.
fn parse_gradle_tasks(text: &str) -> Vec<String> {
    let name_re = Regex::new(r"^([a-zA-Z][\w-]*)$").unwrap();
    let desc_re = Regex::new(r"^([a-zA-Z][\w-]*)\s+-\s+").unwrap();
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('>') || t.starts_with('-') || t.starts_with('=') {
            continue;
        }
        if let Some(c) = desc_re.captures(t) {
            if let Some(n) = c.get(1) {
                out.push(n.as_str().to_string());
            }
            continue;
        }
        if let Some(c) = name_re.captures(t) {
            // Bare single-word line. Section headers are multi-word ("Build tasks"),
            // so a bare identifier here is a task name.
            out.push(c.get(1).unwrap().as_str().to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn cache_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("repo"))
}

// ============ tests ============

#[cfg(test)]
impl TaskRunners {
    /// Build a runner set from explicit parts — hermetic tests without touching
    /// the filesystem or the process-wide CWD.
    fn from_parts(
        npm: Option<(String, IndexMap<String, String>)>,
        deno: Option<IndexMap<String, String>>,
        mise: Option<HashSet<String>>,
        just: Option<HashSet<String>>,
        make: Option<HashSet<String>>,
    ) -> Self {
        Self {
            npm,
            deno,
            mise,
            just,
            make,
            gradle_marker: None,
            gradle: OnceLock::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm_map(scripts: &[(&str, &str)]) -> (String, IndexMap<String, String>) {
        let m: IndexMap<String, String> = scripts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ("npm".to_string(), m)
    }

    #[test]
    fn npm_scripts_argv() {
        let t = TaskRunners::from_parts(
            Some(npm_map(&[("dev", "node server.js")])),
            None,
            None,
            None,
            None,
        );
        let f = t.find("dev").unwrap();
        assert_eq!(f.runner, Runner::Npm("npm".into()));
        assert_eq!(f.argv, vec!["npm", "run", "dev"]);
    }

    #[test]
    fn npm_missing_name_is_none() {
        let t = TaskRunners::from_parts(Some(npm_map(&[("dev", "x")])), None, None, None, None);
        assert!(t.find("nope").is_none());
    }

    #[test]
    fn mise_argv() {
        let mut mise = HashSet::new();
        mise.insert("lint".to_string());
        let t = TaskRunners::from_parts(None, None, Some(mise), None, None);
        let f = t.find("lint").unwrap();
        assert_eq!(f.runner, Runner::Mise);
        assert_eq!(f.argv, vec!["mise", "run", "lint"]);
        assert_eq!(f.display(), "mise run lint");
    }

    #[test]
    fn parse_justfile_extracts_recipes() {
        let tmp = tempfile_dir();
        let just = tmp.join("justfile");
        fs::write(
            &just,
            "build:\n    cargo build\ndev:\n    cargo watch -x run\n_private:\n    echo hi\n",
        )
        .unwrap();
        let set = parse_justfile(&just).unwrap();
        assert!(set.contains("build"));
        assert!(set.contains("dev"));
        assert!(set.contains("_private"));
    }

    #[test]
    fn parse_justfile_skips_assignments() {
        let tmp = tempfile_dir();
        let just = tmp.join("justfile");
        fs::write(&just, "VERSION := \"1.2\"\nbuild:\n    cargo build\n").unwrap();
        let set = parse_justfile(&just).unwrap();
        assert!(set.contains("build"));
        assert!(!set.contains("VERSION"), "`:=` assignment is not a recipe");
    }

    #[test]
    fn make_targets_skip_phony_and_patterns() {
        let tmp = tempfile_dir();
        fs::write(
            tmp.join("Makefile"),
            ".PHONY: build\nbuild:\n\tcargo build\n%.o: %.c\n\tgcc -c $\nCLEAN_UP:\n\trm -rf target\n",
        )
        .unwrap();
        let set = make_targets_in(&tmp).unwrap();
        assert!(set.contains("build"));
        assert!(set.contains("CLEAN_UP"));
        assert!(!set.contains(".PHONY"), ".PHONY line must not be a target");
        assert!(
            !set.iter().any(|t| t.contains('%')),
            "pattern rules excluded"
        );
    }

    #[test]
    fn make_targets_skip_assignments() {
        let tmp = tempfile_dir();
        fs::write(
            tmp.join("Makefile"),
            "VERSION := 1.2\nGOOS ?= linux\nall:\n\t@echo hi\n",
        )
        .unwrap();
        let set = make_targets_in(&tmp).unwrap();
        assert!(set.contains("all"));
        assert!(!set.contains("VERSION"), "`:=` is an assignment");
        assert!(!set.contains("GOOS"), "`?=` is an assignment");
    }

    #[test]
    fn gradle_task_parsing() {
        let out = "\
------------------------------------------------------------
Tasks runnable from root project 'demo'
------------------------------------------------------------

Build tasks
-----------
assemble - Assembles the outputs of this project.
build - Assembles and tests this project.

Verification tasks
------------------
test - Runs the unit tests.
check";
        let tasks = parse_gradle_tasks(out);
        assert!(tasks.contains(&"assemble".to_string()));
        assert!(tasks.contains(&"build".to_string()));
        assert!(tasks.contains(&"test".to_string()));
        assert!(tasks.contains(&"check".to_string()));
        assert!(!tasks.contains(&"Build".to_string()));
        assert!(!tasks.iter().any(|t| t == "Tasks"), "header line excluded");
    }

    #[test]
    fn priority_just_beats_npm() {
        let mut just = HashSet::new();
        just.insert("dev".to_string());
        let t =
            TaskRunners::from_parts(Some(npm_map(&[("dev", "x")])), None, None, Some(just), None);
        let f = t.find("dev").unwrap();
        assert_eq!(f.runner, Runner::Just, "just must outrank npm");
        assert_eq!(f.argv, vec!["just", "dev"]);
    }

    #[test]
    fn priority_mise_beats_just() {
        let mut mise = HashSet::new();
        mise.insert("dev".to_string());
        let mut just = HashSet::new();
        just.insert("dev".to_string());
        let t = TaskRunners::from_parts(None, None, Some(mise), Some(just), None);
        let f = t.find("dev").unwrap();
        assert_eq!(f.runner, Runner::Mise, "mise must outrank just");
        assert_eq!(f.argv, vec!["mise", "run", "dev"]);
    }

    #[test]
    fn priority_order_full() {
        // All runners define "x"; verify exact order mise → just → make → deno → npm.
        let mk_set = || {
            let mut s = HashSet::new();
            s.insert("x".to_string());
            Some(s)
        };
        let mk_deno = || {
            let mut m = IndexMap::new();
            m.insert("x".to_string(), "y".to_string());
            Some(m)
        };
        let mk_npm = || Some(npm_map(&[("x", "y")]));

        let t = TaskRunners::from_parts(mk_npm(), mk_deno(), mk_set(), mk_set(), mk_set());
        assert_eq!(t.find("x").unwrap().runner, Runner::Mise);

        let t = TaskRunners::from_parts(mk_npm(), mk_deno(), None, mk_set(), mk_set());
        assert_eq!(t.find("x").unwrap().runner, Runner::Just);

        let t = TaskRunners::from_parts(mk_npm(), mk_deno(), None, None, mk_set());
        assert_eq!(t.find("x").unwrap().runner, Runner::Make);

        let t = TaskRunners::from_parts(mk_npm(), mk_deno(), None, None, None);
        assert_eq!(t.find("x").unwrap().runner, Runner::Deno);

        let t = TaskRunners::from_parts(mk_npm(), None, None, None, None);
        assert_eq!(t.find("x").unwrap().runner, Runner::Npm("npm".into()));
    }

    #[test]
    fn find_any_returns_first_hit() {
        let t = TaskRunners::from_parts(Some(npm_map(&[("start", "x")])), None, None, None, None);
        let f = t.find_any(&["run", "start", "dev"]).unwrap();
        assert_eq!(f.argv, vec!["npm", "run", "start"]);
        assert!(t.find_any(&["run", "dev"]).is_none());
    }

    #[test]
    fn list_all_runners_for_name() {
        let mut mise = HashSet::new();
        mise.insert("build".into());
        let mut just = HashSet::new();
        just.insert("build".into());
        let t = TaskRunners::from_parts(
            Some(npm_map(&[("build", "x")])),
            None,
            Some(mise),
            Some(just),
            None,
        );
        let found = t.list("build");
        // mise, just and npm define "build"; all reported, priority order.
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].runner, Runner::Mise);
        assert_eq!(found[1].runner, Runner::Just);
        assert_eq!(found[2].runner, Runner::Npm("npm".into()));
    }

    #[test]
    fn parse_mise_toml_extracts_tasks() {
        let raw = "[tasks.build]\nrun = \"cargo build\"\n\n[tasks.lint]\nrun = \"x\"\n";
        let set = parse_mise_toml(raw);
        assert!(set.contains("build"));
        assert!(set.contains("lint"));
    }

    #[test]
    fn parse_mise_toml_sub_tables_and_quotes() {
        let raw = "[tasks.build.env]\nFOO = \"1\"\n\
                   [tasks.\"my task\"]\nrun = \"x\"\n\
                   [tasks.'quoted-dash']\nrun = \"x\"\n\
                   [tasks]\nbuild = \"inline\"\n\
                   # [tasks.commented]\n";
        let set = parse_mise_toml(raw);
        // `[tasks.build.env]` still means task `build`, not `build.env`.
        assert!(set.contains("build"));
        assert!(!set.iter().any(|t| t.contains('.')), "no dotted names");
        assert!(set.contains("my task"), "quoted names keep spaces");
        assert!(set.contains("quoted-dash"));
        assert!(!set.contains("commented"), "commented headers ignored");
        assert!(!set.contains("tasks"), "bare `[tasks]` is not a task");
    }

    #[cfg(unix)]
    #[test]
    fn mise_file_tasks_grouping_and_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile_dir().join("mise-file-tasks");
        let _ = fs::remove_dir_all(&tmp);
        let task_dir = tmp.join("mise-tasks");
        fs::create_dir_all(task_dir.join("test")).unwrap();
        fs::create_dir_all(task_dir.join("deploy")).unwrap();
        let exe = |p: &Path| {
            fs::write(p, "#!/usr/bin/env bash\necho hi\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        exe(&task_dir.join("hello.sh"));
        exe(&task_dir.join("test").join("units"));
        exe(&task_dir.join("test").join("_default"));
        exe(&task_dir.join("deploy").join("ok.sh"));
        // No exec bit → invisible, like mise itself.
        fs::write(task_dir.join("deploy").join("noexec"), "echo hi\n").unwrap();
        // Hidden files hold no tasks.
        exe(&task_dir.join(".hidden"));
        // `*.toml` is not a file-task format.
        fs::write(task_dir.join("weird.toml"), "run = \"x\"\n").unwrap();

        let set = mise_file_tasks_in(&task_dir);
        assert!(set.contains("hello"), "extension stripped: {set:?}");
        assert!(set.contains("test"), "`_default` names the group: {set:?}");
        assert!(
            set.contains("test:units"),
            "subdir groups with `:`: {set:?}"
        );
        assert!(set.contains("deploy:ok"), "{set:?}");
        assert!(!set.iter().any(|t| t.contains("noexec")), "needs exec bit");
        assert!(
            !set.iter().any(|t| t.contains("hidden")),
            "dotfiles skipped"
        );
        assert!(!set.iter().any(|t| t.contains("weird")), "*.toml skipped");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_mise_configs_merges_toml_and_dirs() {
        let tmp = tempfile_dir().join("mise-configs");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            tmp.join("mise.toml"),
            "[tasks.build]\nrun = \"cargo build\"\n",
        )
        .unwrap();
        fs::write(tmp.join(".mise.local.toml"), "[tasks.local]\nrun = \"x\"\n").unwrap();
        // A project without the mise binary falls back to file parsing.
        let set = parse_mise_configs(&tmp).unwrap();
        assert!(set.contains("build"));
        assert!(set.contains("local"));
        let _ = fs::remove_dir_all(&tmp);
    }

    fn tempfile_dir() -> PathBuf {
        // Unique per test thread to avoid races between parallel tests.
        let dir = std::env::temp_dir().join(format!(
            "repo-tasks-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace(':', "-")
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }
}
