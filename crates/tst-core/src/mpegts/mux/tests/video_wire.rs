//! PES and adaptation-field flags of video pushes, and the AV1 carriage rules.

use super::pes::{PesFlags, PesPtsField, write_pes_header};
use super::*;
use crate::mpegts::common::Pts90khz;

const VIDEO_PID: u16 = 0x101;
const TWO_POW_33: i64 = 1 << 33;

fn one_video_muxer(codec: VideoCodec, av1_carriage: Option<Av1CarriageMode>) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(VIDEO_PID, codec);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    if let Some(mode) = av1_carriage {
        b.av1_carriage(mode);
    }
    Muxer::new(b.build().unwrap()).unwrap()
}

/// The video PID's PES as written: every TS payload byte of that PID from
/// its first payload-unit start on, adaptation fields stripped.
fn video_pes(m: &mut Muxer) -> Vec<u8> {
    let mut buf = vec![0u8; 188 * 64];
    let n = m.pull(&mut buf);
    let mut pes = Vec::new();
    let mut started = false;
    for pkt in buf[..n].chunks_exact(188) {
        if ((u16::from(pkt[1]) & 0x1F) << 8 | u16::from(pkt[2])) != VIDEO_PID {
            continue;
        }
        started |= pkt[1] & 0x40 != 0;
        if !started {
            continue;
        }
        let start = if pkt[3] & 0x20 != 0 {
            5 + usize::from(pkt[4])
        } else {
            4
        };
        pes.extend_from_slice(&pkt[start.min(188)..]);
    }
    assert!(started, "no PES start on the video PID");
    pes
}

fn stamp(bytes: &[u8]) -> u64 {
    (u64::from(bytes[0] >> 1) & 0x07) << 30
        | u64::from(bytes[1]) << 22
        | (u64::from(bytes[2]) >> 1) << 15
        | u64::from(bytes[3]) << 7
        | u64::from(bytes[4]) >> 1
}

const ANNEX_B_AU: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x40, 0x01];

// ── PES flags ─────────────────────────────────────────────────────────────

#[test]
fn hevc_and_vvc_pes_set_data_alignment_indicator() {
    // TS 101 154 §4.1.6.9: an HEVC or VVC PES header sets the bit, for the
    // elementary and the wire form alike.
    for codec in [VideoCodec::H265, VideoCodec::H266] {
        for wire in [false, true] {
            let mut m = one_video_muxer(codec, None);
            let h = m.video_stream_handle(0).unwrap();
            let pts = Pts90khz::new(90_000);
            if wire {
                m.push_video_wire_to(h, &ANNEX_B_AU, pts, true).unwrap();
            } else {
                m.push_video_to(h, &ANNEX_B_AU, pts, true).unwrap();
            }
            let pes = video_pes(&mut m);
            assert_eq!(pes[6], 0x84, "{codec:?} wire={wire}: flags1");
        }
    }
}

#[test]
fn equal_pts_and_dts_encode_pts_only() {
    // H.222.0 §2.7.5: a DTS appears only when it differs from the PTS.
    for wire in [false, true] {
        let mut m = one_video_muxer(VideoCodec::H265, None);
        let h = m.video_stream_handle(0).unwrap();
        let t = Pts90khz::new(90_000);
        if wire {
            m.push_video_wire_to_with_dts(h, &ANNEX_B_AU, t, t, true)
                .unwrap();
        } else {
            m.push_video_to_with_dts(h, &ANNEX_B_AU, t, t, true)
                .unwrap();
        }
        let pes = video_pes(&mut m);
        assert_eq!(&pes[7..9], &[0x80, 0x05], "wire={wire}: PTS only");
        assert_eq!(pes[9] >> 4, 0b0010, "wire={wire}: PTS-only marker");
        assert_eq!(stamp(&pes[9..14]), 90_000);
        // The header is 14 bytes: the access unit follows at once.
        assert_eq!(&pes[14..20], &ANNEX_B_AU, "wire={wire}: payload offset");
    }
}

#[test]
fn wrap_equivalent_pts_and_dts_encode_pts_only() {
    // Equal on the wire is equal modulo 2^33: both values are masked
    // before they are compared.
    for (pts, dts) in [
        (90_000, 90_000 + TWO_POW_33),
        (2 * TWO_POW_33 + 90_000, TWO_POW_33 + 90_000),
    ] {
        let mut m = one_video_muxer(VideoCodec::H265, None);
        let h = m.video_stream_handle(0).unwrap();
        m.push_video_to_with_dts(h, &ANNEX_B_AU, Pts90khz::new(pts), Pts90khz::new(dts), true)
            .unwrap();
        let pes = video_pes(&mut m);
        assert_eq!(&pes[7..9], &[0x80, 0x05], "pts={pts} dts={dts}");
        assert_eq!(stamp(&pes[9..14]), 90_000);
    }
}

