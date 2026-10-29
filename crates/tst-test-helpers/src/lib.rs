//! Test helpers shared across the workspace's `tests/` integration suites.
//!
//! The crate is `publish = false`; no shipping artifact contains it. It is a
//! `[dev-dependencies]` entry everywhere except tst-interop (a test tool) and
//! tst-jni, whose test-only `jni-test-hooks` feature builds `whep_viewer`.

pub mod capability;
pub mod errors;
pub mod mock_transport;
pub mod synthetic_nal;
pub mod rawts;
pub mod timing;
#[cfg(feature = "whep-viewer")]
pub mod whep_viewer;
