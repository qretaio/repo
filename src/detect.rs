//! Project detection + command tables — fully data-driven.
//!
//! Detection rules and command definitions live in an embedded YAML
//! (`defaults.yaml`), overridable by `~/.config/repo/repo.yaml` (global) and
//! `./repo.yaml` (local). CEL expressions power both *detection* (`detect`
//! field) and per-command *conditions* (`when` field). See the header of
//! `defaults.yaml` for the host-function reference.

use anyhow::Context as _;
use cel_interpreter::{Context, Program, Value};
use colored::Colorize;
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct CommandDef {
    pub name: String,
    pub cmd: Vec<String>,
    when: Option<Arc<Program>>,
    pub check_cmd: Option<Vec<String>>,
    pub fix_cmd: Option<Vec<String>>,
    pub cost: u32,
}

impl CommandDef {
    /// Lint-style mode resolution: use `fix_cmd` when fixing, else `cmd`.
    pub fn resolve_fix(&self, fix: bool) -> &[String] {
        if fix {
            self.fix_cmd.as_ref().unwrap_or(&self.cmd)
        } else {
            &self.cmd
        }
    }
}

#[derive(Clone, Default)]
pub struct Commands {
    pub lint: Vec<CommandDef>,
    pub fmt: Vec<CommandDef>,
    pub build: Vec<CommandDef>,
    pub test: Vec<CommandDef>,
    pub install: Vec<CommandDef>,
    pub dev: Vec<CommandDef>,
    pub run: Vec<CommandDef>,
}

impl Commands {
    pub fn get(&self, kind: Kind) -> &[CommandDef] {
        match kind {
            Kind::Lint => &self.lint,
            Kind::Fmt => &self.fmt,
            Kind::Build => &self.build,
            Kind::Test => &self.test,
            Kind::Install => &self.install,
            Kind::Dev => &self.dev,
            Kind::Run => &self.run,
        }
    }

    fn all_cmds(&self) -> impl Iterator<Item = &CommandDef> {
        self.lint
            .iter()
            .chain(self.fmt.iter())
            .chain(self.build.iter())
            .chain(self.test.iter())
            .chain(self.install.iter())
            .chain(self.dev.iter())
            .chain(self.run.iter())
    }
}

pub struct ProjectType {
    pub id: String,
    pub name: String,
    detect: Arc<Program>,
    pub commands: Commands,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lint,
    Fmt,
    Build,
    Test,
    Install,
    Dev,
    Run,
}

// ============ detector ============

pub struct Detector {
    projects: Vec<ProjectType>,
    universal: Commands,
    cel: Context<'static>,
}

impl Detector {
    pub fn new(cwd: PathBuf) -> anyhow::Result<Self> {
        std::env::set_current_dir(find_root(&cwd))?;

        let pkg = read_json("package.json");
        let pm = detect_package_manager().unwrap_or("npm");
        let cfg = load_config()?;
        let mut cel = cel_context(&pkg);

        // Phase 2 — compile detect exprs, build projects, evaluate detection.
        let mut projects = Vec::with_capacity(cfg.projects.len());
        let mut detected = HashSet::new();
        for (id, p) in &cfg.projects {
            let detect = Arc::new(compile_cel(&p.detect, id)?);
            validate_bool(&detect, &cel, id)?;
            if eval_bool(&detect, &cel) {
                detected.insert(id.clone());
            }
            let commands = build_commands(p, pm)?;
            projects.push(ProjectType {
                name: p.name.clone().unwrap_or_else(|| id.clone()),
                id: id.clone(),
                detect,
                commands,
            });
        }

        // Phase 3 — expose detection results to command `when` expressions.
        let det = Arc::new(detected);
        cel.add_function("project", move |id: Arc<String>| -> bool {
            det.contains(id.as_str())
        });

        // Phase 4 — validate every `when` expression (project() now available).
        for p in &projects {
            for c in p.commands.all_cmds() {
                if let Some(w) = &c.when {
                    validate_bool(w, &cel, &format!("{}.{}", p.id, c.name))?;
                }
            }
        }

        // Phase 5 — build + validate universal commands.
        let universal = Commands {
            lint: build_cmds(&cfg.lint, pm)?,
            fmt: build_cmds(&cfg.fmt, pm)?,
            build: build_cmds(&cfg.build, pm)?,
            test: build_cmds(&cfg.test, pm)?,
            install: build_cmds(&cfg.install, pm)?,
            dev: build_cmds(&cfg.dev, pm)?,
            run: build_cmds(&cfg.run, pm)?,
        };
        for c in universal.all_cmds() {
            if let Some(w) = &c.when {
                validate_bool(w, &cel, &format!("universal.{}", c.name))?;
            }
        }

        Ok(Self {
            projects,
            universal,
            cel,
        })
    }

