//! Chunking: definition-aligned chunks where tree-sitter knows the language,
//! overlapping line windows everywhere else.
//!
//! Fixed line windows routinely cut a symbol in half and carry no structural
//! context (the lesson zvec-grep's per-extractor pipeline drives home). We
//! already parse definitions for the symbol store, so the chunker reuses
//! that: one chunk per top-level definition — methods ride along inside their
//! container — with gaps (imports, module docs) filled by line windows. Each
//! structural chunk carries a `breadcrumb` (`Type › method`) that gets
//! prepended to the indexed token stream so symbol paths rank.

use std::path::Path;

use crate::symbols::parse::parse_definitions;

/// Lines per chunk for line-windowed (fallback) regions.
pub const CHUNK_LINES: usize = 64;
/// Lines shared with the previous window, so the step between windows is
/// `CHUNK_LINES - OVERLAP_LINES`.
pub const OVERLAP_LINES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-indexed first line.
    pub start: usize,
    /// 1-indexed last line.
    pub end: usize,
    pub text: String,
    /// Symbol path for definition-aligned chunks (e.g. `LoginHandler`);
    /// `None` for line-windowed gaps and fallback chunks.
    pub breadcrumb: Option<String>,
}

/// Chunk `text` for the file at `path`: one chunk per top-level definition
/// for the five tree-sitter languages, line windows otherwise (and when
/// parsing yields nothing usable).
pub fn chunks_for(text: &str, path: &Path) -> Vec<Chunk> {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = effective_line_count(text, &lines);
    if n == 0 {
        return Vec::new();
    }

    // Top-level regions: definitions without a container (`parent` is only
    // set for methods, which their container's span already covers). Impl
    // blocks are recorded as defs named after the implemented type, so an
    // `impl Foo` region carries its methods with breadcrumb `Foo`.
    let mut regions: Vec<(usize, usize, String)> = parse_definitions(text, &path.to_string_lossy())
        .iter()
        .filter(|d| d.parent.is_none())
        .map(|d| (d.start_line as usize, d.end_line as usize, d.name.clone()))
        .collect();
    regions.sort_by_key(|r| (r.0, r.1));

    // Drop regions nested inside the previous one (fns in fns, mods without
    // spans, …) so chunk line ranges never overlap and sidecar chunk ids
    // stay unique.
    let mut kept: Vec<(usize, usize, String)> = Vec::new();
    for r in regions {
        if let Some(last) = kept.last() {
            if r.0 >= last.0 && r.1 <= last.1 {
                continue;
            }
        }
        kept.push(r);
    }
    if kept.is_empty() {
        return window_lines(&lines, 0, n - 1, None);
    }

    let mut out = Vec::new();
    let mut cursor = 0usize; // 0-indexed next uncovered line
    for (start, end, name) in kept {
        let s = start.min(n);
        let e = end.min(n);
        if s == 0 || e < s {
            continue;
        }
        // Gap before the region (imports, module docs) → anonymous windows,
        // clipped short of the region's own first line. Whitespace-only gaps
        // (blank separators between defs) produce no chunk at all.
        if cursor + 1 < s {
            append_nonblank_window(&mut out, &lines, cursor..s - 1, None);
        }
        out.extend(region_chunks(&lines, s - 1, e - 1, &name));
        cursor = e;
    }
    if cursor < n {
        append_nonblank_window(&mut out, &lines, cursor..n, None);
    }
    out
}

/// Emit line windows over `range` (0-indexed, end-exclusive) trimmed to its
/// non-blank extent; a range with no content produces nothing.
fn append_nonblank_window(
    out: &mut Vec<Chunk>,
    lines: &[&str],
    range: std::ops::Range<usize>,
    crumb: Option<&str>,
) {
    let hi = range.end.min(lines.len());
    let Some(lo) = (range.start..hi).find(|&i| !lines[i].trim().is_empty()) else {
        return;
    };
    let mut last = lo;
    for i in (lo..hi).rev() {
        if !lines[i].trim().is_empty() {
            last = i;
            break;
        }
    }
    out.extend(window_lines(lines, lo, last, crumb));
}

/// One chunk for a region that fits a window; overlapping line windows (each
/// still carrying the breadcrumb) for an oversized symbol.
fn region_chunks(lines: &[&str], lo: usize, hi: usize, crumb: &str) -> Vec<Chunk> {
    if hi - lo < CHUNK_LINES {
        return vec![Chunk {
            start: lo + 1,
            end: hi + 1,
            text: lines[lo..=hi].join("\n"),
            breadcrumb: Some(crumb.to_string()),
        }];
    }
    window_lines(lines, lo, hi, Some(crumb))
}

/// Overlapping [`CHUNK_LINES`]-line windows over `lines[lo..=hi]` (0-indexed,
/// inclusive). Windows are clipped to the region, so a symbol boundary is
/// never straddled by an anonymous window.
fn window_lines(lines: &[&str], lo: usize, hi: usize, crumb: Option<&str>) -> Vec<Chunk> {
    let step = CHUNK_LINES.saturating_sub(OVERLAP_LINES).max(1);
    let mut out = Vec::new();
    let mut s = lo;
    while s <= hi {
        let e = (s + CHUNK_LINES - 1).min(hi);
        out.push(Chunk {
            start: s + 1,
            end: e + 1,
            text: lines[s..=e].join("\n"),
            breadcrumb: crumb.map(str::to_string),
        });
        if e == hi {
            break;
        }
        s += step;
    }
    out
}

