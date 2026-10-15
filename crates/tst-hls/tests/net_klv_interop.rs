//! KLV-over-HLS interop contract: every produced segment must be
//! independently decodable by an HLS client's TS demuxer and carry the KLV
//! stream in the exact shape hls.js parses natively — a dedicated PES PID
//! (stream_type 0x06) tagged with the `KLVA` registration descriptor, whose
//! payloads are bare SMPTE-UL-anchored KLV with monotonic 90 kHz PTS.
//!
//! This is the test that PINS the whole feature's client contract:
//!   (a) segments open PAT → PMT → IDR (independent decodability), and
//!   (b) KLV round-trips byte-identically in the async, UL-anchored shape.
//!
//! Serve-gated because constructing `MuxPublisher<HlsPublisher>` pulls the
//! HTTP server in via `HlsPublisherBuilder`; the test itself never speaks
//! HTTP — it re-demuxes segment files straight off disk.

#![cfg(feature = "serve")]

use std::path::PathBuf;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, StreamKind};
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

/// 16-byte SMPTE Universal Label for a MISB ST 0601 Local Set — the anchor a
/// client slices at. Verified against `classify_klv`'s async sniff, which
/// keys on the first four bytes `06 0E 2B 34`.
const ST0601_UL: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
];

const VIDEO_PID: u16 = 0x100;
const KLV_PID: u16 = 0x101;

fn tmpdir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hls-klv-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A minimal H.264 access unit: AUD NAL (type 9) + IDR-slice NAL (type 5),
/// each Annex-B start-code-prefixed. The exact byte prefix
/// `00 00 00 01 09 10 00 00 00 01 65` is what assertion (6) re-checks at the
/// head of a segment's first video PES payload. Mirrors the shape in
/// `hls_e2e.rs`.
fn synthetic_h264_au() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend(std::iter::repeat(0xab).take(200));
    v
}

/// The known Annex-B head of `synthetic_h264_au`, up to and including the
/// IDR NAL header byte 0x65 — used as the "begins with an IDR AU" witness.
const AU_HEAD: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65,
];

/// One MISB-shaped KLV Local Set: the ST 0601 UL followed by a short BER
/// length + a tiny body. The client shape is "bare UL at offset 0", so this
/// is exactly what the demuxer's async path returns byte-for-byte.
fn synthetic_klv(seq: u8) -> Vec<u8> {
    let mut v = ST0601_UL.to_vec();
    v.push(0x04); // BER short-form length = 4 bytes of value
    v.extend([0x02, 0x01, 0x01, seq]); // Tag 2 (UNIX timestamp stand-in), len 1, then a per-GOP marker
    v
}

