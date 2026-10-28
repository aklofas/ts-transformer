//! Cross-binding integration test harness for ts-transformer.
//!
//! This crate is the cross-binding scenario harness. It houses:
//!
//! - `scenarios/` — the scenario trait + pilot scenario implementations.
//! - `scenarios/golden.rs` — the golden envelope + `CoreEvent` types.
//! - `scenarios/url.rs` — the URL corpus's record and loader, shared by the
//!   Rust leg and the C ABI leg.
//! - `bin/gen_scenarios.rs` — generator binary (default mode: write fixtures;
//!   `--check` mode: diff against committed).
//! - `tests/net_rust_scenarios.rs` — Rust adapter integration tests.
//!
//! # C and Python adapters
//!
//! The C and Python adapters live in their bindings and read the fixtures
//! from this crate: the scenarios through
//! `bindings/c/examples/scenarios/run_scenarios.c` and
//! `bindings/python/tests/test_scenarios.py`, the URL corpus through
//! `bindings/c/tests/net_url_corpus.rs` and
//! `bindings/python/tests/test_url_corpus.py`. The `c` and `python` cargo
//! features gate no code.
//!
//! # Synthetic data only
//!
//! None of the generators in this crate read from `testfiles/`, any `local/`
//! directory, or any real corpus.

pub mod scenarios;
