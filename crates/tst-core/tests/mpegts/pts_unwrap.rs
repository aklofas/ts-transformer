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

    // Default off: byte-for-byte today's behavior — the raw wire values
    // come through unchanged, reorder and all.
    assert_eq!(
        video_pts(&demux_all(&ts_buf, false), 0x100),
        vec![WRAP - 100, 100, WRAP - 50, 200]
    );

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

/// Test F (CORR-06) — a PID whose FIRST sample arrives after the
/// program's wire clock has already wrapped must anchor onto the
/// program's running timeline, not at its own raw (post-wrap, small)
/// value. Video on PID 0x100 crosses the boundary first (`W-100` then
/// `100`); the KLV PID 0x101 in the SAME program starts flowing only at
/// the shared post-wrap instant `100`. Anchoring it at raw would strand
/// it a full epoch BELOW the video sample it was sampled with, breaking
/// the cross-PID comparability the knob documents.
#[test]
fn late_starting_pid_anchors_to_its_programs_running_clock() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(
            0x101,
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

    // Drain after each push so the wire order is unambiguous: both video
    // samples (and therefore the program's wrap) precede the KLV PID's
    // very first sample.
    let mut ts_buf = Vec::new();
    mux.push_video(&au, Pts90khz::new(WRAP - 100), true)
        .unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));
    mux.push_video(&au, Pts90khz::new(100), true).unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));
    mux.push_klv(&klv, Pts90khz::new(100), 0x00).unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));

    let events = demux_all(&ts_buf, true);
    let video = video_pts(&events, 0x100);
    let klv_out = klv_pts(&events, 0x101);

    assert_eq!(
        video,
        vec![WRAP - 100, WRAP + 100],
        "test setup: the video PID must establish the program's post-wrap epoch first"
    );
    assert_eq!(
        klv_out,
        vec![WRAP + 100],
        "a late-starting PID must anchor onto its program's running clock, \
         not one epoch below it at its own raw value"
    );
    assert_eq!(
        klv_out[0], video[1],
        "KLV sampled at the same wire instant as the video frame must compare equal"
    );

    // Knob off: byte-for-byte today's behavior — raw wire values only.
    let off = demux_all(&ts_buf, false);
    assert_eq!(video_pts(&off, 0x100), vec![WRAP - 100, 100]);
    assert_eq!(klv_pts(&off, 0x101), vec![100]);
}

/// Test G (CORR-06 control) — programs are independent time bases
/// (ITU-T H.222.0 §2.4.3.5), so the per-program anchor must NEVER cross
/// program boundaries. Program 1's video wraps; program 2's video then
/// starts for the first time at a small raw value. Program 2 must anchor
/// at its own raw value — inheriting program 1's epoch would invent a
/// ~26.5 h offset out of thin air.
#[test]
fn independent_programs_do_not_share_an_anchor() {
    let cfg = {
        let mut prog1 = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog1.add_video(0x1011, VideoCodec::H264);
        let mut prog2 = MuxerProgramConfigBuilder::new(2, 0x1100);
        prog2.add_video(0x1111, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog1.build());
        b.add_program(prog2.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let au = minimal_h264_au();
    let p1_video = mux.video_handles_for_program(1).unwrap()[0];
    let p2_video = mux.video_handles_for_program(2).unwrap()[0];

    // Program 1 wraps; only afterwards does program 2 emit anything.
    let mut ts_buf = Vec::new();
    mux.push_video_to(p1_video, &au, Pts90khz::new(WRAP - 100), true)
        .unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));
    mux.push_video_to(p1_video, &au, Pts90khz::new(100), true)
        .unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));
    mux.push_video_to(p2_video, &au, Pts90khz::new(100), true)
        .unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));

    let events = demux_all(&ts_buf, true);
    assert_eq!(video_pts(&events, 0x1011), vec![WRAP - 100, WRAP + 100]);
    assert_eq!(
        video_pts(&events, 0x1111),
        vec![100],
        "program 2 has its own time base — it must anchor at its own raw \
         value, never inherit program 1's epoch"
    );
}