#[test]
fn unequal_pts_and_dts_keep_both_stamps() {
    for (pts, dts) in [(200_000, 100_000), (90_001, 90_000)] {
        let mut m = one_video_muxer(VideoCodec::H265, None);
        let h = m.video_stream_handle(0).unwrap();
        m.push_video_to_with_dts(h, &ANNEX_B_AU, Pts90khz::new(pts), Pts90khz::new(dts), true)
            .unwrap();
        let pes = video_pes(&mut m);
        assert_eq!(&pes[7..9], &[0xC0, 0x0A], "pts={pts} dts={dts}");
        assert_eq!(stamp(&pes[9..14]), pts as u64);
        assert_eq!(stamp(&pes[14..19]), dts as u64);
    }
}

#[test]
fn the_pes_packet_length_matches_the_header_written() {
    // A video PES is unbounded (length 0), so the bounded length is read
    // on the writer itself: it must count the stamp bytes it wrote.
    let t = Pts90khz::new(90_000);
    let mut out = [0u8; 32];
    let written = write_pes_header(
        &mut out,
        0xE0,
        PesPtsField::PtsAndDts { pts: t, dts: t },
        Some(100),
        PesFlags {
            data_alignment_indicator: true,
        },
    );
    assert_eq!(written, 14);
    assert_eq!(u16::from_be_bytes([out[4], out[5]]), 3 + 5 + 100);
    assert_eq!(out[8], 5);
}

// ── AV1 elementary input ──────────────────────────────────────────────────

/// A temporal delimiter (`12 00`) then a sequence-header OBU of four body
/// bytes (`32 04 ..`): two whole OBUs with a size field each.
const TWO_OBU_AU: [u8; 8] = [0x12, 0x00, 0x32, 0x04, 0x10, 0x00, 0x00, 0x00];

#[test]
fn av1_binding_rejects_every_truncated_prefix() {
    let mut refused: Vec<Vec<u8>> = [1, 3, 4, 5, 6, 7]
        .iter()
        .map(|&cut| TWO_OBU_AU[..cut].to_vec())
        .collect();
    refused.extend(
        [
            &[0x0A][..],         // a header whose size byte is missing
            &[0x0A, 0x01],       // a size of 1 and no body
            &[0x16],             // the extension flag and no extension byte
            &[0x10],             // no size field
            &[0x12, 0x80],       // a size that continues past the end
            &[0x12, 0x00, 0x00], // one stray zero byte after a whole OBU
        ]
        .map(<[u8]>::to_vec),
    );
    for au in refused {
        let mut m = one_video_muxer(VideoCodec::Av1, None);
        let h = m.video_stream_handle(0).unwrap();
        m.push_video_to(h, &TWO_OBU_AU, Pts90khz::new(90_000), true)
            .unwrap();
        let pending = m.pending_packets();
        let stats = m.stream_codec_stats(VIDEO_PID);
        assert_eq!(
            m.push_video_to(h, &au, Pts90khz::new(93_600), true),
            Err(MuxError::InvalidAv1Obu),
            "{au:02x?}"
        );
        assert_eq!(m.pending_packets(), pending, "{au:02x?}: nothing queued");
        assert_eq!(m.stream_codec_stats(VIDEO_PID), stats, "{au:02x?}");
    }
}

#[test]
fn av1_binding_accepts_whole_obus() {
    for cut in [2, 8] {
        let mut m = one_video_muxer(VideoCodec::Av1, None);
        let h = m.video_stream_handle(0).unwrap();
        m.push_video_to(h, &TWO_OBU_AU[..cut], Pts90khz::new(90_000), true)
            .unwrap();
    }
}

#[test]
fn av1_binding_never_panics_on_arbitrary_bytes() {
    let mut m = one_video_muxer(VideoCodec::Av1, None);
    let h = m.video_stream_handle(0).unwrap();
    let mut buf = vec![0u8; 188 * 64];
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut accepted, mut refused) = (0u32, 0u32);
    for i in 0..10_000i64 {
        let len = (next() % 65) as usize;
        let au: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        match m.push_video_to(h, &au, Pts90khz::new(90_000 + i * 3_600), false) {
            Ok(()) => accepted += 1,
            Err(MuxError::InvalidAv1Obu) => refused += 1,
            Err(e) => panic!("{au:02x?}: {e:?}"),
        }
        while m.pull(&mut buf) > 0 {}
    }
    assert!(accepted > 0 && refused > 0, "{accepted} / {refused}");
}
