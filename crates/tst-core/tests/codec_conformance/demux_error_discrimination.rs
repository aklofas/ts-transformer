//! Discriminating-variant tests for [`DemuxError`].
//!
//! Each test asserts on the **specific error variant** via `matches!` with
//! destructured fields. Asserting on `is_err()` alone cannot catch a future
//! regression that swaps which variant is returned.
//!
//! ## Coverage
//!
//! | Variant | Test | Notes |
//! |---------|------|-------|
//! | `SyncBufExhausted` | `sync_buf_exhausted_when_no_sync_byte_for_4mib` | Reliable: the 4 MiB ceiling is unconditional |
//! | `MalformedPes` | `malformed_pes_strict_mode_returns_malformed_pes_or_strict_rejection` | Strict-mode (`StrictMode::Full`) required; needs valid PAT/PMT first |
//!
//! ### Why `Unrecoverable` and `StrictRejection` are not covered here
//!
//! - `Unrecoverable` requires losing the 0x47 sync byte for >64 KiB
//!   consecutively after an initial sync has been established. That requires
//!   first feeding valid TS to establish sync, which makes the test a
//!   significant amount of state-machine setup. It is covered by unit tests
//!   inside `tst-core/src/mpegts/demux/demuxer.rs`.
//! - `StrictRejection` similarly requires a configured `StrictMode` plus a
//!   specific `NonConformantIssue` that the chosen mode rejects. It is also
//!   covered by unit + integration tests in `mpegts/demux_strict.rs`.

use tst_core::error::DemuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

// ---------------------------------------------------------------------------
// Test 1: SyncBufExhausted — reliable, unconditional ceiling
// ---------------------------------------------------------------------------