    pub fn detect_project_types(&self) -> Vec<&ProjectType> {
        self.projects
            .iter()
            .filter(|p| eval_bool(&p.detect, &self.cel))
            .collect()
    }

    pub fn universal_commands(&self) -> &Commands {
        &self.universal
    }

    pub fn get_applicable(&self, cmds: &[CommandDef]) -> Vec<CommandDef> {
        cmds.iter()
            .filter(|c| c.when.as_ref().is_none_or(|w| eval_bool(w, &self.cel)))
            .cloned()
            .collect()
    }

    pub fn get_commands_by_type(&self, kind: Kind) -> Vec<CommandDef> {
        let detected = self.detect_project_types();
        let mut out: Vec<CommandDef> = self.get_applicable(self.universal.get(kind));
        let multi = detected.len() > 1;
        for project in &detected {
            for mut cmd in self.get_applicable(project.commands.get(kind)) {
                if multi {
                    cmd.name = format!("{}: {}", project.name, cmd.name);
                }
                out.push(cmd);
            }
        }
        out
    }

    pub fn list_commands(&self, kind: Kind, label: &str, include_universal: bool) {
        let detected = self.detect_project_types();
        let universal = if include_universal {
            self.get_applicable(self.universal.get(kind))
        } else {
            vec![]
        };

        println!("{}", format!("Detected {label}:").blue());

        if detected.is_empty() && universal.is_empty() {
            println!("{}", format!("  No {label} found").yellow());
            return;
        }

        if !universal.is_empty() {
            println!();
            println!("{}", "Universal:".blue());
            for c in &universal {
                println!("  - {}: {}", c.name, c.cmd.join(" "));
            }
        }

        for project in &detected {
            let commands = self.get_applicable(project.commands.get(kind));
            if commands.is_empty() {
                continue;
            }
            println!();
            println!("  {}:", project.name.green());
            for c in &commands {
                println!("    - {}: {}", c.name, c.cmd.join(" "));
            }
        }
    }
}

/// Resolve the command array for a given mode (mirrors getCommandForMode).
pub fn command_for_mode(cmd: &CommandDef, check_mode: bool) -> &[String] {
    if check_mode {
        if let Some(c) = cmd.check_cmd.as_ref() {
            return c;
        }
    } else if let Some(c) = cmd.fix_cmd.as_ref() {
        return c;
    }
    &cmd.cmd
}

// ---- project root + file helpers ----

/// Walk up from `start` to the nearest directory containing `.git`.
/// Falls back to `start` itself if none found (non-Git projects).
fn find_root(start: &Path) -> PathBuf {
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    start.to_path_buf()
}

fn read_json(rel: &str) -> Option<serde_json::Value> {
    fs::read_to_string(rel)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn detect_package_manager() -> Option<&'static str> {
    if Path::new("pnpm-lock.yaml").exists() {
        Some("pnpm")
    } else if Path::new("yarn.lock").exists() {
        Some("yarn")
    } else if Path::new("bun.lockb").exists() {
        Some("bun")
    } else if Path::new("package-lock.json").exists() {
        Some("npm")
    } else {
        None
    }
}

const DEFAULTS: &str = include_str!("defaults.yaml");

#[derive(Deserialize, Default)]
struct RepoConfig {
    #[serde(default)]
    projects: IndexMap<String, ProjectRaw>,
    #[serde(default)]
    lint: Vec<CommandRaw>,
    #[serde(default)]
    fmt: Vec<CommandRaw>,
    #[serde(default)]
    build: Vec<CommandRaw>,
    #[serde(default)]
    test: Vec<CommandRaw>,
    #[serde(default)]
    install: Vec<CommandRaw>,
    #[serde(default)]
    dev: Vec<CommandRaw>,
    #[serde(default)]
    run: Vec<CommandRaw>,
}

#[derive(Deserialize)]
struct ProjectRaw {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    detect: String,
    #[serde(default)]
    lint: Vec<CommandRaw>,
    #[serde(default)]
    fmt: Vec<CommandRaw>,
    #[serde(default)]
    build: Vec<CommandRaw>,
    #[serde(default)]
    test: Vec<CommandRaw>,
    #[serde(default)]
    install: Vec<CommandRaw>,
    #[serde(default)]
    dev: Vec<CommandRaw>,
    #[serde(default)]
    run: Vec<CommandRaw>,
}

#[derive(Deserialize)]
struct CommandRaw {
    name: String,
    cmd: String,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    fix: Option<String>,
    #[serde(default)]
    check: Option<String>,
    #[serde(default)]
    cost: u32,
}

