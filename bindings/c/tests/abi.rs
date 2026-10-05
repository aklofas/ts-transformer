//! Domain harness: C ABI surface + hygiene: smoke, version, exported-symbol check, header drift, feature matrix, error routing
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `abi::<file>::` prefix. Per-file `#![cfg(feature=…)]`
//! gates still apply (a gated-out member compiles to an empty module).
#[path = "abi/error_routing.rs"]
mod error_routing;
#[path = "abi/feature_matrix_compile.rs"]
mod feature_matrix_compile;
#[path = "abi/header_drift.rs"]
mod header_drift;
#[path = "abi/smoke.rs"]
mod smoke;
#[path = "abi/symbol_audit.rs"]
mod symbol_audit;
#[path = "abi/version_check.rs"]
mod version_check;
