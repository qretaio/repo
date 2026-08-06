//! Search command — ranked BM25 code search across the repository.

use clap::Args;
use colored::Colorize;
use serde_json::json;

use crate::detect::Detector;
use crate::search;
use crate::Globals;

#[derive(Args)]
pub struct SearchArgs {
    /// Search query: function names, API calls, error strings, …
    pub query: String,

    /// Maximum number of results to return.
    #[arg(short, long, default_value = "10")]
    pub limit: usize,

    /// Filter results by language (rust, python, go, typescript, …).
    #[arg(long)]
    pub lang: Option<String>,

    /// Filter results by file-path substring.
    #[arg(long)]
    pub path: Option<String>,

    /// Lines of context shown around each match (0 = matched line only).
    #[arg(long, default_value = "2")]
    pub context: usize,

    /// Output results as JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(_d: &Detector, _g: &Globals, args: &SearchArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    // Auto-build on first use; warn (do not block) when stale.
    let dir_exists = search::index::index_dir(&root)
        .map(|d| d.join("meta.json").exists())
        .unwrap_or(false);
    if !dir_exists {
        eprintln!("{}", "No index yet — building…".cyan());
        if let Err(e) = search::index::build(&root, true) {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    } else if search::index::is_stale(&root) {
        eprintln!(
            "{}",
            "Index is stale — run `repo index` to refresh.".yellow()
        );
    }

    let hits = match search::index::search(
        &root,
        &args.query,
        args.limit,
        args.lang.as_deref(),
        args.path.as_deref(),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            return 1;
        }
    };

    if hits.is_empty() {
        if !args.json {
            eprintln!("{}", format!("No results for {:?}.", args.query).yellow());
        }
        return 0;
    }

    if args.json {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                json!({
                    "path": h.path,
                    "start": h.start,
                    "end": h.end,
                    "lang": h.lang,
                    "score": h.score,
                    "source": h.source,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return 0;
    }

    let terms: Vec<String> = args
        .query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();

    println!(
        "{}",
        format!("Results for {:?} ({}):", args.query, hits.len()).bold()
    );
    for (i, h) in hits.iter().enumerate() {
        println!(
            "\n{}. {}  [{} {:.2}]",
            i + 1,
            format!("{}:{}-{}", h.path, h.start, h.end).cyan(),
            h.lang,
            h.score
        );
        print_matches(&h.source, h.start, &terms, args.context);
    }
    0
}

/// Print the lines of a chunk that contain any query term, plus `ctx` lines of
/// surrounding context. Falls back to the first few lines if nothing matches.
fn print_matches(chunk: &str, start_line: u64, terms: &[String], ctx: usize) {
    let lines: Vec<&str> = chunk.lines().collect();
    let mut matched: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            let low = lines[i].to_ascii_lowercase();
            terms.iter().any(|t| low.contains(t))
        })
        .collect();

    if matched.is_empty() {
        // No literal term hit (ranking may come from sub-word expansion):
        // show the head of the chunk as a preview.
        matched = (0..lines.len().min(3)).collect();
    }

    let mut printed: Vec<usize> = Vec::new();
    for &m in &matched {
        let lo = m.saturating_sub(ctx);
        let hi = (m + ctx).min(lines.len().saturating_sub(1));
        for i in lo..=hi {
            if !printed.contains(&i) {
                printed.push(i);
            }
        }
    }
    printed.sort_unstable();

    let mut prev: Option<usize> = None;
    for &i in &printed {
        if let Some(p) = prev {
            if i != p + 1 {
                println!("{}", "…".bright_black());
            }
        }
        let n = start_line + i as u64;
        println!("{:>5} │ {}", n, lines[i]);
        prev = Some(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_matches_does_not_panic_on_empty_chunk() {
        print_matches("", 1, &["foo".to_string()], 2);
    }
}