/// Parse the 13-bit PID from a raw 188-byte TS packet header.
fn pid_of(packet: &[u8]) -> u16 {
    (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16
}

/// Drain every event a fresh demuxer recovers from one whole segment.
fn demux_segment(bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut demux = Demuxer::new();
    // A segment is a complete, self-contained TS bytestream. feed() + flush()
    // is the standard pull loop (see crates/tst-core demux tests).
    demux.feed(bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }
    events
}

#[test]
fn segments_are_self_contained_and_carry_klv() {
    let dir = tmpdir("interop");

    // Event mode + a long segment_duration so wall-clock cutting never fires:
    // the ONLY cuts are keyframe-driven (one segment per GOP), which is what
    // makes the per-segment PAT→PMT→IDR contract deterministic.
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(3600))
        .playlist_window(16)
        .mode(HlsMode::Event)
        .build()
        .unwrap();

    // One H.264 video stream + one KLV stream carried as PrivateData
    // (stream_type 0x06 + auto-emitted KLVA registration descriptor,
    // carries_pts=true → each KLV PES carries its own PTS). This is the
    // hls.js-native shape.
    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, VideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };

    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg).unwrap();
    let au = synthetic_h264_au();

    // Three GOPs, each = 1 keyframe AU + 2 non-key AUs, PTS step 3003 ticks
    // (~29.97 fps). One KLV LS per GOP, emitted at the keyframe PTS — a
    // plausible telemetry cadence. Keyframes BEGIN segments
    // (cut-before-push), so GOP N opens segment N.
    const GOP: usize = 3;
    const STEP: i64 = 3003;
    let mut sent_klv: Vec<Vec<u8>> = Vec::new();
    let mut sent_klv_pts: Vec<i64> = Vec::new();
    let mut frame = 0i64;
    for gop in 0..GOP {
        let key_pts = frame * STEP;
        // KLV at the keyframe PTS.
        let klv = synthetic_klv(gop as u8);
        pub_shell.send_klv(&klv, Pts90khz::new(key_pts), 0).unwrap();
        sent_klv.push(klv);
        sent_klv_pts.push(key_pts);
        // Keyframe AU, then two non-key AUs.
        pub_shell
            .send_video(&au, Pts90khz::new(key_pts), true)
            .unwrap();
        frame += 1;
        for _ in 0..2 {
            pub_shell
                .send_video(&au, Pts90khz::new(frame * STEP), false)
                .unwrap();
            frame += 1;
        }
    }

    let publisher = pub_shell.finish().unwrap();
    publisher.finish().unwrap();

    // Collect segment files in sequence order.
    let mut segments: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("segment_") && n.ends_with(".ts"))
        })
        .collect();
    segments.sort();
    assert_eq!(
        segments.len(),
        GOP,
        "expected one segment per GOP, got {:?}",
        segments
    );

    // Running accumulators for cross-segment assertions.
    let mut recovered_klv: Vec<Vec<u8>> = Vec::new();
    let mut recovered_klv_pts: Vec<i64> = Vec::new();

    for (seg_idx, seg) in segments.iter().enumerate() {
        let bytes = std::fs::read(seg).unwrap();

        // (1) Framing + independent decodability: the file is whole TS
        //     packets and the FIRST packet is the PAT (PID 0), so a client's
        //     demuxer syncs from byte 0.
        assert_eq!(
            bytes.len() % 188,
            0,
            "segment {seg:?} is not a whole number of 188-byte TS packets"
        );
        assert_eq!(
            pid_of(&bytes[0..188]),
            0,
            "segment must open with PAT (PID 0): {seg:?}"
        );

        let events = demux_segment(&bytes);

        // (5a) Descriptor surface: the demuxer classifies the KLV PID as
        //      KlvAsync ONLY when the PMT carried the KLVA registration
        //      descriptor (see demux::pmt_classify::classify_0x06). Observing
        //      a KlvAsync stream on this PID in the ProgramMap is the
        //      demuxer's own confirmation the descriptor round-tripped.
        let pm = events
            .iter()
            .find_map(|e| match e {
                DemuxEvent::ProgramMap(pm) => Some(pm),
                _ => None,
            })
            .unwrap_or_else(|| panic!("segment {seg:?} carried no PMT"));
        let klv_stream = pm
            .streams
            .iter()
            .find(|s| s.pid == KLV_PID)
            .unwrap_or_else(|| panic!("segment {seg:?} PMT has no KLV PID {KLV_PID:#x}"));
        assert_eq!(
            klv_stream.kind,
            StreamKind::KlvAsync,
            "KLV PID must classify as async KLV (proves KLVA registration \
             descriptor is present): {seg:?}"
        );

        // (5b) Belt-and-suspenders: raw byte-scan for the ASCII "KLVA"
        //      registration format_identifier somewhere in the segment's PSI
        //      (the PMT section). Independent of the demuxer's classification.
        assert!(
            bytes.windows(4).any(|w| w == [0x4B, 0x4C, 0x56, 0x41]),
            "segment {seg:?} PMT is missing the KLVA registration descriptor \
             (ASCII \"KLVA\") on a byte scan"
        );

        // (2) KLV recovery: pull every KlvAsync Metadata event, in order.
        for e in &events {
            if let DemuxEvent::Metadata {
                stream,
                pts,
                payload,
                ..
            } = e
            {
                if stream.pid != KLV_PID {
                    continue;
                }
                // (3) Each payload begins with the 16-byte SMPTE UL prefix —
                //     the client-side slice-at-UL anchor.
                assert!(
                    payload.len() >= 16 && payload[..16] == ST0601_UL,
                    "recovered KLV in {seg:?} does not start with the ST 0601 UL"
                );
                recovered_klv.push(payload.clone());
                recovered_klv_pts.push(pts.as_ticks());
            }
        }

        // (6) Independent decodability of the VIDEO ES: in each non-first
        //     segment (which opens with a keyframe under
        //     cut-before-push), the FIRST recovered video sample is an IDR AU
        //     — witnessed by the synthetic AU's known Annex-B head.
        if seg_idx > 0 {
            let first_video = events
                .iter()
                .find_map(|e| match e {
                    DemuxEvent::Sample {
                        stream,
                        payload: SamplePayload::Video { raw, .. },
                        ..
                    } if stream.pid == VIDEO_PID => Some(raw),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("segment {seg:?} carried no video sample"));
            assert!(
                first_video.starts_with(AU_HEAD),
                "non-first segment {seg:?} does not begin with the IDR AU head"
            );
        }
    }

    // (2) Across ALL segments: total KLV count == pushed count, byte-identical
    //     payloads, in push order.
    assert_eq!(
        recovered_klv.len(),
        sent_klv.len(),
        "recovered KLV count != pushed count"
    );
    for (i, (got, want)) in recovered_klv.iter().zip(sent_klv.iter()).enumerate() {
        assert_eq!(got, want, "KLV #{i} did not round-trip byte-identically");
    }

    // (4) KLV PTS strictly monotonic across segments (and equal to what we
    //     sent, which was itself strictly increasing).
    for w in recovered_klv_pts.windows(2) {
        assert!(
            w[1] > w[0],
            "KLV PTS not strictly monotonic across segments: {:?}",
            recovered_klv_pts
        );
    }
    assert_eq!(
        recovered_klv_pts, sent_klv_pts,
        "recovered KLV PTS values differ from what was sent"
    );
}
