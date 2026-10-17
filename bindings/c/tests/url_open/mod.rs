//! C-ABI URL parsing integration tests.
//!
//! Each test opens a real listener on a random local port, opens a sender
//! via the C ABI with a URL containing query params, and verifies the
//! resulting socket has the expected option values applied.
//!
//! Threading pattern: the Listener runs on a background thread (binding to
//! 127.0.0.1:0 and communicating the kernel-assigned port back via mpsc).
//! The sender is opened on the main thread so all raw C-ABI pointers
//! (*mut TstSenderConfig, *mut TstSender) never cross thread
//! boundaries.
//!
//! This file is the entry point for the folder-shaped `url_open` integration
//! test binary; the actual test bodies live in stream-type-keyed siblings
//! (see the `mod` declarations below). Shared helpers live here so each
//! sibling can pick them up via `use super::*;`.

// All sibling files import `tst_srt::SrtListenerBuilder` and
// `tstrans::sender::*`/`receiver::*` (all `cfg(feature = "srt")`).
#![cfg(feature = "srt")]
#![allow(unused_unsafe)]

#[allow(dead_code)]
#[path = "../common/mod.rs"]
mod common;
pub(crate) use common::last_error_msg;

mod demux_receiver;
mod mux_sender;
mod raw_sender;
mod ts_sender;
