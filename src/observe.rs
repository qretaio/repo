//! Local, structured telemetry for `repo`.
//!
//! Every search, index build, and eval run appends one JSON line to a
//! per-repository event log under `~/.cache/repo/metrics/<key>.jsonl` (keyed
//! like the other caches, by canonical root). `repo metrics` aggregates the
//! log — stage timings, zero-result queries, rerank fallbacks, errors, build
//! history, eval trends — and `repo eval` links its full reports (under
//! `~/.cache/repo/evals/`) from an event.
//!
//! The log never leaves the machine. Disable entirely with
//! `observability.enabled: false` in repo.yaml; keep the file but record a
//! query hash instead of the raw text with `observability.log_queries: false`.
//! When the log passes 10 MB it is rotated to `<key>.jsonl.old` (one
//! generation kept) so it stays reviewable and bounded.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::search::index::fnv1a64;

/// Rotate the event log when it exceeds this size.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilitySettings {
    pub enabled: bool,
    pub log_queries: bool,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            log_queries: true,
        }
    }
}

#[derive(Deserialize, Default)]
struct ObservabilityRaw {
    enabled: Option<bool>,
    log_queries: Option<bool>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    observability: Option<ObservabilityRaw>,
}

/// Load observability settings: embedded defaults → global
/// (`~/.config/repo/repo.yaml`) → local (`./repo.yaml`), merged per-field.
pub fn settings_from_config(root: &Path) -> ObservabilitySettings {
    let mut settings = ObservabilitySettings::default();
    let apply = |settings: &mut ObservabilitySettings, raw: Option<ObservabilityRaw>| {
        let Some(raw) = raw else { return };
        if let Some(v) = raw.enabled {
            settings.enabled = v;
        }
        if let Some(v) = raw.log_queries {
            settings.log_queries = v;
        }
    };
    if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(crate::detect::DEFAULTS) {
        apply(&mut settings, cfg.observability);
    }
    if let Some(dir) = global_config_dir() {
        if let Ok(content) = std::fs::read_to_string(dir.join("repo.yaml")) {
            if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(&content) {
                apply(&mut settings, cfg.observability);
            }
        }
    }
    if let Ok(content) = std::fs::read_to_string(root.join("repo.yaml")) {
        if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(&content) {
            apply(&mut settings, cfg.observability);
        }
    }
    settings
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

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

fn repo_key(root: &Path) -> Option<String> {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Some(format!("{:016x}", fnv1a64(&canon.to_string_lossy())))
}

fn cache_dir(sub: &str, root: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("repo")
            .join(sub)
            .join(repo_key(root)?),
    )
}

/// The per-repository JSONL event log.
pub fn events_path(root: &Path) -> Option<PathBuf> {
    cache_dir("metrics", root).map(|d| d.with_extension("jsonl"))
}

/// Directory where `repo eval` persists full reports.
pub fn reports_dir(root: &Path) -> Option<PathBuf> {
    cache_dir("evals", root)
}

// ---------------------------------------------------------------------------
// recording
// ---------------------------------------------------------------------------

