//! Test helpers shared across the workspace's `tests/` integration suites.
//!
//! The crate is `publish = false` and lives only in `[dev-dependencies]`; no
//! shipping artifact contains it.

pub mod errors;
pub mod mock_transport;
pub mod synthetic_nal;
pub mod rawts;