fn load_config() -> anyhow::Result<RepoConfig> {
    let mut configs = Vec::new();

    configs.push(parse_yaml(DEFAULTS, "embedded defaults")?);

    if let Some(dir) = global_config_dir() {
        let path = dir.join("repo.yaml");
        if let Ok(content) = fs::read_to_string(&path) {
            configs.push(parse_yaml(&content, &path.display().to_string())?);
        }
    }

    if let Ok(content) = fs::read_to_string("repo.yaml") {
        configs.push(parse_yaml(&content, "repo.yaml")?);
    }

    Ok(configs.into_iter().reduce(merge_pair).unwrap_or_default())
}

fn parse_yaml(content: &str, source: &str) -> anyhow::Result<RepoConfig> {
    serde_yaml::from_str(content).with_context(|| format!("failed to parse {source}"))
}

fn global_config_dir() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|p| p.join("repo"))
}

/// Deep-merge `over` into `base`. Projects merge by id; command lists
/// merge by name (replace existing, append new).
fn merge_pair(mut base: RepoConfig, over: RepoConfig) -> RepoConfig {
    for (id, p) in over.projects {
        if let Some(b) = base.projects.get_mut(&id) {
            if !p.detect.is_empty() {
                b.detect = p.detect;
            }
            if p.name.is_some() {
                b.name = p.name;
            }
            merge_cmds(&mut b.lint, p.lint);
            merge_cmds(&mut b.fmt, p.fmt);
            merge_cmds(&mut b.build, p.build);
            merge_cmds(&mut b.test, p.test);
            merge_cmds(&mut b.install, p.install);
            merge_cmds(&mut b.dev, p.dev);
            merge_cmds(&mut b.run, p.run);
        } else {
            base.projects.insert(id, p);
        }
    }
    merge_cmds(&mut base.lint, over.lint);
    merge_cmds(&mut base.fmt, over.fmt);
    merge_cmds(&mut base.build, over.build);
    merge_cmds(&mut base.test, over.test);
    merge_cmds(&mut base.install, over.install);
    merge_cmds(&mut base.dev, over.dev);
    merge_cmds(&mut base.run, over.run);
    base
}

fn merge_cmds(base: &mut Vec<CommandRaw>, over: Vec<CommandRaw>) {
    for cmd in over {
        if let Some(i) = base.iter().position(|c| c.name == cmd.name) {
            base[i] = cmd;
        } else {
            base.push(cmd);
        }
    }
}

// ============ command building ============

fn build_commands(p: &ProjectRaw, pm: &str) -> anyhow::Result<Commands> {
    Ok(Commands {
        lint: build_cmds(&p.lint, pm)?,
        fmt: build_cmds(&p.fmt, pm)?,
        build: build_cmds(&p.build, pm)?,
        test: build_cmds(&p.test, pm)?,
        install: build_cmds(&p.install, pm)?,
        dev: build_cmds(&p.dev, pm)?,
        run: build_cmds(&p.run, pm)?,
    })
}

fn build_cmds(raws: &[CommandRaw], pm: &str) -> anyhow::Result<Vec<CommandDef>> {
    raws.iter().map(|r| build_cmd(r, pm)).collect()
}

fn build_cmd(raw: &CommandRaw, pm: &str) -> anyhow::Result<CommandDef> {
    Ok(CommandDef {
        name: raw.name.clone(),
        cmd: sub_pm(&raw.cmd, pm),
        when: raw
            .when
            .as_ref()
            .map(|s| compile_cel(s, &raw.name).map(Arc::new))
            .transpose()?,
        fix_cmd: raw.fix.as_ref().map(|s| sub_pm(s, pm)),
        check_cmd: raw.check.as_ref().map(|s| sub_pm(s, pm)),
        cost: raw.cost,
    })
}

/// Replace `{pm}` then split on whitespace into argv.
fn sub_pm(s: &str, pm: &str) -> Vec<String> {
    s.replace("{pm}", pm)
        .split_whitespace()
        .map(String::from)
        .collect()
}

// ============ CEL helpers ============

fn compile_cel(expr: &str, label: &str) -> anyhow::Result<Program> {
    Program::compile(expr).map_err(|e| anyhow::anyhow!("CEL error in '{label}': {e}"))
}

fn eval_bool(prog: &Program, ctx: &Context) -> bool {
    matches!(prog.execute(ctx), Ok(Value::Bool(true)))
}

fn validate_bool(prog: &Program, ctx: &Context, label: &str) -> anyhow::Result<()> {
    match prog.execute(ctx) {
        Ok(Value::Bool(_)) => Ok(()),
        Ok(v) => anyhow::bail!("CEL '{label}' must evaluate to bool, got {v:?}"),
        Err(e) => anyhow::bail!("CEL '{label}': {e}"),
    }
}

