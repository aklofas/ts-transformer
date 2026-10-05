//! Lenient-mode recovery from `DemuxError::MalformedPes` on both feed paths.
//!
//! Background: if `Demuxer::feed` and `Demuxer::feed_aligned` propagated
//! `MalformedPes` fatally, that would end the receive loop in
//! `tst_pipeline::DemuxReceiver`: a single corrupt PES header on one PID
//! would tear down the whole receiver. This file
//! verifies that lenient mode (default) converts the error to a
//! `NonConformantIssue::MalformedPes` event and continues parsing, on
//! BOTH the byte-stream `feed` path and the aligned-packet `feed_aligned`
//! path (the hot path used by `DemuxReceiver`).

use tst_core::error::DemuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, SamplePayload, StrictMode,
};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

/// One H.264 AU (AUD + IDR) — minimal valid input for the muxer.
fn build_minimal_h264_au() -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, // AUD
        0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC, // IDR
    ]
}

/// Drain the muxer fully into a Vec.
fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
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

/// Build a TS byte stream containing: PAT + PMT + first H.264 PES (with its
/// PES start-code corrupted → triggers `MalformedPes` on the FIRST PUSI for
/// the video PID) + a second valid H.264 PES (recovery target).
///
/// Returns `(bytes, aligned_packets)` so tests can exercise both `feed`
/// (the byte-stream path) and `feed_aligned` (the hot path).
fn build_stream_with_malformed_pes() -> (Vec<u8>, Vec<[u8; 188]>) {
    // Build a real well-formed stream first via Muxer, then corrupt the
    // first video PES's start-code byte. This guarantees PAT/PMT/PSI
    // structure is valid (so the demuxer accepts the program shape and
    // routes the corrupt PUSI packet to PES reassembly).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // First AU → first PES on PID 0x100. This is the one we'll corrupt.
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let bytes1 = drain_mux(&mut mux);

    // Second AU → second PES on PID 0x100. Stays well-formed; the demuxer
    // must recover and emit a Sample for it.
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(180_000), true)
        .unwrap();
    let bytes2 = drain_mux(&mut mux);

    let mut bytes = bytes1;
    bytes.extend_from_slice(&bytes2);

    // Walk packets and find the first PUSI packet on PID 0x100, then
    // corrupt the PES start-code prefix at the payload start.
    // TS packet layout: byte 0=sync, byte 1 has PUSI=0x40 and high PID
    // bits, byte 2 is low PID bits, byte 3 has AFC; payload begins at
    // offset 4 (no AF in our muxer for the PUSI packet of the first PES;
    // verify and skip AF if present).
    let mut corrupted = false;
    for chunk in bytes.chunks_exact_mut(188) {
        if chunk[0] != 0x47 {
            continue;
        }
        let pusi = (chunk[1] & 0x40) != 0;
        let pid = (((chunk[1] as u16) & 0x1F) << 8) | (chunk[2] as u16);
        if !pusi || pid != 0x100 {
            continue;
        }
        let afc = (chunk[3] >> 4) & 0x3;
        let mut payload_off = 4usize;
        if afc & 0x2 != 0 {
            let af_len = chunk[4] as usize;
            payload_off = 5 + af_len;
        }
        if payload_off + 3 >= 188 {
            continue;
        }
        // PES start code prefix is `00 00 01` at the payload start. Flip
        // the third byte so `parse_complete` in pes.rs returns
        // `DemuxError::MalformedPes { reason: "missing 0x000001 PES start
        // code prefix", ... }`.
        chunk[payload_off + 2] = 0xFF;
        corrupted = true;
        break;
    }
    assert!(
        corrupted,
        "test setup: no PUSI packet found on video PID 0x100 to corrupt"
    );

    let packets: Vec<[u8; 188]> = bytes
        .chunks_exact(188)
        .map(|c| {
            let mut a = [0u8; 188];
            a.copy_from_slice(c);
            a
        })
        .collect();

    (bytes, packets)
}

#[test]
fn malformed_pes_variant_exists() {
    let issue = NonConformantIssue::MalformedPes {
        pid: 0x100,
        reason: "test",
    };
    let _ = issue;
}

#[test]
fn feed_lenient_mode_recovers_from_malformed_pes() {
    let mut d = Demuxer::new(); // default = StrictMode::Off
    let (bytes, _) = build_stream_with_malformed_pes();
    d.feed(&bytes)
        .expect("lenient feed must not propagate MalformedPes fatally");
    d.flush();

    let mut saw_malformed = false;
    let mut saw_sample = false;
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MalformedPes { .. },
                ..
            } => saw_malformed = true,
            DemuxEvent::Sample {
                payload: SamplePayload::Video { .. },
                ..
            } => saw_sample = true,
            _ => {}
        }
    }
    assert!(
        saw_malformed,
        "feed: must surface MalformedPes as NonConformant in lenient mode"
    );
    assert!(
        saw_sample,
        "feed: must continue parsing past the corrupt PES and emit the recovery Sample"
    );
}

