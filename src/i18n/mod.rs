//! Multilingual search-query expansion.
//!
//! The goal is that a user searching for "肖邦", "蕭邦", "xiaobang", or
//! "Chopin" all reach the same composer. We do this with a small curated
//! alias dictionary (`assets/zh_aliases.csv`) compiled into the binary at
//! build time. Each row carries the same name in English, Simplified
//! Chinese, Traditional Chinese, and Hanyu Pinyin.
//!
//! At search time we tokenize the input on whitespace + punctuation,
//! look each token up in a script-agnostic index, and emit one query
//! variant per script (4 max) where every matched token is rewritten
//! to that script. Unmatched tokens pass through unchanged.
//!
//! Known limitations (intentional for the MVP):
//! - No CJK sub-token scanning: "肖邦夜曲" (no space) is treated as a
//!   single unknown token. Adding jieba-rs later closes this.
//! - No phonetic Pinyin → Hanzi for unknown words. Only the curated
//!   alias names round-trip across scripts.

pub mod alias;

pub use alias::expand_query;
