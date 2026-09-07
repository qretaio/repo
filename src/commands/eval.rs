//! Eval command — golden-query retrieval evaluation.
//!
//! A spec file (`eval.yaml` by default) lists queries with graded expected
//! results. Each query runs in every requested mode (BM25 and/or the hybrid
//! semantic pipeline), and the command reports recall@k, MRR, and nDCG@k plus
//! latency percentiles per mode. Full reports persist under
//! `~/.cache/repo/evals/` and a summary event lands in the metrics log, so
//! `repo metrics` shows the trend across runs — the zg-style paired
//! comparison, local and repeatable.

use clap::Args;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::detect::Detector;
use crate::observe;
use crate::search::{self, index::Hit};
use crate::Globals;

#[derive(Args)]
pub struct EvalArgs {
    /// Path to the eval spec (default: ./eval.yaml).
    #[arg(long)]
    pub spec: Option<PathBuf>,
    /// Top-k cutoff for recall@k / nDCG@k (overrides the spec's `k`).
    #[arg(long)]
    pub k: Option<usize>,
    /// Write the full report JSON to stdout instead of a summary table.
    #[arg(long)]
    pub json: bool,
    /// Write a commented starter eval.yaml in the current directory.
    #[arg(long)]
    pub init: bool,
}

// ---------------------------------------------------------------------------
// spec
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EvalSpec {
    /// Top-k cutoff (default 10; `--k` wins).
    pub k: Option<usize>,
    /// Modes to run: any of `bm25`, `hybrid` (default: both).
    pub modes: Option<Vec<String>>,
    pub queries: Vec<QuerySpec>,
}

#[derive(Debug, Deserialize)]
pub struct QuerySpec {
    pub q: String,
    pub relevant: Vec<RelevantSpec>,
}

#[derive(Debug, Deserialize)]
pub struct RelevantSpec {
    /// File a good result must surface.
    pub path: String,
    /// Optional line range the hit must overlap.
    pub lines: Option<(usize, usize)>,
    /// Relevance weight for nDCG (default 1).
    pub grade: Option<f32>,
}

impl RelevantSpec {
    fn grade(&self) -> f32 {
        self.grade.unwrap_or(1.0)
    }
}

const TEMPLATE: &str = r#"# Golden-query eval spec for `repo eval` — edit, then run `repo eval`.
# relevance = the files (and optionally line ranges) a good result must surface.
k: 10
modes: [bm25, hybrid]   # hybrid needs the local embed/rerank servers
queries:
  - q: where do we validate login credentials
    relevant:
      - path: src/auth.rs
        # lines: [10, 40]   # hit must overlap this range
        # grade: 3          # nDCG weight (default 1)
"#;

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

/// Fraction of expected items matched by the top-k hits.
pub fn recall_at_k(matched: usize, expected: usize) -> f32 {
    if expected == 0 {
        1.0
    } else {
        matched as f32 / expected as f32
    }
}

/// 1/rank of the first relevant hit (0.0 when none of the top-k is relevant).
pub fn mrr(relevance: &[f32]) -> f32 {
    relevance
        .iter()
        .position(|&g| g > 0.0)
        .map_or(0.0, |i| 1.0 / (i as f32 + 1.0))
}

/// nDCG@k with exponential gain (2^g − 1); 1.0 when there is nothing ideal to
/// find (an empty expectation is trivially satisfied).
pub fn ndcg_at_k(relevance: &[f32], ideal: &[f32], k: usize) -> f32 {
    let dcg = gain(relevance, k);
    let idcg = gain(ideal, k);
    if idcg == 0.0 {
        1.0
    } else {
        dcg / idcg
    }
}

fn gain(grades: &[f32], k: usize) -> f32 {
    grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &g)| (2f32.powf(g) - 1.0) / ((i + 2) as f32).log2())
        .sum()
}

// ---------------------------------------------------------------------------
// scoring
// ---------------------------------------------------------------------------

/// Per-query scores.
#[derive(Debug, Clone, Serialize)]
pub struct QueryScore {
    pub recall: f32,
    pub mrr: f32,
    pub ndcg: f32,
    pub matched: usize,
    pub expected: usize,
}