#[test]
fn feed_aligned_lenient_mode_recovers_from_malformed_pes() {
    let mut d = Demuxer::new();
    let (_, packets) = build_stream_with_malformed_pes();
    for pkt in &packets {
        d.feed_aligned(pkt)
            .expect("lenient feed_aligned must not propagate MalformedPes fatally");
    }
    d.flush();

    let mut saw_malformed = false;
    let mut saw_sample = false;
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MalformedPes { .. },
                ..
            } => saw_malformed = true,
            DemuxEvent::Sample {
                payload: SamplePayload::Video { .. },
                ..
            } => saw_sample = true,
            _ => {}
        }
    }
    assert!(
        saw_malformed,
        "feed_aligned: must surface MalformedPes as NonConformant in lenient mode"
    );
    assert!(
        saw_sample,
        "feed_aligned: must continue parsing past the corrupt PES and emit the recovery Sample"
    );
}

#[test]
fn feed_strict_mode_escalates_malformed_pes_to_error() {
    let mut opts = DemuxerConfig::default();
    opts.strict = StrictMode::Full;
    let mut d = Demuxer::with_config(opts);
    let (bytes, _) = build_stream_with_malformed_pes();
    let err = d
        .feed(&bytes)
        .expect_err("strict feed must escalate MalformedPes");
    // Strict-mode rejection of a NonConformant goes through
    // `DemuxError::StrictRejection`; the underlying `MalformedPes` error
    // path goes through `DemuxError::MalformedPes`. Either is acceptable
    // as long as the strict path doesn't silently absorb the failure.
    assert!(
        matches!(
            err,
            DemuxError::MalformedPes { .. } | DemuxError::StrictRejection(_)
        ),
        "strict feed should escalate MalformedPes (got {err:?})"
    );
}

#[test]
fn feed_aligned_strict_mode_escalates_malformed_pes_to_error() {
    let mut opts = DemuxerConfig::default();
    opts.strict = StrictMode::Full;
    let mut d = Demuxer::with_config(opts);
    let (_, packets) = build_stream_with_malformed_pes();
    let mut last_err: Option<DemuxError> = None;
    for pkt in &packets {
        if let Err(e) = d.feed_aligned(pkt) {
            last_err = Some(e);
            break;
        }
    }
    let err = last_err.expect("strict feed_aligned must escalate at the malformed PES");
    assert!(
        matches!(
            err,
            DemuxError::MalformedPes { .. } | DemuxError::StrictRejection(_)
        ),
        "strict feed_aligned should escalate MalformedPes (got {err:?})"
    );
}

/// A valid PES that LENGTH-COMPLETES in the same `push` as a malformed
/// neighbour on the same PID must survive; returning `Err` from
/// `Reassembler::push` instead of its outcomes drops it with the
/// neighbour's error. Both orders, both feed entry points. The helper's two video
/// PESes are unbounded; here the second one is made bounded (its
/// `PES_packet_length` set to the payload remaining in its PUSI packet) so
/// it completes inside that packet's push.
#[test]
fn malformed_pes_preserves_a_valid_adjacent_bounded_completion() {
    for bad_first in [true, false] {
        let (mut bytes, _) = build_stream_with_malformed_pes();
        let mut starts = 0;
        for pkt in bytes.chunks_exact_mut(188) {
            let pid = (u16::from(pkt[1] & 0x1f) << 8) | u16::from(pkt[2]);
            if pid != 0x100 || pkt[1] & 0x40 == 0 {
                continue;
            }
            let afc = (pkt[3] >> 4) & 3;
            let off = if afc & 2 != 0 {
                5 + usize::from(pkt[4])
            } else {
                4
            };
            starts += 1;
            if starts == 1 {
                // The helper already corrupted this byte; restore or keep it.
                pkt[off + 2] = if bad_first { 0xff } else { 1 };
            } else if starts == 2 {
                pkt[off + 2] = if bad_first { 1 } else { 0xff };
                let len = (188 - off - 6) as u16;
                pkt[off + 4..off + 6].copy_from_slice(&len.to_be_bytes());
            }
        }
        assert_eq!(starts, 2, "fixture must carry exactly two video PUSIs");
        for aligned in [false, true] {
            let mut d = Demuxer::new();
            if aligned {
                for chunk in bytes.chunks_exact(188) {
                    d.feed_aligned(chunk.try_into().unwrap()).unwrap();
                }
            } else {
                d.feed(&bytes).unwrap();
            }
            d.flush();
            let (mut malformed, mut video) = (0, 0);
            while let Some(event) = d.next_event() {
                match event {
                    DemuxEvent::NonConformant {
                        issue: NonConformantIssue::MalformedPes { .. },
                        ..
                    } => malformed += 1,
                    DemuxEvent::Sample {
                        payload: SamplePayload::Video { .. },
                        ..
                    } => video += 1,
                    _ => {}
                }
            }
            assert_eq!(malformed, 1, "bad_first={bad_first}, aligned={aligned}");
            assert_eq!(video, 1, "bad_first={bad_first}, aligned={aligned}");
        }
    }
}
