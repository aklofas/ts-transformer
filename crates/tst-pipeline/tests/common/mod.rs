//! Shared scaffolding for the tst-pipeline integration tests: in-memory
//! transports, cancel flags, reconnect policies and TS byte helpers.
//!
//! Each test binary declares `mod common;` once at its root and reaches the
//! items as `crate::common::…`. The file is `tests/common/mod.rs`, not
//! `tests/common.rs`, because Cargo compiles every `tests/*.rs` as its own
//! test binary. A helper belongs here when two or more test files in this
//! crate need it; it moves to `tst-test-helpers` only when another crate
//! does.

#![allow(dead_code)] // each binary uses a subset

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tst_core::mpegts::mux::Muxer;
use tst_core::transport::{BrokenCause, TransportCancel, TransportError};
use tst_pipeline::{BackoffStrategy, ReconnectPolicy};

/// `TransportError::Broken` with no errno and an unspecified cause — the
/// shape every mock uses for "the link died".
pub fn broken(msg: &str) -> TransportError {
    TransportError::Broken {
        msg: msg.into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    }
}

pub fn flag(set: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(set))
}

/// A cancel handle that is nothing but a shared flag.
pub struct FlagCancel(pub Arc<AtomicBool>);

impl TransportCancel for FlagCancel {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Reconnect immediately (zero backoff), `max_attempts` times; every other
/// field at its default.
pub fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    }
}

/// Latch-and-poll with a bounded watchdog — never a wall-clock assert.
/// Returns whether `f` held before `deadline` (checked once more at the end).
pub fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    f()
}

/// Minimal valid Annex-B H.264 AU (AUD + IDR slice), 14 bytes. `marker`
/// is the byte after the slice NAL header, so each AU is distinguishable.
pub fn build_h264_au(marker: u8) -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, marker, 0xBB, 0xCC,
    ]
}

/// Tiny ST 0601 local set: UL + BER length + a 4-byte body. It does not
/// decode; the pairer and the muxer treat it as opaque bytes.
pub fn dummy_klv() -> Vec<u8> {
    let mut v = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    v.push(4); // BER short-form length
    v.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    v
}

/// Pull everything the muxer has queued, as one contiguous TS byte run.
pub fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Split packet-aligned muxer output into 188-byte packets.
pub fn to_ts_packets(bytes: &[u8]) -> Vec<[u8; 188]> {
    assert_eq!(
        bytes.len() % 188,
        0,
        "muxer output must be TS-packet aligned"
    );
    bytes
        .chunks_exact(188)
        .map(|c| {
            let mut a = [0u8; 188];
            a.copy_from_slice(c);
            a
        })
        .collect()
}