/// Build a CEL evaluation context.
/// CWD was set to the project root by `Detector::new`, so all paths are relative.
fn cel_context(pkg: &Option<serde_json::Value>) -> Context<'static> {
    let pkg = Arc::new(pkg.clone());
    let mut ctx = Context::default();

    // repo_root — available to expressions that need it, but not required.
    if let Ok(dir) = std::env::current_dir() {
        let _ = ctx.add_variable("repo_root", dir.to_string_lossy().into_owned());
    }

    // glob(pattern) — true if any file matches.
    ctx.add_function("glob", move |pattern: Arc<String>| -> bool {
        let p = pattern.as_str();
        if !p.contains('*') && !p.contains('?') && !p.contains('[') {
            return Path::new(p).exists();
        }
        glob::glob(p).is_ok_and(|mut it| it.next().is_some())
    });

    // contains(file, needle) — file content includes needle.
    ctx.add_function(
        "contains",
        move |name: Arc<String>, needle: Arc<String>| -> bool {
            fs::read_to_string(name.as_str()).is_ok_and(|c| c.contains(needle.as_str()))
        },
    );

    // script(name) — package.json defines a script named `name`.
    ctx.add_function("script", move |name: Arc<String>| -> bool {
        pkg.as_ref()
            .as_ref()
            .and_then(|v| v.get("scripts"))
            .and_then(|s| s.get(name.as_str()))
            .is_some()
    });

    // bin(name) — binary is on PATH.
    ctx.add_function("bin", move |name: Arc<String>| -> bool {
        which::which(name.as_str()).is_ok()
    });

    ctx
}

// ============ tests ============

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    #[test]
    fn cel_host_functions() {
        let root = find_root(&cwd());
        let _ = std::env::set_current_dir(&root);
        let pkg = read_json("package.json");
        let ctx = cel_context(&pkg);
        let eval = |src: &str| -> bool {
            matches!(
                Program::compile(src).unwrap().execute(&ctx),
                Ok(Value::Bool(true))
            )
        };
        assert!(eval("glob('Cargo.toml')"));
        assert!(!eval("glob('nope.xyz')"));
        assert!(eval("glob('src/**/*.rs')"));
        assert!(eval("glob('*.toml')"));
        assert!(!eval("glob('*.py')"));
        assert!(eval("contains('Cargo.toml', '[package]')"));
        assert!(!eval("contains('Cargo.toml', 'zz-not-present')"));
    }

    #[test]
    fn repo_root_available() {
        let root = find_root(&cwd());
        let _ = std::env::set_current_dir(&root);
        let ctx = cel_context(&None);
        let val = Program::compile("repo_root")
            .unwrap()
            .execute(&ctx)
            .unwrap();
        assert!(matches!(val, Value::String(ref s) if s.contains("src/repo")));
    }

    #[test]
    fn find_root_from_subdir() {
        let root = find_root(&cwd());
        let subdir = root.join("src/commands");
        let found = find_root(&subdir);
        assert_eq!(found, root, "should walk up to .git boundary");
    }

    #[test]
    fn defaults_load_and_detect_rust() {
        let d = Detector::new(cwd()).unwrap();
        let ids: Vec<&str> = d
            .detect_project_types()
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert!(ids.contains(&"rust"), "rust should be detected: {ids:?}");
    }

    #[test]
    fn sub_pm_splits_and_substitutes() {
        assert_eq!(sub_pm("cargo build", "npm"), vec!["cargo", "build"]);
        assert_eq!(sub_pm("{pm} test", "pnpm"), vec!["pnpm", "test"]);
        assert_eq!(
            sub_pm("{pm} run build", "yarn"),
            vec!["yarn", "run", "build"]
        );
    }

    #[test]
    fn merge_extends_and_overrides() {
        let base = parse_yaml(DEFAULTS, "test").unwrap();
        let over = parse_yaml(
            r#"
projects:
  rust:
    lint:
      - name: Custom Linter
        cmd: my-linter .
      - name: Clippy
        cmd: cargo clippy --quiet
"#,
            "test",
        )
        .unwrap();
        let merged = merge_pair(base, over);
        let rust = merged.projects.get("rust").unwrap();
        // New command appended
        assert!(rust.lint.iter().any(|c| c.name == "Custom Linter"));
        // Existing command overridden by name, not duplicated
        let clippy = rust.lint.iter().find(|c| c.name == "Clippy").unwrap();
        assert_eq!(clippy.cmd, "cargo clippy --quiet");
        assert_eq!(rust.lint.iter().filter(|c| c.name == "Clippy").count(), 1);
        // Other commands survive
        assert!(rust.lint.iter().any(|c| c.name == "Format Check"));
        assert!(!rust.fmt.is_empty(), "fmt should survive");
    }
}
