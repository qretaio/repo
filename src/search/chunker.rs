//! Line-based chunking with overlap.
//!
//! A chunk is a window of consecutive source lines. Overlap guarantees that a
//! symbol spanning the window boundary is still indexed in full in one chunk.
//! Shebe chunks by characters; minni by tree-sitter symbols. We chunk by lines
//! — no tree-sitter dependency, trivially correct, good enough for ranked
//! retrieval on the codebase sizes `repo` targets.

/// Lines per chunk (inclusive of any overlap carried from the previous chunk).
pub const CHUNK_LINES: usize = 64;
/// Lines shared with the previous chunk, so the step between windows is
/// `CHUNK_LINES - OVERLAP_LINES`.
pub const OVERLAP_LINES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-indexed first line.
    pub start: usize,
    /// 1-indexed last line.
    pub end: usize,
    pub text: String,
}

/// Split `text` into overlapping line windows. Lines are 1-indexed in output.
/// Empty input yields no chunks; a single short file yields one chunk.
pub fn chunks(text: &str) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline produces a spurious empty last element; drop it so the
    // reported line range matches what an editor shows.
    let n = if text.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    };
    if n == 0 {
        return Vec::new();
    }

    let step = CHUNK_LINES.saturating_sub(OVERLAP_LINES).max(1);
    let mut out = Vec::new();
    let mut start_idx = 0usize; // 0-indexed into `lines`
    while start_idx < n {
        let end_idx = (start_idx + CHUNK_LINES).min(n) - 1; // inclusive, 0-indexed
        let body: String = lines[start_idx..=end_idx].join("\n");
        out.push(Chunk {
            start: start_idx + 1,
            end: end_idx + 1,
            text: body,
        });
        if end_idx + 1 >= n {
            break;
        }
        start_idx += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yields_nothing() {
        assert!(chunks("").is_empty());
    }

    #[test]
    fn trailing_newline_does_not_inflate_range() {
        let c = chunks("a\nb\n").into_iter().next().unwrap();
        assert_eq!(c.start, 1);
        assert_eq!(c.end, 2);
    }

    #[test]
    fn small_file_is_one_chunk() {
        let body = "x\ny\nz";
        let out = chunks(body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, 1);
        assert_eq!(out[0].end, 3);
        assert_eq!(out[0].text, "x\ny\nz");
    }

    #[test]
    fn many_lines_chunk_with_overlap() {
        let n = CHUNK_LINES * 3;
        let body: String = (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunks(&body);
        let step = CHUNK_LINES - OVERLAP_LINES;
        // Each window starts `step` after the previous; the last window's tail
        // reaches the final line.
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
