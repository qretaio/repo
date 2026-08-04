//! Project detection module — detects project types and available commands.
//! Built-in detection is marker-file based; rich boolean detection (AND/OR,
//! source-layout, marker-content) is configurable via CEL expressions in
//! `repo.toml` (see [`CompiledRule`] / [`Detector::load_rules`]).

use anyhow::Context as _;
use cel_interpreter::{Context, Program, Value};
use colored::Colorize;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Predicate over the detector. Pure (captures nothing) so it coerces to a fn pointer.
pub type Pred = fn(&Detector) -> bool;

#[derive(Clone)]
pub struct CommandDef {
    pub name: String,
    pub cmd: Vec<String>,
    pub only_if: Option<Pred>,
    pub check_cmd: Option<Vec<String>>,
    pub fix_cmd: Option<Vec<String>>,
    pub heavy: bool,
}

impl CommandDef {
    fn new(name: &str, cmd: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            only_if: None,
            check_cmd: None,
            fix_cmd: None,
            heavy: false,
        }
    }
    fn when(mut self, p: Pred) -> Self {
        self.only_if = Some(p);
        self
    }
    fn heavy(mut self) -> Self {
        self.heavy = true;
        self
    }
    fn fix(mut self, c: &[&str]) -> Self {
        self.fix_cmd = Some(c.iter().map(|s| s.to_string()).collect());
        self
    }
    fn check(mut self, c: &[&str]) -> Self {
        self.check_cmd = Some(c.iter().map(|s| s.to_string()).collect());
        self
    }
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
}

#[derive(Clone)]
pub struct ProjectType {
    pub name: String,
    pub id: String,
    /// Built-in fallback: project is detected when ANY of these entries exist.
    /// A `repo.toml` `[[detect]]` rule with the same `id` overrides this.
    pub detect_files: &'static [&'static str],
    pub commands: Commands,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lint,
    Fmt,
    Build,
    Test,
}

impl Commands {
    pub fn get(&self, kind: Kind) -> &[CommandDef] {
        match kind {
            Kind::Lint => &self.lint,
            Kind::Fmt => &self.fmt,
            Kind::Build => &self.build,
            Kind::Test => &self.test,
        }
    }
}

/// Holds pre-parsed project state so predicates are cheap and deterministic.
pub struct Detector {
    cwd: PathBuf,
    /// All immediate entries of `cwd`, scanned once at construction. Every
    /// `exists()` call is an O(1) set lookup instead of a `stat` syscall.
    entries: HashSet<String>,
    pkg: Option<serde_json::Value>,
    pyproject: Option<String>,
    requirements: Option<String>,
    node_pm: Option<&'static str>,
    #[allow(dead_code)] // Phase 2: detectProjectTypeFlags.packageManager
    python_pm: Option<&'static str>,
    /// Compiled `repo.toml` `[[detect]]` rules. Override built-in detection by
    /// `id`, or declare new detect-only project types. Empty when no config.
    rules: Vec<CompiledRule>,
}

impl Detector {
    pub fn new(cwd: PathBuf) -> anyhow::Result<Self> {
        let entries = scan_entries(&cwd);
        // Read file contents only when the file actually exists.
        let read = |rel: &str| -> Option<String> {
            if entries.contains(rel) {
                fs::read_to_string(cwd.join(rel)).ok()
            } else {
                None
            }
        };
        let pyproject = read("pyproject.toml");
        let pkg = read("package.json").and_then(|s| serde_json::from_str(&s).ok());
        let requirements = read("requirements.txt");
        let node_pm = detect_package_manager(&entries);
        let python_pm = detect_python_package_manager(&entries, &pyproject);
        let rules = load_rules(&cwd)?;
        // Fail loud: every rule must compile (done in load_rules) AND evaluate
        // to a bool against this snapshot, so a typo'd host function or a
        // non-boolean expression is caught at startup, not mid-lint.
        for rule in &rules {
            let ctx = cel_context(&cwd, &entries);
            match rule.program.execute(&ctx) {
                Ok(Value::Bool(_)) => {}
                Ok(v) => anyhow::bail!(
                    "repo.toml detect[{}]: expression must evaluate to bool, got {:?}",
                    rule.id,
                    v
                ),
                Err(e) => anyhow::bail!("repo.toml detect[{}]: {}", rule.id, e),
            }
        }
        Ok(Self {
            cwd,
            entries,
            pkg,
            pyproject,
            requirements,
            node_pm,
            python_pm,
            rules,
        })
    }