/// Feeding more than 4 MiB of bytes without a 0x47 sync byte triggers
/// `DemuxError::SyncBufExhausted`.
///
/// The ceiling is set unconditionally in `Demuxer::feed` regardless of mode.
/// The `observed` field equals the buffer length at the moment the ceiling
/// fired; `max` is always `4 * 1024 * 1024`.
///
/// This test exercises the primary safety-hardening path that bounds
/// adversarial-input memory growth.
#[test]
fn sync_buf_exhausted_when_no_sync_byte_for_4mib() {
    const MIB_4: usize = 4 * 1024 * 1024;

    let mut d = Demuxer::new();

    // Feed 4 MiB + 1 byte of zeros — all-zero contains no 0x47 sync byte,
    // so the demuxer's sync buffer grows until it hits the hard ceiling.
    //
    // We feed in one shot: the ceiling check projects the post-extend total
    // and rejects before copying any byte in, ahead of the inner sync-walk
    // loop. Feeding incrementally works too, but one-shot is the simplest
    // trigger.
    let adversarial = vec![0u8; MIB_4 + 1];
    let err = d
        .feed(&adversarial)
        .expect_err("must error when the 4 MiB sync-buf ceiling is exceeded");

    assert!(
        matches!(
            err,
            DemuxError::SyncBufExhausted { observed, max }
            if observed > MIB_4 && max == MIB_4
        ),
        "expected SyncBufExhausted {{ observed > 4MiB, max == 4MiB }}, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: MalformedPes — strict mode, discriminating on the specific variant
// ---------------------------------------------------------------------------

/// Feeding a stream whose first video PES has a corrupted start-code prefix
/// (`00 00 FF` instead of `00 00 01`) to a **strict-mode** demuxer surfaces
/// either `DemuxError::MalformedPes` or `DemuxError::StrictRejection` — both
/// represent the escalated failure; either is acceptable as a discriminating
/// assertion that the demuxer did NOT silently absorb the error.
///
/// **Strict mode required.** In lenient mode (the default) `MalformedPes` is
/// converted to a `NonConformantIssue::MalformedPes` event and `feed` returns
/// `Ok(())`. Strict-mode (`StrictMode::Full`) is required to surface it as a
/// fatal `DemuxError`. See `demux_malformed_pes_recovery.rs` for the lenient
/// path.
///
/// The test builds a valid TS stream via `Muxer` (so the demuxer sees a real
/// PAT + PMT and opens a video PES assembler for PID 0x100), then hand-patches
/// the first PUSI packet on PID 0x100 so `parse_complete` (pes.rs) returns
/// `DemuxError::MalformedPes { reason: "missing 0x000001 PES start code
/// prefix", .. }`.
#[test]
fn malformed_pes_strict_mode_returns_malformed_pes_or_strict_rejection() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Minimal valid H.264 AU: AUD + IDR slice.
    let h264_au = vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, // AUD NAL
        0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC, // IDR NAL
    ];
    // Push two AUs so the muxer emits PAT + PMT + first video PES (corrupt)
    // + second video PES (recovery). A single AU sometimes doesn't trigger PES
    // emission if the muxer buffers internally; two AUs guarantee the first PES
    // is emitted before the pull loop drains.
    mux.push_video(&h264_au, Pts90khz::new(90_000), true)
        .unwrap();
    // Drain after first AU — PAT + PMT + first video PES.
    let mut ts_bytes = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        ts_bytes.extend_from_slice(&buf[..n]);
    }
    // Push and drain second AU to ensure PES bytes were actually emitted.
    mux.push_video(&h264_au, Pts90khz::new(180_000), true)
        .unwrap();
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        ts_bytes.extend_from_slice(&buf[..n]);
    }

    // Corrupt the first PUSI on PID 0x100: flip byte 2 of the PES start code
    // from 0x01 to 0xFF so `parse_complete` returns MalformedPes.
    let mut corrupted = false;
    for chunk in ts_bytes.chunks_exact_mut(188) {
        if chunk[0] != 0x47 {
            continue;
        }
        let pusi = (chunk[1] & 0x40) != 0;
        let pid = (((chunk[1] as u16) & 0x1F) << 8) | (chunk[2] as u16);
        if !pusi || pid != 0x100 {
            continue;
        }
        // Locate the payload start (skip AF if present).
        let afc = (chunk[3] >> 4) & 0x3;
        let mut payload_off = 4usize;
        if afc & 0x2 != 0 {
            let af_len = chunk[4] as usize;
            payload_off = 5 + af_len;
        }
        if payload_off + 3 >= 188 {
            continue;
        }
        // PES start code: 0x00 0x00 0x01. Flip the third byte.
        chunk[payload_off + 2] = 0xFF;
        corrupted = true;
        break;
    }
    assert!(
        corrupted,
        "test setup: no PUSI on video PID 0x100 found to corrupt"
    );

    // Feed to a strict-mode demuxer; must escalate rather than swallow.
    let mut opts = DemuxerConfig::default();
    opts.strict = StrictMode::Full;
    let mut d = Demuxer::with_config(opts);

    let err = d
        .feed(&ts_bytes)
        .expect_err("strict-mode demuxer must escalate the malformed PES");

    // Either `MalformedPes` (direct path) or `StrictRejection` (NonConformant
    // escalation path via handle_process_packet_result → queue_nonconformant →
    // fatal) is a discriminating signal that the error was NOT silently absorbed.
    assert!(
        matches!(
            err,
            DemuxError::MalformedPes { pid: 0x100, .. } | DemuxError::StrictRejection(_)
        ),
        "expected MalformedPes {{ pid: 0x100, .. }} or StrictRejection(_), got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: an overlong PSI section is not a DemuxError
// ---------------------------------------------------------------------------

/// A PAT whose `section_length` (255) overruns its packet is reported as a
/// non-conformance or dropped — `feed` returns `Ok(())`, never an error.
#[test]
fn overlong_psi_section_is_not_a_demux_error() {
    let mut pkt = [0u8; 188];
    pkt[0] = 0x47; // sync byte
    pkt[1] = 0x40; // PUSI=1, PID high bits = 0
    pkt[2] = 0x00; // PID low bits = 0 (PAT PID)
    pkt[3] = 0x10; // no adaptation field, payload present, CC=0
    pkt[4] = 0x00; // pointer_field = 0 (section starts at pkt[5])
    pkt[5] = 0x00; // table_id = 0x00 (PAT)
    pkt[6] = 0xB0; // section_syntax_indicator=1, '0', reserved=11, section_length hi = 0
    pkt[7] = 0xFF; // section_length lo = 0xFF → 255, overlong for the packet

    let mut d = Demuxer::new();
    let result = d.feed(&pkt);
    assert!(
        result.is_ok(),
        "an overlong PSI section must not surface as a DemuxError (got: {result:?})"
    );
}
