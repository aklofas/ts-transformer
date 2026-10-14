//! Round-trip integrity: mux→demux subtitle PES bytes must come
//! back identical.

use crate::common::mux_demux::drain_mux as drain_all;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSub, VideoCodec,
};

/// Push `payload` through a subtitle PID 0x200, demux, and return the
/// payload bytes from the first `SamplePayload::Subtitle` event. Returns
/// an empty Vec if no Sample event surfaces (e.g. empty PES dropped).
fn round_trip(codec: MuxSub, payload: &[u8]) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, codec);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), payload)
        .unwrap();
    let bytes = drain_all(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut out: Option<Vec<u8>> = None;
    while let Some(e) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Subtitle { payload, .. },
            ..
        } = e
        {
            out = Some(payload.to_vec());
            break;
        }
    }
    out.unwrap_or_default()
}

#[test]
fn roundtrip_webvtt_payload_byte_identical() {
    let input = b"WEBVTT\n\n00:00:01.000 --> 00:00:05.000\nhello world\n".to_vec();
    let output = round_trip(MuxSub::WebVttInTs, &input);
    assert_eq!(input, output);
}

#[test]
fn roundtrip_dvb_subtitling_payload_byte_identical() {
    // Caller supplies raw subtitling_segment bytes (per EN 300 743 §7.2.2,
    // each starting with sync_byte 0x0F); the muxer auto-prepends the
    // §6.2 PES_data_field envelope (data_identifier=0x20 +
    // subtitle_stream_id=0x00 + segments + end_of_PES_data_field_marker=0xFF).
    // The demuxer strips that envelope before surfacing, so the round-tripped
    // payload is byte-identical to what the caller pushed.
    let segments = vec![0x0F, 0x10, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10];

    let output = round_trip(
        MuxSub::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
        &segments,
    );
    assert_eq!(segments, output);
}

#[test]
fn roundtrip_dvb_teletext_payload_prefixes_pes_with_tail_stuffing() {
    // EN 300 472 §4.2 mandates a 45-byte stuffed PES header and a PES that
    // is exactly N×184 bytes long. Per EN 300 472 §4.4, the muxer pads the
    // tail with spec-conformant stuffing_data_units ([0xFF, 0x2C, 0x00×44]
    // = 46 bytes each), not raw 0xFF bytes.
    //
    // The demuxer surfaces everything after the 45-byte header verbatim
    // (no envelope stripping for teletext), so output = input + stuffing tail.
    //
    // Input starts with 0x10 (in 0x10..=0x1F range) so no auto-prepend fires.
    // total useful = 45 + 7 = 52; N = ceil(52/184) = 1; total PES = 184 bytes;
    // tail = 184 − 52 = 132 bytes = 2×46-byte units + 1×40-byte partial unit.
    let input = vec![0x10, 0x02, 0x10, 0xFC, 0x40, 0x40, 0x80];
    let output = round_trip(
        MuxSub::DvbTeletext {
            language: *b"eng",
            teletext_type: 0x02,
            magazine_number: 0,
            page_number: 0x88,
        },
        &input,
    );
    // Prefix must be exactly the caller's payload.
    assert_eq!(&output[..input.len()], &input[..]);
    // PES is exactly N×184 bytes; demuxer surfaces (N×184 − 45) bytes of body.
    let body_len = output.len();
    let total = body_len + 45;
    assert_eq!(
        total % 184,
        0,
        "demuxed body length implies non-conformant PES total length"
    );
    // Tail must be spec-conformant stuffing_data_units per EN 300 472 §4.4.
    // Two whole units (46 bytes each) followed by one partial unit (40 bytes):
    // [0xFF, length=38=0x26, 0x00×38].
    let tail = &output[input.len()..];
    assert_eq!(tail.len(), 132, "tail must be 132 bytes");
    // First two whole stuffing_data_units.
    for unit_idx in 0..2usize {
        let base = unit_idx * 46;
        assert_eq!(tail[base], 0xFF, "unit[{}] data_unit_id", unit_idx);
        assert_eq!(
            tail[base + 1],
            0x2C,
            "unit[{}] data_unit_length=44",
            unit_idx
        );
        assert!(
            tail[base + 2..base + 46].iter().all(|&b| b == 0x00),
            "unit[{}] padding",
            unit_idx
        );
    }
    // Partial stuffing_data_unit at the end: 40 bytes [0xFF, 0x26, 0x00×38].
    let partial = &tail[92..];
    assert_eq!(partial.len(), 40);
    assert_eq!(partial[0], 0xFF, "partial unit data_unit_id");
    assert_eq!(partial[1], 0x26, "partial unit data_unit_length=38");
    assert!(
        partial[2..].iter().all(|&b| b == 0x00),
        "partial unit padding"
    );
}

#[test]
fn roundtrip_cea708_standalone_payload_byte_identical() {
    let input = vec![0xFE, 0x80, 0x80, 0x80, 0xFC, 0x80, 0x80, 0x80];
    let output = round_trip(MuxSub::Cea708Standalone, &input);
    assert_eq!(input, output);
}

#[test]
fn roundtrip_empty_payload_accepted() {
    let output = round_trip(MuxSub::WebVttInTs, &[]);
    assert!(output.is_empty());
}
