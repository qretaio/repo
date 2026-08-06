//! Ranked code search (Phase 3).
//!
//! Inspired by [shebe](https://gitlab.com/shebe-oss/shebe) (BM25 ranking) and
//! [minni](https://codeberg.org/drangus/minni) (code-aware tokenization), built
//! natively on Tantivy. Public surface: [`index::build`] / [`index::is_stale`]
//! / [`index::search`].
//!
//! Pipeline: walk source files → line-chunk → expand identifiers into sub-tokens
//! → Tantivy BM25 index → ranked query with lang/path post-filters.

pub mod chunker;
pub mod index;
pub mod symbols;
pub mod tokenizer;
