//! Demuxer resource-cap regression tests.
//!
//! Validates that hostile input cannot grow `Demuxer::sync_buf` past the
//! 4 MiB hard ceiling. If `extend_from_slice` ran before the
//! `SYNC_SEARCH_WINDOW` check, a peer feeding a single multi-GB `feed` with
//! no 0x47 sync byte could OOM the host before the loop got a chance to bail.

use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};

#[test]
fn demux_rejects_unbounded_sync_buf_growth() {
    let mut dx = Demuxer::new();
    // 8 MiB of 0xFF (no sync byte 0x47 anywhere). Twice the 4 MiB cap so
    // the very first `feed` call must trip the ceiling.
    let garbage = vec![0xFFu8; 8 * 1024 * 1024];
    let result = dx.feed(&garbage);
    assert!(
        matches!(result, Err(DemuxError::SyncBufExhausted { .. })),
        "expected SyncBufExhausted, got {result:?}"
    );
    // The `observed` figure reports the projected post-extend total even
    // though the bytes were never copied in (check-before-extend, T2-DEMUX-INGRESS).
    if let Err(DemuxError::SyncBufExhausted { observed, max }) = result {
        assert_eq!(observed, 8 * 1024 * 1024);
        assert_eq!(max, 4 * 1024 * 1024);
    }
    // Post-rejection the buffer is cleared, so the demuxer remains usable:
    // a subsequent small, well-under-cap feed is accepted (returns Ok).
    assert!(dx.feed(&[0xFFu8; 16]).is_ok());
}

/// Every demuxer constructor path builds without panicking: `new()`,
/// `with_config(default)` (both resolve the `None` cap fields to the
/// default constants) and `with_config` with explicit small caps.
///
/// This checks construction only: it does not show the resolved caps are
/// finite, which would need a PES stream sized past the cap.
#[test]
fn demuxer_default_pes_caps_construct_without_panic() {
    // Constructor path 1: zero-arg `new()`.
    let _ = Demuxer::new();
    // Constructor path 2: `with_config(default)` — also resolves both
    // `None` cap fields to the default constants.
    let _ = Demuxer::with_config(DemuxerConfig::default());
    // Constructor path 3: explicit cap overrides also succeed without
    // panicking. 1 MiB / 8 MiB are arbitrary finite values — the goal
    // here is the constructor doesn't reject small caps.
    let _ = Demuxer::with_config({
        let mut cfg = DemuxerConfig::default();
        cfg.pes_cap_per_pid = Some(1024 * 1024);
        cfg.pes_cap_total = Some(8 * 1024 * 1024);
        cfg
    });
}
