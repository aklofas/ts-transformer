//! Wire-level integration tests for the opt-in
//! `DemuxerConfig::unwrap_timestamps` monotonic PTS/DTS unwrap knob
//! (default off). Drives `Muxer` to produce real TS bytes so the raw
//! 33-bit wire PTS values land exactly where a real encoder would put
//! them, then demuxes with the knob on and off.
//!
//! White-box coverage of the `unwrap_pts` / `unwrap_secondary_ts`
//! accumulator itself lives in `crates/tst-core/src/mpegts/demux/demuxer.rs`.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, MetadataKind, NonConformantIssue, SamplePayload,
};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Mirrors `crates/tst-interop/src/profiles.rs`'s `PTS_ROLLOVER_START`: 5 s
/// (450,000 ticks at 90 kHz) below the 33-bit PTS wraparound boundary
/// (`1 << 33`, ITU-T H.222.0 §2.4.3.6).
const PTS_ROLLOVER_START: i64 = (1i64 << 33) - 450_000;
const WRAP: i64 = 1i64 << 33;

fn minimal_h264_au() -> Vec<u8> {
    // Annex-B: AUD (nal_type=9) + IDR (nal_type=5). Contents aren't
    // parsed by the demuxer (raw-first) — any bytes work as the AU.
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC,
    ]
}

/// Minimal 17-byte KLV LS packet (16-byte ST 0601 UL + 1-byte BER
/// length=0) — bare SMPTE UL, recognized by `classify_klv` as the async
/// (`KlvShape::Async`) shape.
fn minimal_klv() -> Vec<u8> {
    vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ]
}

fn drain(mux: &mut Muxer) -> Vec<u8> {
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

/// Feed `ts_buf` through a fresh `Demuxer` (`unwrap_timestamps` per
/// `unwrap`), flush, and collect every event in order.
fn demux_all(ts_buf: &[u8], unwrap: bool) -> Vec<DemuxEvent> {
    let cfg = DemuxerConfig::builder().unwrap_timestamps(unwrap).build();
    let mut d = Demuxer::with_config(cfg);
    d.feed(ts_buf).unwrap();
    d.flush();
    let mut events = Vec::new();
    while let Some(e) = d.next_event() {
        events.push(e);
    }
    events
}

/// Ordered `pts` (raw ticks) of every video `Sample` event on `pid`.
fn video_pts(events: &[DemuxEvent], pid: u16) -> Vec<i64> {
    events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Sample {
                stream,
                pts,
                payload: SamplePayload::Video { .. },
                ..
            } if stream.pid == pid => Some(pts.as_ticks()),
            _ => None,
        })
        .collect()
}

/// Ordered `pts` (raw ticks) of every KLV-async `Metadata` event on `pid`.
fn klv_pts(events: &[DemuxEvent], pid: u16) -> Vec<i64> {
    events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Metadata {
                stream,
                pts,
                kind: MetadataKind::KlvAsync,
                ..
            } if stream.pid == pid => Some(pts.as_ticks()),
            _ => None,
        })
        .collect()
}

