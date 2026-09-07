//! Metrics command — aggregates the local JSONL telemetry log so retrieval
//! shortcomings are visible: slow stages, zero-result queries, rerank
//! fallbacks, errors, build history, and eval trends. The log itself lives
//! under `~/.cache/repo/metrics/` (see [`crate::observe`]).

use clap::Args;
use colored::Colorize;
use std::path::Path;

use crate::detect::Detector;
use crate::observe::{self, Summary};
use crate::Globals;

#[derive(Args)]
pub struct MetricsArgs {
    /// Print the aggregated summary as JSON.
    #[arg(long)]
    pub json: bool,
    /// Print the last N raw events instead of the summary.
    #[arg(long)]
    pub tail: Option<usize>,
}

pub fn run(_d: &Detector, _g: &Globals, args: &MetricsArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    let events = observe::read_events(&root);
    if events.is_empty() {
        println!(
            "{}",
            "No telemetry recorded yet for this repository — run some searches first.".yellow()
        );
        if let Some(path) = observe::events_path(&root) {
            println!("{}", format!("Log: {}", path.display()).bright_black());
        }
        return 0;
    }

    if let Some(n) = args.tail {
        for event in events.iter().rev().take(n).rev() {
            println!("{}", event);
        }
        return 0;
    }

    let summary = observe::summarize(&events);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        );
        return 0;
    }
    print_summary(&root, &summary);
    0
}

fn print_summary(root: &Path, s: &Summary) {
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    println!(
        "{}",
        format!("Telemetry for {root_name} — {} events", s.events).bold()
    );
    if let Some(path) = observe::events_path(root) {
        println!("{}", format!("Log: {}", path.display()).bright_black());
    }

    println!("\n{}", format!("Searches: {}", s.searches).bold());
    for (mode, stats) in &s.by_mode {
        println!(
            "  {mode:<9} {:>4} queries   p50 {}   p95 {}   {} zero-result   {} fallbacks   {} errors",
            stats.queries,
            fmt_ms(stats.pct(50)),
            fmt_ms(stats.pct(95)),
            stats.zero_result.len(),
            stats.fallbacks.len(),
            stats.errors,
        );
    }

    let mut slowest: Vec<(&str, &observe::Sample)> = s
        .by_mode
        .iter()
        .flat_map(|(mode, m)| m.samples.iter().map(move |s| (mode.as_str(), s)))
        .collect();
    slowest.sort_by_key(|(_, sample)| std::cmp::Reverse(sample.ms));
    slowest.truncate(5);
    if !slowest.is_empty() {
        println!("\n{}", "Slowest queries".bold());
        for (mode, sample) in slowest {
            println!("  {:>7}  {mode:<9}{}", fmt_ms(sample.ms), sample.query);
        }
    }

    print_shortcoming("Zero-result queries", s, |m| &m.zero_result);
    print_shortcoming(
        "Rerank fallbacks (reranker scored every candidate irrelevant)",
        s,
        |m| &m.fallbacks,
    );

    if s.builds > 0 {
        println!("\n{}", format!("Index builds: {}", s.builds).bold());
        for (label, event) in [
            ("bm25", &s.last_bm25_build),
            ("semantic", &s.last_semantic_build),
        ] {
            if let Some(e) = event {
                if e["error"].is_string() {
                    println!(
                        "  last {label}: {}",
                        format!("ERROR {}", e["error"].as_str().unwrap_or("")).red()
                    );
                } else {
                    let kind = if e["rebuilt"].as_bool().unwrap_or(false) {
                        if e["incremental"].as_bool().unwrap_or(false) {
                            "incremental"
                        } else {
                            "full"
                        }
                    } else {
                        "up to date"
                    };
                    println!(
                        "  last {label}: {kind}, {} files, {} chunks, {}",
                        e["files"],
                        e["chunks"],
                        fmt_ms(e["ms"].as_u64().unwrap_or(0)),
                    );
                }
            }
        }
    }

    if !s.evals.is_empty() {
        println!("\n{}", format!("Eval runs: {}", s.evals.len()).bold());
        if let Some(latest) = s.evals.first() {
            println!("  {}", latest["iso"].as_str().unwrap_or(""));
            for (mode, m) in latest["modes"]
                .as_object()
                .unwrap_or(&serde_json::Map::new())
            {
                println!(
                    "  {mode:<9} recall@{} {:.2}   mrr {:.2}   ndcg {:.2}   {} errors",
                    latest["k"],
                    m["recall"].as_f64().unwrap_or(0.0),
                    m["mrr"].as_f64().unwrap_or(0.0),
                    m["ndcg"].as_f64().unwrap_or(0.0),
                    m["errors"],
                );
            }
            if let Some(report) = latest["report"].as_str() {
                println!("  {}", format!("report: {report}").bright_black());
            }
        }
    }
}

fn print_shortcoming(title: &str, s: &Summary, pick: impl Fn(&observe::ModeStats) -> &[String]) {
    let any = s.by_mode.values().any(|m| !pick(m).is_empty());
    if !any {
        return;
    }
    println!("\n{}", title.bold());
    for (mode, stats) in &s.by_mode {
        let items = pick(stats);
        if items.is_empty() {
            continue;
        }
        println!("  {mode}: {}", items.join(", "));
    }
}

/// Milliseconds → compact human duration ("8ms", "1.4s").
fn fmt_ms(ms: u64) -> String {
    if ms >= 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ms_stays_compact() {
        assert_eq!(fmt_ms(8), "8ms");
        assert_eq!(fmt_ms(9_999), "9999ms");
        assert_eq!(fmt_ms(10_000), "10.0s");
    }

    #[test]
    fn summary_json_roundtrip() {
        // The Summary must serialize (the --json flag promise).
        let s = Summary::default();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["searches"], 0);
    }
}