    /// O(1) membership check against the single up-front directory scan.
    /// Only meaningful for immediate entries of `cwd` (all callers pass bare
    /// filenames); nested paths use `has_files_with_ext` / `read`.
    fn exists(&self, rel: &str) -> bool {
        self.entries.contains(rel)
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.cwd.join(rel)).ok()
    }

    fn node_pm_or(&self, default: &str) -> String {
        self.node_pm.unwrap_or(default).to_string()
    }

    fn has_files_with_ext(&self, dir: &str, ext: &str) -> bool {
        dir_has_ext(&self.cwd.join(dir), ext)
    }

    #[allow(dead_code)] // Phase 2: detectProjectTypeFlags (frameworks)
    fn has_dependency(&self, dep: &str) -> bool {
        let Some(pkg) = &self.pkg else {
            return false;
        };
        for field in ["dependencies", "devDependencies"] {
            if let Some(obj) = pkg.get(field).and_then(|v| v.as_object()) {
                for name in obj.keys() {
                    if name == dep || name.starts_with(&format!("{dep}/")) {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[allow(dead_code)] // Phase 2: detectProjectTypeFlags (django)
    fn has_python_dependency(&self, dep: &str) -> bool {
        let Some(content) = &self.pyproject else {
            return false;
        };
        python_dep_match(content, dep)
    }

    #[allow(dead_code)] // Phase 2: detectProjectTypeFlags (django)
    fn has_requirements_dependency(&self, dep: &str) -> bool {
        let Some(content) = &self.requirements else {
            return false;
        };
        content.lines().any(|line| {
            let t = line.trim_start();
            t.starts_with(dep) && t[dep.len()..].starts_with(['=', '<', '>', '~'])
        })
    }

    fn has_script(&self, script: &str) -> bool {
        self.pkg
            .as_ref()
            .and_then(|p| p.get("scripts"))
            .and_then(|s| s.get(script))
            .is_some()
    }

    fn has_typescript(&self) -> bool {
        ["tsconfig.json", "tsconfig.base.json", "tsconfig.build.json"]
            .iter()
            .any(|f| self.exists(f))
    }

    fn has_eslint(&self) -> bool {
        [
            ".eslintrc.js",
            ".eslintrc.json",
            ".eslintrc.yaml",
            ".eslintrc.yml",
            ".eslintrc",
            "eslint.config.js",
            "eslint.config.mjs",
            "eslint.config.cjs",
        ]
        .iter()
        .any(|f| self.exists(f))
    }

    fn has_prettier(&self) -> bool {
        [
            ".prettierrc",
            ".prettierrc.json",
            ".prettierrc.yaml",
            ".prettierrc.yml",
            ".prettierrc.json5",
            ".prettierrc.js",
            ".prettierrc.cjs",
            "prettier.config.js",
            "prettier.config.cjs",
        ]
        .iter()
        .any(|f| self.exists(f))
    }

    fn has_spotless(&self) -> bool {
        (self.exists("build.gradle") || self.exists("build.gradle.kts"))
            && self
                .read(if self.exists("build.gradle") {
                    "build.gradle"
                } else {
                    "build.gradle.kts"
                })
                .unwrap_or_default()
                .contains("spotless")
    }

    pub fn detect_project_types(&self) -> Vec<ProjectType> {
        let ctx = cel_context(&self.cwd, &self.entries);
        let mut out: Vec<ProjectType> = Vec::new();
        // Built-ins: a config rule with the same id overrides marker-file
        // detection; otherwise fall back to detect_files.
        for mut p in self.all_project_types() {
            match self.rules.iter().find(|r| r.id == p.id) {
                Some(rule) if rule.eval_bool(&ctx) => {
                    if !rule.name.is_empty() {
                        p.name = rule.name.clone();
                    }
                    out.push(p);
                }
                None if p.detect_files.iter().any(|f| self.exists(f)) => out.push(p),
                _ => {}
            }
        }
        // Config-only types (id not built-in): detected, but carry no commands
        // until command tables are also configurable (future work).
        for rule in &self.rules {
            if self.all_project_types().iter().any(|p| p.id == rule.id) {
                continue;
            }
            if rule.eval_bool(&ctx) {
                out.push(ProjectType {
                    name: rule.name.clone(),
                    id: rule.id.clone(),
                    detect_files: &[],
                    commands: Commands::default(),
                });
            }
        }
        out
    }

    /// All five project types, unconditionally built (table source of truth).
    fn all_project_types(&self) -> Vec<ProjectType> {
        let npm = self.node_pm_or("npm");
        vec![
            self.node_project(&npm),
            self.python_project(),
            self.rust_project(),
            self.go_project(),
            self.jvm_project(),
        ]
    }

    fn node_project(&self, npm: &str) -> ProjectType {
        ProjectType {
            name: "Node.js".into(),
            id: "nodejs".into(),
            detect_files: &["package.json"],
            commands: Commands {
                lint: vec![
                    CommandDef::new("ESLint", &["npx", "eslint", "."])
                        .fix(&["npx", "eslint", "--fix", "."])
                        .when(|d| d.has_eslint()),
                    CommandDef::new("Type Check", &["npx", "tsc", "--noEmit"])
                        .when(|d| d.has_typescript()),
                    CommandDef::new("Format Check", &["npx", "prettier", "--check", "."])
                        .fix(&["npx", "prettier", "--write", "."])
                        .when(|d| d.has_prettier()),
                    CommandDef::new("Security Audit", &[npm, "audit"])
                        .when(|d| d.exists("package.json"))
                        .heavy(),
                ],
                fmt: vec![
                    CommandDef::new(
                        "Prettier",
                        &["npx", "prettier", "--log-level", "warn", "--write", "."],
                    )
                    .check(&["npx", "prettier", "--log-level", "warn", "--check", "."])
                    .when(|d| d.has_prettier()),
                    CommandDef::new("ESLint --fix", &["npx", "eslint", "--fix", "."])
                        .when(|d| d.has_eslint()),
                ],
                build: vec![
                    CommandDef::new("Build", &[npm, "run", "build"])
                        .when(|d| d.has_script("build"))
                        .heavy(),
                    CommandDef::new("TypeScript", &["npx", "-y", "tsc", "--noEmit"])
                        .when(|d| d.has_typescript()),
                ],
                test: vec![CommandDef::new("Tests", &[npm, "test"])
                    .when(|d| d.has_script("test"))
                    .heavy()],
            },
        }
    }

    fn python_project(&self) -> ProjectType {
        ProjectType {
            name: "Python".into(),
            id: "python".into(),
            detect_files: &[
                "pyproject.toml",
                "requirements.txt",
                ".python-version",
                "setup.py",
            ],
            commands: Commands {
                lint: vec![
                    CommandDef::new("Ruff", &["uv", "run", "ruff", "check", "."])
                        .fix(&["uv", "run", "ruff", "check", "--fix", "."])
                        .when(py_ruff),
                    CommandDef::new("Mypy", &["uv", "run", "mypy", "."]).when(|d| {
                        d.exists("mypy.ini")
                            || d.exists(".mypy.ini")
                            || (d.exists("pyproject.toml")
                                && d.read("pyproject.toml")
                                    .unwrap_or_default()
                                    .contains("[tool.mypy]"))
                    }),
                    CommandDef::new(
                        "Format Check",
                        &["uv", "run", "ruff", "format", "--check", "."],
                    )
                    .fix(&["uv", "run", "ruff", "format", "."])
                    .when(py_ruff),
                    CommandDef::new("Pylint", &["uv", "run", "pylint", "."])
                        .when(|d| d.exists("pylintrc") || d.exists(".pylintrc"))
                        .heavy(),
                    CommandDef::new("Security Audit", &["uv", "run", "pip-audit"])
                        .when(|d| d.exists("pyproject.toml") || d.exists("requirements.txt"))
                        .heavy(),
                ],
                fmt: vec![
                    CommandDef::new("Ruff format", &["uv", "run", "ruff", "format", "."])
                        .check(&["uv", "run", "ruff", "format", "--check", "."])
                        .when(py_ruff),
                    CommandDef::new("Black", &["uv", "run", "black", "."])
                        .check(&["uv", "run", "black", "--check", "."])
                        .when(|d| d.exists("pyproject.toml")),
                ],
                build: vec![CommandDef::new("Build", &["uv", "build", "."])
                    .when(|d| d.exists("pyproject.toml"))
                    .heavy()],
                test: vec![CommandDef::new("Pytest", &["uv", "run", "pytest", "-q"])
                    .when(|d| {
                        d.exists("pyproject.toml")
                            || d.exists("pytest.ini")
                            || d.exists("setup.cfg")
                    })
                    .heavy()],
            },
        }
    }

    fn rust_project(&self) -> ProjectType {
        ProjectType {
            name: "Rust".into(),
            id: "rust".into(),
            detect_files: &["Cargo.toml"],
            commands: Commands {
                lint: vec![
                    CommandDef::new(
                        "Clippy",
                        &["cargo", "clippy", "--all-targets", "--", "-D", "warnings"],
                    ),
                    CommandDef::new("Security Audit", &["cargo", "audit"])
                        .when(|d| d.exists("Cargo.lock"))
                        .heavy(),
                    CommandDef::new("Format Check", &["cargo", "fmt", "--", "--check"])
                        .fix(&["cargo", "fmt"]),
                ],
                fmt: vec![CommandDef::new("Format", &["cargo", "fmt"])
                    .check(&["cargo", "fmt", "--", "--check"])],
                build: vec![
                    CommandDef::new("Build", &["cargo", "build"]).heavy(),
                    CommandDef::new("Check (faster)", &["cargo", "check"]),
                ],
                test: vec![CommandDef::new("Tests", &["cargo", "test"]).heavy()],
            },
        }
    }

    fn go_project(&self) -> ProjectType {
        ProjectType {
            name: "Go".into(),
            id: "go".into(),
            detect_files: &["go.mod"],
            commands: Commands {
                lint: vec![
                    CommandDef::new("Vet", &["go", "vet", "./..."]),
                    CommandDef::new(
                        "Security Audit",
                        &[
                            "go",
                            "run",
                            "golang.org/x/vuln/cmd/govulncheck@latest",
                            "./...",
                        ],
                    )
                    .when(|d| d.exists("go.mod"))
                    .heavy(),
                    CommandDef::new("golangci-lint", &["golangci-lint", "run"])
                        .when(|d| {
                            d.exists(".golangci.yml")
                                || d.exists(".golangci.yaml")
                                || d.exists(".golangci.toml")
                                || d.exists(".golangci.json")
                        })
                        .heavy(),
                    CommandDef::new(
                        "Staticcheck",
                        &[
                            "go",
                            "run",
                            "honnef.co/go/tools/cmd/staticcheck@latest",
                            "./...",
                        ],
                    )
                    .when(|d| d.exists("go.mod")),
                ],
                fmt: vec![
                    CommandDef::new("Format", &["gofmt", "-w", "."]).check(&["gofmt", "-l", "."])
                ],
                build: vec![CommandDef::new("Build", &["go", "build", "./..."]).heavy()],
                test: vec![CommandDef::new("Tests", &["go", "test", "./..."]).heavy()],
            },
        }
    }

    fn jvm_project(&self) -> ProjectType {
        let is_maven = self.exists("pom.xml");
        let build_cmd = if is_maven {
            vec!["mvn", "compile"]
        } else {
            vec!["./gradlew", "build"]
        };
        let test_cmd = if is_maven {
            vec!["mvn", "test"]
        } else {
            vec!["./gradlew", "test"]
        };
        ProjectType {
            name: "Java/Kotlin".into(),
            id: "jvm".into(),
            detect_files: &[
                "build.gradle",
                "build.gradle.kts",
                "pom.xml",
                "settings.gradle",
            ],
            commands: Commands {
                lint: vec![
                    CommandDef::new("Checkstyle", &["./gradlew", "checkstyleMain"])
                        .when(|d| d.exists("build.gradle") || d.exists("build.gradle.kts")),
                    CommandDef::new("Detekt", &["./gradlew", "detekt"])
                        .when(|d| d.exists("detekt.yml") || d.exists(".detekt.yml")),
                    CommandDef::new("Format Check", &["./gradlew", "spotlessCheck"])
                        .fix(&["./gradlew", "spotlessApply"])
                        .when(|d| d.has_spotless()),
                ],
                fmt: vec![
                    CommandDef::new("Spotless apply", &["./gradlew", "spotlessApply"])
                        .check(&["./gradlew", "spotlessCheck"])
                        .when(|d| d.exists("build.gradle") || d.exists("build.gradle.kts")),
                ],
                build: vec![CommandDef::new("Build", &build_cmd).heavy()],
                test: vec![CommandDef::new("Tests", &test_cmd).heavy()],
            },
        }
    }

    pub fn universal_commands(&self) -> Commands {
        let npm = self.node_pm_or("npm");
        Commands {
            lint: vec![
                CommandDef::new(
                    "Semgrep",
                    &["npx", "-y", "semgrep", "scan", "--config", "auto"],
                )
                .heavy(),
                CommandDef::new("Knip", &["npx", "knip"])
                    .when(|d| {
                        d.exists("knip.json")
                            || d.exists("knip.jsonc")
                            || d.exists("knip.config.js")
                            || d.exists("knip.config.ts")
                            || (d.exists("package.json")
                                && d.read("package.json")
                                    .unwrap_or_default()
                                    .contains("\"knip\""))
                    })
                    .heavy(),
                CommandDef::new(
                    "Secrets (Gitleaks)",
                    &["gitleaks", "detect", "--verbose", "--redact", "--no-git"],
                )
                .when(has_gitleaks)
                .heavy(),
                CommandDef::new(
                    "Security (Trivy)",
                    &["trivy", "fs", ".", "--severity", "HIGH,CRITICAL"],
                )
                .when(has_trivy)
                .heavy(),
                CommandDef::new(
                    "Secrets (Trufflehog)",
                    &["trufflehog", "filesystem", ".", "--only-verified"],
                )
                .when(has_trufflehog)
                .heavy(),
                CommandDef::new(
                    "DESIGN.md",
                    &["npx", "-y", "@google/design.md", "lint", "DESIGN.md"],
                )
                .when(|d| d.exists("DESIGN.md")),
                CommandDef::new(
                    "Shell",
                    &["npx", "-y", "shellcheck", "scripts/*.sh", "bin/*.sh"],
                )
                .when(|d| {
                    d.has_files_with_ext("scripts", ".sh") || d.has_files_with_ext("bin", ".sh")
                }),
                CommandDef::new(
                    "Markdown",
                    &["npx", "-y", "markdownlint-cli2", "**/*.md", "#node_modules"],
                )
                .when(|d| {
                    d.exists(".markdownlint.json")
                        || d.exists(".markdownlint.yaml")
                        || d.exists(".markdownlint.yml")
                        || d.exists(".markdownlint-cli2.jsonc")
                }),
                CommandDef::new("Format Check", &["npx", "prettier", "--check", "."])
                    .fix(&["npx", "prettier", "--write", "."])
                    .when(|d| {
                        d.has_prettier()
                            && !d.detect_project_types().iter().any(|p| p.id == "nodejs")
                    }),
            ],
            fmt: vec![
                CommandDef::new(
                    "Prettier",
                    &["npx", "prettier", "--log-level", "warn", "--write", "."],
                )
                .check(&["npx", "prettier", "--log-level", "warn", "--check", "."])
                .when(|d| d.has_prettier()),
                CommandDef::new("Dprint", &["npx", "-y", "dprint", "fmt"])
                    .check(&["npx", "-y", "dprint", "check"])
                    .when(|d| d.exists("dprint.json") || d.exists("dprint.jsonc")),
            ],
            build: vec![],
            test: vec![CommandDef::new("Tests", &[npm.as_str(), "test"])
                .when(|d| {
                    d.has_script("test")
                        && !d.detect_project_types().iter().any(|p| p.id == "nodejs")
                })
                .heavy()],
        }
    }

    pub fn get_applicable(&self, cmds: &[CommandDef]) -> Vec<CommandDef> {
        cmds.iter()
            .filter(|c| match c.only_if {
                Some(p) => p(self),
                None => true,
            })
            .cloned()
            .collect()
    }

    pub fn get_commands_by_type(&self, kind: Kind) -> Vec<CommandDef> {
        let detected = self.detect_project_types();
        let mut out: Vec<CommandDef> = self.get_applicable(self.universal_commands().get(kind));
        let multi = detected.len() > 1;
        for project in &detected {
            for cmd in self.get_applicable(project.commands.get(kind)) {
                let mut cmd = cmd;
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
            self.get_applicable(self.universal_commands().get(kind))
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

// ---- package managers ----

fn scan_entries(dir: &Path) -> HashSet<String> {
    match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => HashSet::new(),
    }
}

fn detect_package_manager(entries: &HashSet<String>) -> Option<&'static str> {
    if entries.contains("pnpm-lock.yaml") {
        Some("pnpm")
    } else if entries.contains("yarn.lock") {
        Some("yarn")
    } else if entries.contains("bun.lockb") {
        Some("bun")
    } else if entries.contains("package-lock.json") {
        Some("npm")
    } else {
        None
    }
}

fn detect_python_package_manager(
    entries: &HashSet<String>,
    pyproject: &Option<String>,
) -> Option<&'static str> {
    if entries.contains("uv.lock") {
        Some("uv")
    } else if entries.contains("requirements.txt") || entries.contains("requirements.lock") {
        Some("pip")
    } else if let Some(content) = pyproject {
        if content.contains("[tool.uv]") {
            Some("uv")
        } else {
            None
        }
    } else {
        None
    }
}

// ---- free predicates (coerce to fn pointers) ----

fn py_ruff(d: &Detector) -> bool {
    d.exists("pyproject.toml") || d.exists(".ruff.toml") || d.exists("ruff.toml")
}

fn has_gitleaks(_d: &Detector) -> bool {
    which::which("gitleaks").is_ok()
}
fn has_trivy(_d: &Detector) -> bool {
    which::which("trivy").is_ok()
}
fn has_trufflehog(_d: &Detector) -> bool {
    which::which("trufflehog").is_ok()
}

/// Mirrors hasPythonDependency: matches `^dep[=<>~]`, `"dep"`, or `'dep'`.
#[allow(dead_code)] // Phase 2: detectProjectTypeFlags
fn python_dep_match(content: &str, dep: &str) -> bool {
    let dq = format!("\"{dep}\"");
    let sq = format!("'{dep}'");
    if content.contains(&dq) || content.contains(&sq) {
        return true;
    }
    content.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with(dep) && t[dep.len()..].starts_with(['=', '<', '>', '~'])
    })
}

// ---- CEL config-driven detection ----

/// A `repo.toml` `[[detect]]` rule with its CEL expression compiled once at
/// load. Evaluation against a project snapshot is cheap and repeated.
pub struct CompiledRule {
    pub id: String,
    pub name: String,
    program: Program,
}

impl CompiledRule {
    fn eval_bool(&self, ctx: &Context) -> bool {
        matches!(self.program.execute(ctx), Ok(Value::Bool(true)))
    }
}

#[derive(Deserialize)]
struct DetectConfig {
    #[serde(default)]
    detect: Vec<DetectRaw>,
}

#[derive(Deserialize)]
struct DetectRaw {
    id: String,
    #[serde(default)]
    name: Option<String>,
    expr: String,
}

/// Load and compile `repo.toml` `[[detect]]` rules. Returns empty when there is
/// no `repo.toml`. Parse/compile errors fail loud with the offending rule id.
fn load_rules(cwd: &Path) -> anyhow::Result<Vec<CompiledRule>> {
    let path = cwd.join("repo.toml");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let cfg: DetectConfig =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    cfg.detect
        .into_iter()
        .map(|r| {
            let program = Program::compile(&r.expr)
                .map_err(|e| anyhow::anyhow!("repo.toml detect[{}]: {}", r.id, e))?;
            Ok(CompiledRule {
                name: r.name.unwrap_or_else(|| r.id.clone()),
                id: r.id,
                program,
            })
        })
        .collect()
}

/// Build a CEL evaluation context over the project snapshot. Host functions:
/// `file(name)` — entry exists at root; `has_ext(dir, ext)` — `dir` holds a file
/// ending in `ext`; `contains(file, needle)` — `file` exists and contains
/// `needle`. Closures capture owned `Arc` snapshots so the context is `'static`.
fn cel_context(cwd: &Path, entries: &HashSet<String>) -> Context<'static> {
    let files = Arc::new(entries.clone());
    let root = Arc::new(cwd.to_path_buf());
    let mut ctx = Context::default();
    let f = files.clone();
    ctx.add_function("file", move |name: Arc<String>| -> bool {
        f.contains(name.as_str())
    });
    let (f2, root2) = (files.clone(), root.clone());
    ctx.add_function(
        "contains",
        move |name: Arc<String>, needle: Arc<String>| -> bool {
            f2.contains(name.as_str())
                && fs::read_to_string(root2.join(name.as_str()))
                    .is_ok_and(|c| c.contains(needle.as_str()))
        },
    );
    let root3 = root.clone();
    ctx.add_function(
        "has_ext",
        move |dir: Arc<String>, ext: Arc<String>| -> bool {
            dir_has_ext(&root3.join(dir.as_str()), ext.as_str())
        },
    );
    ctx
}

fn dir_has_ext(dir: &Path, ext: &str) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    #[test]
    fn cel_host_functions() {
        let dir = cwd();
        let entries = scan_entries(&dir);
        let ctx = cel_context(&dir, &entries);
        let eval = |src: &str| -> bool {
            matches!(
                Program::compile(src).unwrap().execute(&ctx),
                Ok(Value::Bool(true))
            )
        };
        assert!(eval("file('Cargo.toml')"));
        assert!(!eval("file('nope.xyz')"));
        assert!(eval("file('Cargo.toml') && !file('package.json')"));
        assert!(eval("file('Cargo.toml') || file('nope.xyz')"));
        assert!(eval("has_ext('src', '.rs')"));
        assert!(!eval("has_ext('src', '.py')"));
        assert!(eval("contains('Cargo.toml', '[package]')"));
        assert!(!eval("contains('Cargo.toml', 'zz-not-present')"));
    }

    #[test]
    fn load_rules_without_config_is_empty() {
        if cwd().join("repo.toml").exists() {
            return; // a local config would change this; skip then
        }
        assert!(load_rules(&cwd()).unwrap().is_empty());
    }
}
