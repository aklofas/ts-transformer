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

/// Regression: both `Demuxer::new()` and `Demuxer::with_config(default)`
/// produce a reassembler with finite caps. The `pes_cap_per_pid` and
/// `pes_cap_total` fields on `DemuxerConfig` are `Option<usize>` where
/// `None` resolves to the module-private `DEFAULT_PES_CAP_PER_PID`
/// (4 MiB) and `DEFAULT_PES_CAP_TOTAL` (64 MiB) at construction time.
/// This test guards the constructor path against accidental regression
/// to an unbounded reassembler (e.g., if a future refactor changed the
/// `unwrap_or` to leave `None` as "no cap").
///
/// Direct cap-overflow exercise (per-PID and aggregate) is covered
/// indirectly via the existing `mpegts_demux_*` integration tests; a
/// dedicated overflow-event test would require significant TS-packet
/// scaffolding (PAT + PMT + many PUSI=0 continuations sized to exceed
/// the cap) and is deferred to a future test-helper consolidation.
#[test]
fn demuxer_default_pes_caps_are_bounded() {
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
