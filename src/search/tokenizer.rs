//! Code-aware token expansion.
//!
//! Tantivy's default `TEXT` field lowercases and splits on non-alphanumerics,
//! which is blind to identifier boundaries (`getUserName`, `HTTPServer`,
//! `handle_http_request`). Rather than implement Tantivy's version-sensitive
//! `Tokenizer` trait, we *pre-expand* each source chunk into a space-separated
//! stream of its identifiers plus their sub-words. The result is indexed with
//! the default tokenizer, so a query for `get user` matches `getUserName` and a
//! query for `httpserver` matches `HTTPServer`. Boring, version-proof, correct.

/// Expand source text into a space-separated stream of identifier sub-tokens.
///
/// Each maximal run of `[A-Za-z0-9]` is treated as an identifier. For every
/// identifier we emit the identifier itself, then each sub-word split out by
/// camelCase / PascalCase / digit boundaries (snake_case is handled implicitly
/// since `_` is an identifier boundary). All sub-words are lowercased.
pub fn expand(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in identifier_runs(text) {
        out.push_str(word);
        out.push(' ');
        let lower = word.to_ascii_lowercase();
        if lower != word {
            out.push_str(&lower);
            out.push(' ');
        }
        let subs = split_subwords(word);
        // Only emit sub-words when the identifier actually split into >1 part;
        // otherwise the identifier itself (already emitted) is enough.
        if subs.len() > 1 {
            for s in subs {
                out.push_str(&s.to_ascii_lowercase());
                out.push(' ');
            }
        }
    }
    out
}

/// Yield maximal runs of ASCII alphanumeric characters (the `_` separator is a
/// boundary, which is what gives us snake_case splitting for free).
fn identifier_runs(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            out.push(&text[s..i]);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

/// Split a single identifier on camelCase / acronym / digit boundaries.
///
/// Examples: `getUserName` → `[get, User, Name]`,
/// `HTTPServer` → `[HTTP, Server]`, `parse2Json` → `[parse, 2, Json]`.
fn split_subwords(w: &str) -> Vec<String> {
    let chars: Vec<char> = w.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..chars.len() {
        let prev = chars[i - 1];
        let cur = chars[i];
        let split = is_boundary(&chars, i) && !(prev.is_ascii_digit() && cur.is_ascii_digit());
        if split {
            out.push(chars[start..i].iter().collect());
            start = i;
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

/// True if a sub-word boundary sits between `chars[i-1]` and `chars[i]`.
fn is_boundary(chars: &[char], i: usize) -> bool {
    let prev = chars[i - 1];
    let cur = chars[i];
    // lower → Upper:  `tU` in getU
    if prev.is_ascii_lowercase() && cur.is_ascii_uppercase() {
        return true;
    }
    // Upper Upper lower: acronym tail, split before the last Upper (`HTTP`|`S`)
    if prev.is_ascii_uppercase()
        && cur.is_ascii_uppercase()
        && i + 1 < chars.len()
        && chars[i + 1].is_ascii_lowercase()
    {
        return true;
    }
    // letter ↔ digit
    if prev.is_ascii_alphabetic() && cur.is_ascii_digit()
        || prev.is_ascii_digit() && cur.is_ascii_alphabetic()
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_splits_on_underscore() {
        let e = expand("handle_http_request");
        assert!(e.contains("handle"));
        assert!(e.contains("http"));
        assert!(e.contains("request"));
    }

    #[test]
    fn camel_case_splits_on_case_boundary() {
        let e = expand("getUserName");
        assert!(e.contains("get"));
        assert!(e.contains("user"));
        assert!(e.contains("name"));
    }

    #[test]
    fn pascal_acronym_boundary() {
        let e = expand("HTTPServer");
        assert!(e.contains("http"), "got: {e}");
        assert!(e.contains("server"), "got: {e}");
    }

    #[test]
    fn digit_boundary_splits() {
        let e = expand("parse2Json");
        assert!(e.contains("parse"));
        assert!(e.contains("json"));
        assert!(e.contains("2"));
    }

    #[test]
    fn keeps_original_identifier() {
        // The raw identifier survives so exact-name queries still hit.
        let e = expand("struct Detector");
        assert!(e.contains("Detector"));
        assert!(e.contains("detector"));
    }

    #[test]
    fn non_alphanumeric_are_boundaries() {
        let e = expand("foo.bar(baz)");
        assert!(e.contains("foo"));
        assert!(e.contains("bar"));
        assert!(e.contains("baz"));
    }

    #[test]
    fn empty_input_yields_empty() {
        assert_eq!(expand(""), "");
        assert_eq!(expand("...!!!..."), "");
    }
}