/// Test H (CORR-06 follow-up) — the program's running reference is simply
/// the LAST value emitted on any of its PIDs, so it can legitimately sit
/// BELOW the 33-bit boundary after the program has already crossed it.
/// The KLV PID delivers `WRAP-50` late, after `100`; its PES is
/// length-BOUNDED, so each sample completes on the wire as it is written
/// and the reference genuinely ends on `(WRAP-50, WRAP-50)` before the
/// video PID debuts. (The reorder cannot ride the video PID for this:
/// video PES is length-unbounded, so its last AU only finalises at
/// `flush()` — after the newcomer has already anchored.)
///
/// What this pins is that the newcomer anchors *through* that below-the-
/// boundary reference rather than at its own raw value:
/// `pts_diff_33bit(100, WRAP-50) = +150` carries it back up to `WRAP+100`.
/// It is deliberately NOT a case that distinguishes one reference from
/// another — signed modular accumulation is path-independent while every
/// gap stays under half an epoch, so anchoring off the earlier
/// `(100, WRAP+100)` reference yields the same answer, and no newcomer
/// value at this scale could separate them. The mutation it does catch is
/// dropping the program anchor entirely (`anchor_first_sample` returning
/// `raw_ticks`), which strands the newcomer at `100` — a full `1 << 33`
/// below the KLV sample it was captured with.
#[test]
fn late_starting_pid_anchors_through_a_reordered_reference_sample() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(
            0x101,
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

    // Drain after each push so the wire order is unambiguous: the KLV PID
    // crosses the boundary and then delivers its late pre-wrap sample, all
    // before the video PID's very first sample.
    let mut ts_buf = Vec::new();
    for pts in [WRAP - 100, 100, WRAP - 50] {
        mux.push_klv(&klv, Pts90khz::new(pts), 0x00).unwrap();
        ts_buf.extend_from_slice(&drain(&mut mux));
    }
    mux.push_video(&au, Pts90khz::new(100), true).unwrap();
    ts_buf.extend_from_slice(&drain(&mut mux));

    let events = demux_all(&ts_buf, true);
    assert_eq!(
        klv_pts(&events, 0x101),
        vec![WRAP - 100, WRAP + 100, WRAP - 50],
        "test setup: every KLV sample emits as it is written, so the program \
         reference standing when the video PID debuts is the reordered \
         (WRAP-50, WRAP-50)"
    );
    assert_eq!(
        video_pts(&events, 0x100),
        vec![WRAP + 100],
        "the newcomer sits +150 ticks from the reordered reference, which \
         carries it back above the boundary alongside the KLV sample it was \
         captured with; anchoring at its own raw value leaves it at 100"
    );
}

// ── CORR-07 topology fixtures ───────────────────────────────────────────
//
// The `Muxer` always writes PSI at version 0, so it cannot express the PMT
// version bump a PID removal/re-addition needs. These builders hand-write
// the PAT/PMT sections instead; the elementary packets (and therefore every
// wire PTS value under test) still come from real muxer output, with the
// muxer's own PSI filtered back out by `strip_psi`.

/// Build a PAT section (table_id 0x00). `programs` is `(program_number,
/// pmt_pid)`.
fn build_pat_section(version: u8, programs: &[(u16, u16)]) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x00); // table_id = PAT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F)); // ssi=1, reserved, length hi
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
    s.push(0xC1 | ((version & 0x1F) << 1)); // reserved | version | current_next=1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    for &(pn, pid) in programs {
        s.extend_from_slice(&pn.to_be_bytes());
        s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        s.push((pid & 0xFF) as u8);
    }
    append_crc(&mut s);
    s
}

/// Build a PMT section (table_id 0x02). `streams` is `(stream_type,
/// elementary_pid, es_info_descriptor_bytes)`.
fn build_pmt_section(
    program_number: u16,
    pcr_pid: u16,
    version: u8,
    streams: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let stream_loop_len: usize = streams.iter().map(|(_, _, d)| 5 + d.len()).sum();
    let section_length = 9 + stream_loop_len + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x02); // table_id = PMT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&program_number.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    s.push((pcr_pid & 0xFF) as u8);
    s.push(0xF0); // reserved | program_info_length hi
    s.push(0x00); // program_info_length lo (no program descriptors)
    for &(stream_type, pid, descriptors) in streams {
        s.push(stream_type);
        s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        s.push((pid & 0xFF) as u8);
        s.push(0xF0 | ((descriptors.len() >> 8) as u8 & 0x0F));
        s.push((descriptors.len() & 0xFF) as u8);
        s.extend_from_slice(descriptors);
    }
    append_crc(&mut s);
    s
}

/// Append the CRC-32/MPEG-2 trailer over everything written so far.
fn append_crc(section: &mut Vec<u8>) {
    let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(section);
    section.extend_from_slice(&crc.to_be_bytes());
}

/// Wrap a PSI section into one 188-byte TS packet (PUSI, payload-only).
///
/// `cc` must advance across successive packets on the same PID or the
/// demuxer's duplicate suppression swallows the second one.
fn psi_packet(pid: u16, section: &[u8], cc: u8) -> Vec<u8> {
    let mut pkt = vec![0xFFu8; 188];
    pkt[0] = 0x47; // sync byte
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID hi
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only + continuity counter
    pkt[4] = 0x00; // pointer_field
    let end = 5 + section.len();
    assert!(end <= 188, "section too large for one TS packet");
    pkt[5..end].copy_from_slice(section);
    pkt
}