/// Score `hits` (ranked) against the expected items. A hit matches an
/// unconsumed expectation when the path equals and — when the expectation
/// pins lines — the hit's range overlaps it. Each expectation is consumed
/// once, so repeated hits of the same file can't inflate the score.
pub fn score_query(hits: &[Hit], expected: &[RelevantSpec], k: usize) -> QueryScore {
    let mut consumed = vec![false; expected.len()];
    let mut relevance: Vec<f32> = Vec::new();
    let mut matched = 0usize;
    for hit in hits.iter().take(k) {
        let mut grade = 0.0;
        for (i, exp) in expected.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            let overlaps = exp
                .lines
                .is_none_or(|(lo, hi)| (hit.start as usize) <= hi && lo <= (hit.end as usize));
            if hit.path == exp.path && overlaps {
                consumed[i] = true;
                grade = exp.grade();
                matched += 1;
                break;
            }
        }
        relevance.push(grade);
    }
    let mut ideal: Vec<f32> = expected.iter().map(|e| e.grade()).collect();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    QueryScore {
        recall: recall_at_k(matched, expected.len()),
        mrr: mrr(&relevance),
        ndcg: ndcg_at_k(&relevance, &ideal, k),
        matched,
        expected: expected.len(),
    }
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct EvalReport {
    root: String,
    k: usize,
    ran_at: String,
    modes: BTreeMap<String, ModeReport>,
}

#[derive(Serialize)]
struct ModeReport {
    recall: f32,
    mrr: f32,
    ndcg: f32,
    p50_ms: u64,
    p95_ms: u64,
    errors: usize,
    per_query: Vec<QueryLine>,
}