/// Stamp and append one event. No-op when observability is disabled in
/// config. `fields` becomes the event body minus the timestamp, kind, and
/// (optionally redacted) free-text fields handled here.
pub fn append_event(root: &Path, kind: &str, mut fields: serde_json::Value) {
    let settings = settings_from_config(root);
    if !settings.enabled {
        return;
    }
    let Some(path) = events_path(root) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // One rotation generation keeps the log bounded and reviewable.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let obj = fields.as_object_mut().expect("event body is a JSON object");
    obj.insert("ts".into(), json!(now));
    obj.insert("iso".into(), json!(iso_from_unix(now)));
    obj.insert("kind".into(), json!(kind));
    if let Some(query) = obj.remove("query") {
        // Query text is the most useful — and most sensitive — field; honor
        // the privacy switch by keeping only a stable hash.
        let kept = if settings.log_queries {
            query
        } else {
            json!(format!(
                "h{:016x}",
                fnv1a64(query.as_str().unwrap_or_default())
            ))
        };
        obj.insert("query".into(), kept);
    }
    let mut line = fields.to_string();
    line.push('\n');
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// Record an index-build (or embedding-build) outcome. Wrapped around
/// `index::build` / `semantic::build` so every caller reports uniformly.
pub fn record_build(
    root: &Path,
    store: &str,
    result: &anyhow::Result<crate::search::index::BuildStats>,
    elapsed: std::time::Duration,
) {
    let body = match result {
        Ok(stats) => json!({
            "store": store,
            "files": stats.files,
            "chunks": stats.chunks,
            "rebuilt": stats.rebuilt,
            "incremental": stats.incremental,
            "ms": elapsed.as_millis() as u64,
        }),
        Err(e) => json!({
            "store": store,
            "error": format!("{e:#}"),
            "ms": elapsed.as_millis() as u64,
        }),
    };
    append_event(root, "index_build", body);
}

/// Read this repository's events, oldest first. Bad lines are skipped;
/// a missing log yields an empty vec.
pub fn read_events(root: &Path) -> Vec<serde_json::Value> {
    let Some(path) = events_path(root) else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` (civil-from-days; no chrono dep).
fn iso_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Current UTC wall-clock time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_from_unix(secs)
}

/// Summarize a slice of parsed events (see [`read_events`]). Pure so tests can
/// feed synthetic events; `commands::metrics` renders it.
pub fn summarize(events: &[serde_json::Value]) -> Summary {
    let mut summary = Summary::default();
    for event in events {
        summary.events += 1;
        match event["kind"].as_str().unwrap_or("") {
            "search" => {
                summary.searches += 1;
                let mode = event["mode"].as_str().unwrap_or("unknown").to_string();
                let stats = summary.by_mode.entry(mode).or_default();
                stats.queries += 1;
                let ms = event["total_ms"].as_u64().unwrap_or(0);
                stats.latencies.push(ms);
                let failed = event["error"].is_string();
                if failed {
                    stats.errors += 1;
                }
                if !failed {
                    let results = event["results"].as_u64().unwrap_or(0);
                    let query = event["query"].as_str().unwrap_or("").to_string();
                    if results == 0 {
                        stats.zero_result.push(query.clone());
                    }
                    if event["fallback"].as_bool().unwrap_or(false) {
                        stats.fallbacks.push(query.clone());
                    }
                    stats.samples.push(Sample { query, ms, results });
                }
            }
            "index_build" => {
                summary.builds += 1;
                match event["store"].as_str().unwrap_or("") {
                    "semantic" => summary.last_semantic_build = Some(event.clone()),
                    _ => summary.last_bm25_build = Some(event.clone()),
                }
            }
            "eval_run" => summary.evals.push(event.clone()),
            _ => {}
        }
    }
    for stats in summary.by_mode.values_mut() {
        stats.latencies.sort_unstable();
        stats
            .samples
            .sort_by(|a, b| b.ms.cmp(&a.ms).then(b.results.cmp(&a.results)));
        stats.zero_result.sort();
        stats.zero_result.dedup();
        stats.fallbacks.sort();
        stats.fallbacks.dedup();
    }
    summary.evals.reverse(); // newest first
    summary
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ModeStats {
    pub queries: usize,
    pub errors: usize,
    #[serde(skip)]
    pub latencies: Vec<u64>,
    /// Slowest first, capped at 10.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Sample>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub zero_result: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
}

impl ModeStats {
    /// Nearest-rank percentile over recorded latencies.
    pub fn pct(&self, p: u32) -> u64 {
        pct(&self.latencies, p)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub query: String,
    pub ms: u64,
    pub results: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Summary {
    pub events: usize,
    pub searches: usize,
    pub builds: usize,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub by_mode: std::collections::BTreeMap<String, ModeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_bm25_build: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_semantic_build: Option<serde_json::Value>,
    /// Eval runs, newest first (summary only — full reports live on disk).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evals: Vec<serde_json::Value>,
}

/// Nearest-rank percentile of a sorted sample (0 for an empty sample): the
/// smallest value covering at least `p`% of the observations.
pub fn pct(sorted: &[u64], p: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() as u64 * u64::from(p)).div_ceil(100);
    sorted[(rank as usize).clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search_event(mode: &str, ms: u64, results: u64, fallback: bool) -> serde_json::Value {
        json!({
            "kind": "search", "mode": mode, "total_ms": ms, "results": results,
            "fallback": fallback, "query": format!("q{ms}"),
        })
    }

    #[test]
    fn summarize_groups_by_mode_and_flags_shortcomings() {
        let events = vec![
            search_event("bm25", 5, 10, false),
            search_event("bm25", 50, 0, false),
            search_event("semantic", 900, 3, false),
            search_event("semantic", 1200, 4, true),
            json!({"kind": "search", "mode": "semantic", "error": "rerank server returned HTTP 500", "total_ms": 10, "query": "boom"}),
            json!({"kind": "index_build", "store": "bm25", "files": 3, "chunks": 9, "rebuilt": true, "incremental": true, "ms": 12}),
            json!({"kind": "eval_run", "report": "x.json"}),
        ];
        let s = summarize(&events);
        assert_eq!(s.events, 7);
        assert_eq!(s.searches, 5);
        assert_eq!(s.builds, 1);

        let bm25 = &s.by_mode["bm25"];
        assert_eq!(bm25.queries, 2);
        assert_eq!(bm25.zero_result, vec!["q50".to_string()]);
        assert_eq!(bm25.pct(50), 5);
        assert_eq!(bm25.pct(95), 50);

        let sem = &s.by_mode["semantic"];
        assert_eq!(sem.queries, 3);
        assert_eq!(sem.errors, 1);
        assert_eq!(sem.fallbacks, vec!["q1200".to_string()]);
        assert_eq!(sem.pct(50), 900);

        assert_eq!(s.last_bm25_build.as_ref().unwrap()["chunks"], 9);
        assert_eq!(s.evals.len(), 1);
    }

    #[test]
    fn pct_handles_empty_and_single() {
        assert_eq!(pct(&[], 50), 0);
        assert_eq!(pct(&[7], 50), 7);
        assert_eq!(pct(&[1, 2, 3, 4], 95), 4);
    }

    #[test]
    fn iso_timestamps_are_utc() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso_from_unix(1_759_564_427), "2025-10-04T07:53:47Z");
    }
}