fn effective_line_count(text: &str, lines: &[&str]) -> usize {
    if text.is_empty() {
        return 0;
    }
    // A trailing newline produces a spurious empty last element; drop it so
    // the reported line range matches what an editor shows.
    if text.ends_with('\n') {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yields_nothing() {
        assert!(chunks_for("", Path::new("a.rs")).is_empty());
    }

    #[test]
    fn trailing_newline_does_not_inflate_range() {
        let c = chunks_for("a\nb\n", Path::new("x.sh"))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(c.start, 1);
        assert_eq!(c.end, 2);
    }

    #[test]
    fn small_file_is_one_chunk() {
        let body = "x\ny\nz";
        let out = chunks_for(body, Path::new("x.sh"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, 1);
        assert_eq!(out[0].end, 3);
        assert_eq!(out[0].text, "x\ny\nz");
        assert_eq!(out[0].breadcrumb, None);
    }

    #[test]
    fn rust_chunks_align_to_definitions_with_breadcrumbs() {
        let src = "//! Module docs.\n\
                   pub fn handle_login(user: &str) -> bool {\n\
                       let auth = authenticate(user);\n\
                       auth\n\
                   }\n\
                   \n\
                   pub struct Session { id: u32 }\n\
                   \n\
                   pub fn authenticate(token: &str) -> bool {\n\
                       token.len() > 3\n\
                   }\n";
        let out = chunks_for(src, Path::new("src/auth.rs"));
        let crumbs: Vec<Option<&str>> = out.iter().map(|c| c.breadcrumb.as_deref()).collect();
        // Two function regions + one struct region; the doc-comment gap
        // before the first fn is anonymous.
        assert!(crumbs.contains(&Some("handle_login")), "{crumbs:?}");
        assert!(crumbs.contains(&Some("Session")), "{crumbs:?}");
        assert!(crumbs.contains(&Some("authenticate")), "{crumbs:?}");
        assert_eq!(crumbs[0], None, "leading gap is an anonymous window");

        // Each definition's body lands whole inside its own chunk.
        let login = out
            .iter()
            .find(|c| c.breadcrumb.as_deref() == Some("handle_login"))
            .unwrap();
        assert!(login.text.contains("authenticate(user)"));
        let auth = out
            .iter()
            .find(|c| c.breadcrumb.as_deref() == Some("authenticate"))
            .unwrap();
        assert!(auth.text.contains("token.len() > 3"));
    }

    #[test]
    fn python_class_chunk_carries_its_methods() {
        let src = "class Fetcher:\n\
                   \n\
                   \x20   def get(self, url):\n\
                   \x20       return requests.get(url)\n\
                   \x20\n\
                   \x20   def post(self, url, data):\n\
                   \x20       return requests.post(url, data=data)\n\
                   \n\
                   def helper():\n\
                   \x20   pass\n";
        let out = chunks_for(src, Path::new("pkg/fetcher.py"));
        let fetcher = out
            .iter()
            .find(|c| c.breadcrumb.as_deref() == Some("Fetcher"))
            .expect("class region");
        assert!(fetcher.text.contains("requests.post"), "methods ride along");
        // Top-level helper is its own region, not swallowed by the class.
        assert!(out
            .iter()
            .any(|c| c.breadcrumb.as_deref() == Some("helper")));
    }

    #[test]
    fn oversized_symbol_splits_into_breadcrumb_windows() {
        let mut src = String::from("pub fn big() {\n");
        for i in 0..200 {
            src.push_str(&format!("    let v{i} = {i}; // filler line\n"));
        }
        src.push_str("}\n");
        let out = chunks_for(&src, Path::new("big.rs"));
        assert!(out.len() > 1, "200+ line fn must split");
        assert!(out.iter().all(|c| c.breadcrumb.as_deref() == Some("big")));
        assert_eq!(out.last().unwrap().end, 202);
    }

    #[test]
    fn unsupported_language_falls_back_to_line_windows() {
        let n = CHUNK_LINES * 3;
        let body: String = (0..n)
            .map(|i| format!("echo line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunks_for(&body, Path::new("script.sh"));
        let step = CHUNK_LINES - OVERLAP_LINES;
        assert_eq!(out[0].start, 1);
        assert_eq!(out[0].end, CHUNK_LINES);
        assert_eq!(out[1].start, 1 + step);
        assert!(out.iter().all(|c| c.breadcrumb.is_none()));
        assert_eq!(out.last().unwrap().end, n);
    }

    #[test]
    fn region_ranges_never_overlap() {
        let src = "fn a() {\n    fn inner() {}\n}\n\nfn b() {}\n";
        let out = chunks_for(src, Path::new("m.rs"));
        let mut spans: Vec<(usize, usize)> = out.iter().map(|c| (c.start, c.end)).collect();
        spans.sort();
        for pair in spans.windows(2) {
            assert!(pair[0].1 < pair[1].0, "spans overlap: {spans:?}");
        }
    }

    #[test]
    fn blank_line_gaps_produce_no_chunks() {
        let src = "pub fn a() {}\n\n\npub fn b() {}\n\n";
        let out = chunks_for(src, Path::new("m.rs"));
        assert_eq!(
            out.len(),
            2,
            "blank separators must not become chunks: {out:?}"
        );
        assert!(out.iter().all(|c| c.breadcrumb.is_some()));
    }

    #[test]
    fn many_lines_chunk_with_overlap() {
        let n = CHUNK_LINES * 3;
        let body: String = (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunks_for(&body, Path::new("script.sh"));
        let step = CHUNK_LINES - OVERLAP_LINES;
        assert_eq!(out[0].start, 1);
        assert_eq!(out[0].end, CHUNK_LINES);
        assert_eq!(out[1].start, 1 + step);
        assert_eq!(out[1].end, CHUNK_LINES + step);
        assert!(
            out.last().unwrap().end == n,
            "last window must reach line {n}"
        );
    }
}