#[derive(Serialize)]
struct QueryLine {
    q: String,
    recall: f32,
    mrr: f32,
    ndcg: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// runner
// ---------------------------------------------------------------------------

pub fn run(_d: &Detector, _g: &Globals, args: &EvalArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    if args.init {
        let path = PathBuf::from("eval.yaml");
        if path.exists() {
            eprintln!("{}", "eval.yaml already exists — not overwriting.".yellow());
            return 1;
        }
        if let Err(e) = std::fs::write(&path, TEMPLATE) {
            eprintln!("{}", format!("Error writing template: {e}").red());
            return 1;
        }
        println!(
            "{}",
            "Wrote eval.yaml — fill in your queries and run `repo eval`.".green()
        );
        return 0;
    }

    let spec_path = args
        .spec
        .clone()
        .unwrap_or_else(|| PathBuf::from("eval.yaml"));
    let spec: EvalSpec = match std::fs::read_to_string(&spec_path)
        .map_err(anyhow::Error::from)
        .and_then(|c| serde_yaml::from_str(&c).map_err(anyhow::Error::from))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "{}",
                format!("Error reading {}: {e}", spec_path.display()).red()
            );
            return 1;
        }
    };
    if spec.queries.is_empty() {
        eprintln!("{}", "Spec has no queries.".red());
        return 1;
    }
    let k = args.k.or(spec.k).unwrap_or(10);

    let modes = match resolve_modes(spec.modes.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    let settings = match search::semantic::settings_from_config(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("Error loading semantic config: {e}").red());
            return 1;
        }
    };

    // Ensure the index exists/current once, like `repo search` does.
    if search::index::is_stale(&root)
        || !search::index::index_dir(&root).is_some_and(|d| d.join("meta.json").exists())
    {
        eprintln!("{}", "Updating index…".cyan());
        if let Err(e) = search::index::build(&root, false) {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    }

    let mut report = EvalReport {
        root: root.display().to_string(),
        k,
        ran_at: String::new(),
        modes: BTreeMap::new(),
    };
    let mut total_runs = 0usize;
    let mut total_errors = 0usize;

    for mode in &modes {
        let force_bm25 = mode == "bm25";
        let mut per_query: Vec<QueryLine> = Vec::new();
        let mut latencies: Vec<u64> = Vec::new();
        let mut errors = 0usize;

        if mode == "hybrid" {
            if let Err(e) = search::semantic::build(&root, &settings, false) {
                // Embeddings unavailable: every hybrid query would fail; skip
                // the mode rather than produce all-error rows.
                eprintln!("{}", format!("Skipping hybrid mode: {e}").yellow());
                continue;
            }
        }

        for q in &spec.queries {
            let outcome = search::run_query(
                search::QueryRequest {
                    root: &root,
                    query: &q.q,
                    limit: k,
                    lang: None,
                    path_filter: None,
                    force_bm25,
                    source: "eval",
                },
                &settings,
            );
            match outcome {
                Ok(outcome) => {
                    let s = score_query(&outcome.hits, &q.relevant, k);
                    latencies.push(outcome.trace.total_ms.unwrap_or(0));
                    per_query.push(QueryLine {
                        q: q.q.clone(),
                        recall: s.recall,
                        mrr: s.mrr,
                        ndcg: s.ndcg,
                        ms: outcome.trace.total_ms,
                        error: None,
                    });
                }
                Err(e) => {
                    errors += 1;
                    per_query.push(QueryLine {
                        q: q.q.clone(),
                        recall: 0.0,
                        mrr: 0.0,
                        ndcg: 0.0,
                        ms: None,
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }

        total_runs += per_query.len();
        total_errors += errors;
        let n = per_query.len().max(1) as f32;
        report.modes.insert(
            mode.clone(),
            ModeReport {
                recall: per_query.iter().map(|q| q.recall).sum::<f32>() / n,
                mrr: per_query.iter().map(|q| q.mrr).sum::<f32>() / n,
                ndcg: per_query.iter().map(|q| q.ndcg).sum::<f32>() / n,
                p50_ms: observe::pct(&sorted(&latencies), 50),
                p95_ms: observe::pct(&sorted(&latencies), 95),
                errors,
                per_query,
            },
        );
    }

    if report.modes.is_empty() || (total_errors == total_runs) {
        eprintln!(
            "{}",
            "Every eval query failed — see the errors above.".red()
        );
        return 1;
    }

    report.ran_at = crate::observe::now_iso();
    let report_json = serde_json::to_string_pretty(&report).unwrap_or_default();

    // Persist the full report; link it from a summary telemetry event.
    let mut report_path = None;
    if let Some(dir) = search_report_dir(&root) {
        let _ = std::fs::create_dir_all(&dir);
        let stamp: String = report
            .ran_at
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == 'T')
            .collect();
        let file = dir.join(format!("{stamp}-eval.json"));
        if std::fs::write(&file, &report_json).is_ok() {
            report_path = Some(file.display().to_string());
        }
    }
    let mut modes_summary = serde_json::Map::new();
    for (mode, m) in &report.modes {
        modes_summary.insert(
            mode.clone(),
            json!({"recall": m.recall, "mrr": m.mrr, "ndcg": m.ndcg, "p50_ms": m.p50_ms, "p95_ms": m.p95_ms, "errors": m.errors}),
        );
    }
    crate::observe::append_event(
        &root,
        "eval_run",
        json!({"k": k, "queries": spec.queries.len(), "modes": modes_summary, "report": report_path}),
    );

    if args.json {
        println!("{report_json}");
        return 0;
    }

    println!(
        "{}",
        format!(
            "Eval: {} queries, k={k} — {}",
            spec.queries.len(),
            report.ran_at
        )
        .bold()
    );
    println!(
        "{:<9} {:>10} {:>7} {:>9} {:>8} {:>8} {:>7}",
        "mode", "recall@k", "mrr", "ndcg@k", "p50", "p95", "errors"
    );
    for (mode, m) in &report.modes {
        println!(
            "{mode:<9} {:>10.2} {:>7.2} {:>9.2} {:>8} {:>8} {:>7}",
            m.recall,
            m.mrr,
            m.ndcg,
            fmt_ms(m.p50_ms),
            fmt_ms(m.p95_ms),
            m.errors,
        );
    }
    if let Some(path) = &report_path {
        println!("{}", format!("report: {path}").bright_black());
    }
    0
}

fn resolve_modes(raw: Option<&[String]>) -> anyhow::Result<Vec<String>> {
    let modes = raw
        .map(|m| m.to_vec())
        .unwrap_or_else(|| vec!["bm25".into(), "hybrid".into()]);
    if modes.is_empty() {
        anyhow::bail!("spec lists no modes");
    }
    for m in &modes {
        if m != "bm25" && m != "hybrid" {
            anyhow::bail!("unknown mode {m:?} (expected \"bm25\" and/or \"hybrid\")");
        }
    }
    Ok(modes)
}

fn sorted(v: &[u64]) -> Vec<u64> {
    let mut out = v.to_vec();
    out.sort_unstable();
    out
}

fn fmt_ms(ms: u64) -> String {
    if ms >= 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn search_report_dir(root: &Path) -> Option<PathBuf> {
    crate::observe::reports_dir(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::index::Hit;

    fn hit(path: &str, start: u64, end: u64) -> Hit {
        Hit {
            path: path.into(),
            start,
            end,
            lang: "rust".into(),
            score: 1.0,
            source: String::new(),
        }
    }

    fn spec(path: &str, lines: Option<(usize, usize)>, grade: f32) -> RelevantSpec {
        RelevantSpec {
            path: path.into(),
            lines,
            grade: Some(grade),
        }
    }

    #[test]
    fn recall_counts_matched_expectations() {
        assert!((recall_at_k(2, 4) - 0.5).abs() < 1e-6);
        assert_eq!(recall_at_k(0, 0), 1.0, "empty expectation is satisfied");
    }

    #[test]
    fn mrr_rewards_first_relevant_rank() {
        assert!((mrr(&[0.0, 0.0, 3.0]) - 1.0 / 3.0).abs() < 1e-6);
        assert!((mrr(&[2.0]) - 1.0).abs() < 1e-6);
        assert_eq!(mrr(&[0.0, 0.0]), 0.0);
    }

    #[test]
    fn ndcg_is_one_for_perfect_ranking() {
        let ideal = vec![3.0, 2.0, 0.0];
        assert!((ndcg_at_k(&ideal, &ideal, 10) - 1.0).abs() < 1e-6);
        // Relevant at rank 2 instead of rank 1 → discounted.
        assert!(ndcg_at_k(&[0.0, 3.0, 2.0], &ideal, 10) < 1.0);
        assert_eq!(
            ndcg_at_k(&[], &[], 10),
            1.0,
            "nothing expected → trivially ideal"
        );
    }

    #[test]
    fn scoring_matches_paths_and_lines_once() {
        let expected = vec![
            spec("src/a.rs", None, 2.0),
            spec("src/b.rs", Some((10, 20)), 1.0),
        ];
        let hits = vec![
            hit("src/b.rs", 12, 18), // overlaps lines 10-20 → grade 1
            hit("src/b.rs", 40, 50), // same file again, already consumed → 0
            hit("src/a.rs", 1, 5),   // path match → grade 2
        ];
        let s = score_query(&hits, &expected, 10);
        assert_eq!(s.matched, 2);
        assert_eq!(s.expected, 2);
        assert!((s.recall - 1.0).abs() < 1e-6);
        // b.rs@1 is relevant-but-not-ideal-first, a.rs@3 second → ndcg < 1.
        assert!(s.ndcg < 1.0);
        assert!((s.mrr - 1.0).abs() < 1e-6, "first hit is relevant");
    }

    #[test]
    fn scoring_is_cutoff_at_k() {
        let expected = vec![spec("far.rs", None, 1.0)];
        let hits: Vec<Hit> = (0..5)
            .map(|i| hit("noise.rs", i * 10, i * 10))
            .chain(std::iter::once(hit("far.rs", 0, 9)))
            .collect();
        let s = score_query(&hits, &expected, 5);
        assert_eq!(s.matched, 0, "relevant hit at rank 6 is beyond k=5");
        assert_eq!(s.recall, 0.0);
        assert_eq!(s.mrr, 0.0);
    }

    #[test]
    fn spec_parses_optional_fields() {
        let yaml = r#"
k: 5
modes: [bm25]
queries:
  - q: fusion
    relevant:
      - path: src/search/semantic.rs
        grade: 3
  - q: walker
    relevant:
      - path: src/search/discovery.rs
        lines: [1, 50]
"#;
        let spec: EvalSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.k, Some(5));
        assert_eq!(spec.queries.len(), 2);
        assert_eq!(spec.queries[0].relevant[0].grade, Some(3.0));
        assert_eq!(spec.queries[1].relevant[0].lines, Some((1, 50)));
        assert_eq!(spec.queries[0].relevant[0].grade(), 3.0);
        assert_eq!(spec.queries[1].relevant[0].grade(), 1.0);
    }
}
