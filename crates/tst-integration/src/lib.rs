//! Cross-binding integration test harness for ts-transformer.
//!
//! This crate is the cross-binding scenario harness. It houses:
//!
//! - `scenarios/` — the scenario trait + pilot scenario implementations.
//! - `scenarios/golden.rs` — the golden envelope + `CoreEvent` types.
//! - `bin/gen_scenarios.rs` — generator binary (default mode: write fixtures;
//!   `--check` mode: diff against committed).
//! - `tests/rust_scenarios.rs` — Rust adapter integration tests.
//!
//! # C and Python adapters
//!
//! The `c` and `python` cargo features gate future adapter code that requires
//! the `libtstrans` cdylib or the `tstrans` Python wheel to be present.
//! Those adapters are a separate later phase and are NOT implemented here.
//!
//! # Synthetic data only
//!
//! None of the generators in this crate read from `testfiles/`, any `local/`
//! directory, or any real corpus.

pub mod scenarios;