/// Drop every packet the muxer wrote on the PAT PID or `pmt_pid`, leaving
/// only elementary-stream packets for the hand-built topology to describe.
fn strip_psi(ts: &[u8], pmt_pid: u16) -> Vec<u8> {
    assert_eq!(ts.len() % 188, 0, "muxer output must be whole TS packets");
    let mut out = Vec::with_capacity(ts.len());
    for pkt in ts.chunks_exact(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pid != 0x0000 && pid != pmt_pid {
            out.extend_from_slice(pkt);
        }
    }
    out
}

/// Test I (CORR-07) — a PID removed from the PMT and re-added after the
/// program's wire clock has wrapped must anchor its first post-re-add
/// sample onto the PROGRAM's running clock, never onto the accumulator its
/// dead predecessor left behind. Here the stale entry is more than half a
/// 33-bit epoch old, so replaying it strands the sample a full `1 << 33`
/// below its siblings — the same hazard a replacement stream reusing the
/// PID would hit.
#[test]
fn pid_removed_and_readded_across_a_wrap_anchors_to_the_program_clock() {
    const PMT_PID: u16 = 0x1000;
    /// Registration descriptor `KLVA` — byte-for-byte what the muxer's own
    /// PMT writes for a `KlvStreamType::PrivateData` stream, and what makes
    /// the demuxer classify stream_type 0x06 as asynchronous KLV.
    const KLVA: &[u8] = &[0x05, 0x04, b'K', b'L', b'V', b'A'];
    let video: (u8, u16, &[u8]) = (0x1B, 0x100, &[]);
    let klv_es: (u8, u16, &[u8]) = (0x06, 0x101, KLVA);

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, PMT_PID);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(
            0x101,
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

    // v0 topology: both PIDs present.
    let mut ts = Vec::new();
    ts.extend_from_slice(&psi_packet(
        0x0000,
        &build_pat_section(0, &[(1, PMT_PID)]),
        0,
    ));
    ts.extend_from_slice(&psi_packet(
        PMT_PID,
        &build_pmt_section(1, 0x100, 0, &[video, klv_es]),
        0,
    ));

    // Both PIDs start at raw 0. The KLV PES carries a bounded
    // `PES_packet_length`, so its sample — and its accumulator entry —
    // completes on the wire before the next PMT arrives.
    mux.push_video(&au, Pts90khz::new(0), true).unwrap();
    ts.extend_from_slice(&strip_psi(&drain(&mut mux), PMT_PID));
    mux.push_klv(&klv, Pts90khz::new(0), 0x00).unwrap();
    ts.extend_from_slice(&strip_psi(&drain(&mut mux), PMT_PID));

    // v1 topology: PID 0x101 is gone.
    ts.extend_from_slice(&psi_packet(
        PMT_PID,
        &build_pmt_section(1, 0x100, 1, &[video]),
        1,
    ));

    // The video PID carries program 1 past the 33-bit boundary in steps
    // small enough to stay unambiguous (each well under half an epoch).
    // The trailing step exists so the wrapped sample (raw 410_065_408) is
    // EMITTED — and therefore published to the program clock — before the
    // KLV PID resumes; video PES is length-unbounded and only finalises on
    // the next PUSI for that PID.
    for pts in [
        3_000_000_000i64,
        6_000_000_000,
        9_000_000_000 - WRAP,
        9_090_000_000 - WRAP,
    ] {
        mux.push_video(&au, Pts90khz::new(pts), true).unwrap();
        ts.extend_from_slice(&strip_psi(&drain(&mut mux), PMT_PID));
    }

    // v2 topology: PID 0x101 is back, resuming at the wire instant the
    // video PID has already unwrapped to 9_000_000_000.
    ts.extend_from_slice(&psi_packet(
        PMT_PID,
        &build_pmt_section(1, 0x100, 2, &[video, klv_es]),
        2,
    ));
    mux.push_klv(&klv, Pts90khz::new(9_000_000_000 - WRAP), 0x00)
        .unwrap();
    ts.extend_from_slice(&strip_psi(&drain(&mut mux), PMT_PID));

    let events = demux_all(&ts, true);
    assert_eq!(
        video_pts(&events, 0x100),
        vec![
            0,
            3_000_000_000,
            6_000_000_000,
            9_000_000_000,
            9_090_000_000
        ],
        "test setup: the surviving video PID must carry program 1 across the wrap"
    );
    assert_eq!(
        klv_pts(&events, 0x101),
        vec![0, 9_000_000_000],
        "the re-added PID's first sample must anchor onto its program's \
         running clock; replaying the accumulator left behind before the \
         removal lands it one full epoch low, at 410_065_408"
    );
}