fn single_video_cfg(pid: u16) -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(pid, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Test A — a two-sample video stream whose raw PTS crosses the 33-bit
/// rollover boundary. With the knob off, the emitted PTS stays the raw
/// (small, post-wrap) wire value. With it on, the second sample unwraps
/// to `first + delta`: monotonic, and strictly greater than the first.
#[test]
fn wrap_forward_unwraps_monotonically_when_enabled() {
    let mut mux = Muxer::new(single_video_cfg(0x100)).unwrap();
    let au = minimal_h264_au();

    let raw1 = PTS_ROLLOVER_START;
    let delta = 900_000i64; // ~10 s at 90 kHz — comfortably crosses the boundary
    let raw2 = raw1 + delta - WRAP; // wraps to a small post-boundary value
    assert!(
        (0..WRAP).contains(&raw2),
        "test setup: raw2 must itself be a legal 33-bit wire value"
    );

    mux.push_video(&au, Pts90khz::new(raw1), true).unwrap();
    mux.push_video(&au, Pts90khz::new(raw2), true).unwrap();
    let ts_buf = drain(&mut mux);

    // Default off: byte-for-byte today's behavior — the still-wrapped
    // raw value comes through unchanged.
    let off = video_pts(&demux_all(&ts_buf, false), 0x100);
    assert_eq!(off, vec![raw1, raw2]);

    // Opt-in: unwrapped to a monotonic timeline.
    let on = video_pts(&demux_all(&ts_buf, true), 0x100);
    assert_eq!(on, vec![raw1, raw1 + delta]);
    assert!(
        on[1] > on[0],
        "unwrapped second PTS must be monotonic across the wrap, got {on:?}"
    );
}

/// Test B — a normal non-wrapping stream emits identical PTS values
/// whether the knob is on or off (the default-off byte-identity proof,
/// checked from the "on" side too: no wrap means no divergence at all).
#[test]
fn non_wrapping_stream_emits_identical_pts_both_modes() {
    let mut mux = Muxer::new(single_video_cfg(0x100)).unwrap();
    let au = minimal_h264_au();
    let base = 90_000i64; // 1 s at 90 kHz
    for i in 1..=3 {
        mux.push_video(&au, Pts90khz::new(base * i), true).unwrap();
    }
    let ts_buf = drain(&mut mux);

    let off = video_pts(&demux_all(&ts_buf, false), 0x100);
    let on = video_pts(&demux_all(&ts_buf, true), 0x100);
    assert_eq!(off, vec![base, base * 2, base * 3]);
    assert_eq!(off, on, "no wrap occurred — both modes must agree exactly");
}

/// Test C — a video PES with no PTS still emits `pts = 0` plus
/// `MissingRequiredPts`, even with the knob on (the synthesized 0 must
/// never reach the accumulator), and a real PTS immediately afterward on
/// the same PID is unaffected.
#[test]
fn missing_pts_stays_zero_and_does_not_corrupt_following_pts() {
    // Single continuous mux session, two video AUs on one PID — avoids
    // the continuity-counter reset that two independent `Muxer`
    // instances concatenated together would introduce.
    let mut mux = Muxer::new(single_video_cfg(0x101)).unwrap();
    let au = minimal_h264_au();
    let placeholder_pts = 90_000i64; // stripped below; the value is irrelevant
    let raw2 = 180_000i64; // real forward-progressing PTS, no wrap
    mux.push_video(&au, Pts90khz::new(placeholder_pts), true)
        .unwrap();
    mux.push_video(&au, Pts90khz::new(raw2), true).unwrap();
    let ts_buf = drain(&mut mux);

    // Strip PTS_DTS_flags + header_data_length on the FIRST PES only
    // (the needle scan returns the earliest match) so the demuxer parses
    // it with no PTS at all — the same technique used by
    // `demux_pes_validation.rs`'s B4 coverage. The second PES's
    // PES_packet_length is untouched, so this patch is fully local to
    // the first PES's header.
    let patched = patch_pes_strip_pts(ts_buf, 0xE0);

    let cfg = DemuxerConfig::builder().unwrap_timestamps(true).build();
    let mut d = Demuxer::with_config(cfg);
    d.feed(&patched).unwrap();
    d.flush();
    let mut events = Vec::new();
    while let Some(e) = d.next_event() {
        events.push(e);
    }

    let pts_seq = video_pts(&events, 0x101);
    assert_eq!(
        pts_seq,
        vec![0, raw2],
        "missing-PTS sample stays 0 (never unwrapped), following real PTS is untouched"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MissingRequiredPts { pid: 0x101 },
                ..
            }
        )),
        "expected MissingRequiredPts on PID 0x101, got {events:?}"
    );
}

/// Test C2 (Fix round 1 regression) — the 33-bit wrap boundary can fall
/// BETWEEN one AU's DTS and PTS (DTS <= PTS always, so DTS can still be
/// pre-wrap while PTS has already wrapped — once per ~26.5h on any
/// B-frame stream). The unwrapped DTS must stay in the PRE-wrap epoch
/// (below the unwrapped PTS), not get shifted a full `1 << 33` past it.
#[test]
fn dts_straddling_the_wrap_stays_below_unwrapped_pts() {
    let cfg = single_video_cfg(0x102);
    let mut mux = Muxer::new(cfg).unwrap();
    let handle = mux.video_handles()[0];
    let au = minimal_h264_au();

    // First AU anchors the pre-wrap epoch (a plain PTS-only push).
    mux.push_video_to(handle, &au, Pts90khz::new(PTS_ROLLOVER_START), true)
        .unwrap();

    // Second AU: PTS has wrapped (small raw); this AU's DTS is still
    // pre-wrap (large raw) — decode-before-compose, straddling the
    // boundary within one PES.
    let pts_raw = 100i64;
    let dts_raw = WRAP - 200; // 300 ticks before pts_raw in real (wrapped) time
    mux.push_video_to_with_dts(
        handle,
        &au,
        Pts90khz::new(pts_raw),
        Pts90khz::new(dts_raw),
        true,
    )
    .unwrap();
    let ts_buf = drain(&mut mux);

    let events = demux_all(&ts_buf, true);
    let (pts_out, dts_out) = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::Sample {
                stream,
                pts,
                dts,
                payload: SamplePayload::Video { .. },
                ..
            } if stream.pid == 0x102 && pts.as_ticks() != PTS_ROLLOVER_START => {
                Some((pts.as_ticks(), dts.map(|d| d.as_ticks())))
            }
            _ => None,
        })
        .expect("the wrapped video sample");

    assert_eq!(pts_out, WRAP + pts_raw, "unwrapped PTS");
    let dts_out = dts_out.expect("DTS present on this sample");
    assert_eq!(
        dts_out, dts_raw,
        "DTS must stay in the pre-wrap epoch (offset 0 relative to its own raw value)"
    );
    assert!(
        dts_out < pts_out,
        "DTS must stay <= PTS across the wrap straddle, got dts={dts_out} pts={pts_out}"
    );
    assert_ne!(
        dts_out,
        WRAP + dts_raw,
        "regression guard: DTS must NOT be shifted a full 1<<33 epoch past its correct value"
    );
}

