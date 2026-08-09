//! Ranked code search (Phase 3).
//!
//! Inspired by [shebe](https://gitlab.com/shebe-oss/shebe) (BM25 ranking) and
//! [minni](https://codeberg.org/drangus/minni) (code-aware tokenization), built
//! natively on Tantivy. Public surface: [`index::build`] / [`index::is_stale`]
//! / [`index::search`].
//!
//! Pipeline: walk source files → line-chunk → expand identifiers into sub-tokens
//! → Tantivy BM25 index → ranked query with lang/path post-filters.
//!
//! Phase 5 extends this with a 3-stage hybrid retrieval (BM25 ∪ dense → cross-encoder rerank)
//! via local llama.cpp. Integrated into the search flow: when semantic is enabled
//! (`semantic.enabled: true` in repo.yaml), the search command uses the hybrid search.
//! Use `--bm25` to force pure BM25 regardless of config.

pub mod chunker;
pub mod index;
pub mod semantic;
pub mod tokenizer;
