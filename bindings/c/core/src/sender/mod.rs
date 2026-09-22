//! Sender-side C ABI surface — write-side entry points.
//!
//! Submodules:
//! - [`mux_sender`] — `tst_mux_sender_t` + `tst_managed_mux_sender_t`.
//! - [`raw_sender`] — `tst_raw_sender_t` + `tst_managed_raw_sender_t`.
//! - [`ts_sender`]  — `tst_sender_t` + `tst_managed_sender_t` (TS-bytes-in).
//!
//! Note: the standalone `tst_muxer_t` surface lives in the unconditional
//! top-level `crate::muxer` module (not gated on `srt`).

pub mod mux_sender;
pub mod raw_sender;
pub mod ts_sender;