/// Test E (CORR-04) — a REORDERED pre-wrap PTS arriving after the wrap
/// must not be pushed into a second epoch. `W-50` arrives numerically
/// LARGER than the preceding (already-wrapped, small) `100`, so a
/// detector that only bumps a running offset when `raw < last_raw`
/// leaves the just-bumped offset applied to it — emitting `2W-50` and
/// then compounding into `2W+200` on the next sample. Signed modular
/// accumulation onto the previous UNWRAPPED value keeps every sample in
/// the epoch its wrap-aware delta actually places it in.
#[test]
fn reordered_pts_across_wrap_does_not_double_the_epoch() {
    let mut mux = Muxer::new(single_video_cfg(0x100)).unwrap();
    let au = minimal_h264_au();

    // Composition order straddling the boundary: two pre-wrap values
    // (`WRAP-100`, `WRAP-50`) interleaved with two post-wrap ones
    // (`100`, `200`) — i.e. the pre-wrap `WRAP-50` is delivered late,
    // after the wrap has already been observed.
    for pts in [WRAP - 100, 100, WRAP - 50, 200] {
        mux.push_video(&au, Pts90khz::new(pts), true).unwrap();
    }
    let ts_buf = drain(&mut mux);

    let got = video_pts(&demux_all(&ts_buf, true), 0x100);
    assert_eq!(
        got,
        vec![WRAP - 100, WRAP + 100, WRAP - 50, WRAP + 200],
        "each sample must land in the epoch its signed 33-bit delta implies; \
         a second epoch (2*WRAP) means the reorder was mistaken for a wrap"
    );
}

/// Locate the PES start (`00 00 01 <stream_id>`) and clear `PTS_DTS_flags`
/// (top 2 bits of byte 7) + zero `header_data_length` (byte 8), so the
/// parsed PES has no PTS. Mirrors
/// `tests/mpegts/demux_pes_validation.rs::patch_pes_strip_pts`.
fn patch_pes_strip_pts(mut bytes: Vec<u8>, stream_id: u8) -> Vec<u8> {
    let needle = [0x00u8, 0x00, 0x01, stream_id];
    let pos = bytes
        .windows(4)
        .position(|w| w == needle)
        .expect("PES start code should appear in the muxed TS");
    let flags2_off = pos + 7;
    let hdr_len_off = pos + 8;
    bytes[flags2_off] &= 0x3F;
    bytes[hdr_len_off] = 0;
    bytes
}

/// Test D — a video PID and a KLV PID, each anchored at a *different*
/// first-observed raw PTS, both cross the same 33-bit wrap. At a later
/// wire instant where they carry the identical raw PTS (same wire clock,
/// sampled together), their unwrapped values must be exactly equal —
/// proving the accumulator emits `offset + raw` (comparable across PIDs)
/// rather than rebasing each PID's timeline to its own zero.
#[test]
fn video_and_klv_pids_share_one_wrap_aware_clock() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(
            0x300,
            KlvStreamType::PrivateData,
            /* carries_pts= */ true,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let au = minimal_h264_au();
    let klv = minimal_klv();

    // Different anchors: KLV starts flowing 30_000 ticks before video.
    let video_raw1 = PTS_ROLLOVER_START;
    let klv_raw1 = PTS_ROLLOVER_START - 30_000;
    assert_ne!(video_raw1, klv_raw1, "test setup: anchors must differ");

    // Both PIDs are then sampled again at the SAME later wire instant —
    // same raw value on both, past the wrap.
    let shared_raw2 = 450_000i64;

    mux.push_video(&au, Pts90khz::new(video_raw1), true)
        .unwrap();
    mux.push_klv(&klv, Pts90khz::new(klv_raw1), 0x00).unwrap();
    mux.push_video(&au, Pts90khz::new(shared_raw2), true)
        .unwrap();
    mux.push_klv(&klv, Pts90khz::new(shared_raw2), 0x00)
        .unwrap();
    let ts_buf = drain(&mut mux);

    let events = demux_all(&ts_buf, true);
    let video = video_pts(&events, 0x100);
    let klv_out = klv_pts(&events, 0x300);

    assert_eq!(video.len(), 2, "expected 2 video samples, got {video:?}");
    assert_eq!(klv_out.len(), 2, "expected 2 KLV samples, got {klv_out:?}");

    // Each PID anchors independently at its own first-observed raw value.
    assert_eq!(video[0], video_raw1);
    assert_eq!(klv_out[0], klv_raw1);
    assert_ne!(
        video[0], klv_out[0],
        "the two PIDs' anchors must stay distinct (not rebased to a shared zero)"
    );

    // At the shared later wire instant, both PIDs independently detected
    // the same single wrap and must land on the EXACT same unwrapped
    // value — this is what makes PTS-based KLV-to-frame pairing possible.
    assert_eq!(
        video[1], klv_out[1],
        "video and KLV must be directly comparable at a shared wire instant"
    );
    assert_eq!(video[1], WRAP + shared_raw2);
    assert!(video[1] > video[0] && klv_out[1] > klv_out[0]);
}
