//! PES and adaptation-field flags of video pushes, and the AV1 carriage rules.

use super::pes::{PesFlags, PesPtsField, write_pes_header};
use super::*;
use crate::mpegts::common::Pts90khz;
use crate::mpegts::stats::StreamCodecStats;

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

/// A temporal delimiter (`12 00`) then a frame OBU of four body bytes
/// (`32 04 ..`): two whole OBUs with a size field each.
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

// ── The OBU statistic of a wire push ──────────────────────────────────────

fn obus_counted(m: &Muxer) -> u64 {
    match m.stream_codec_stats(VIDEO_PID) {
        Some(StreamCodecStats::Video { nals_or_obus, .. }) => nals_or_obus,
        other => panic!("no video statistics: {other:?}"),
    }
}

#[test]
fn av1_binding_wire_push_counts_ts_obus() {
    // The binding's framing: one `00 00 01` per OBU. A whole OBU that ends
    // in a zero byte puts `00 00 00 01` on the wire: still one start code.
    let two = [
        0x00, 0x00, 0x01, 0x12, 0x00, 0x00, 0x00, 0x01, 0x32, 0x04, 0x10, 0x00, 0x00, 0x03, 0x00,
    ];
    let five: Vec<u8> = [0x00, 0x00, 0x01, 0x12, 0x00].repeat(5);
    for (wire, obus) in [(&two[..], 2), (&five[..], 5)] {
        let mut m = one_video_muxer(VideoCodec::Av1, None);
        let h = m.video_stream_handle(0).unwrap();
        m.push_video_wire_to(h, wire, Pts90khz::new(90_000), true)
            .unwrap();
        assert_eq!(obus_counted(&m), obus, "{wire:02x?}");
    }

    // Wire bytes without the framing count no OBU and are still sent.
    let mut m = one_video_muxer(VideoCodec::Av1, None);
    let h = m.video_stream_handle(0).unwrap();
    m.push_video_wire_to(h, &TWO_OBU_AU, Pts90khz::new(90_000), true)
        .unwrap();
    assert_eq!(obus_counted(&m), 0);
    let pes = video_pes(&mut m);
    assert_eq!(&pes[14..22], &TWO_OBU_AU);
}

#[test]
fn av1_interop_raw_obu_pushes_count_walked_obus() {
    // Raw OBUs carry no start codes: the count walks their size fields,
    // for the wire and the elementary form alike.
    for wire in [false, true] {
        let mut m = one_video_muxer(VideoCodec::Av1, Some(Av1CarriageMode::InteropRawObu));
        let h = m.video_stream_handle(0).unwrap();
        let pts = Pts90khz::new(90_000);
        if wire {
            m.push_video_wire_to(h, &TWO_OBU_AU, pts, true).unwrap();
        } else {
            m.push_video_to(h, &TWO_OBU_AU, pts, true).unwrap();
        }
        assert_eq!(obus_counted(&m), 2, "wire={wire}");
    }
}

// ── ES priority on AV1 key frames ─────────────────────────────────────────

/// The first TS packet of the video PID's PES.
fn first_video_packet(m: &mut Muxer) -> [u8; 188] {
    let mut buf = vec![0u8; 188 * 64];
    let n = m.pull(&mut buf);
    let pkt = buf[..n]
        .chunks_exact(188)
        .find(|p| {
            ((u16::from(p[1]) & 0x1F) << 8 | u16::from(p[2])) == VIDEO_PID && p[1] & 0x40 != 0
        })
        .expect("no PES start on the video PID");
    pkt.try_into().unwrap()
}

/// An AV1 muxer whose video PID is the PCR PID (an 8-byte adaptation field
/// on a key frame), or one whose PCR is on a second stream (2 bytes).
fn av1_muxer(carriage: Av1CarriageMode, video_is_pcr_pid: bool) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(VIDEO_PID, VideoCodec::Av1);
    if !video_is_pcr_pid {
        prog.add_video(0x102, VideoCodec::H264);
        prog.pcr_pid(0x102);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.av1_carriage(carriage);
    Muxer::new(b.build().unwrap()).unwrap()
}

/// Push `au` as a key frame (a PES header of 14 bytes, or of 19 with a
/// DTS) and return the adaptation-field flags of its first packet.
fn key_frame_flags(m: &mut Muxer, au: &[u8], wire: bool, with_dts: bool) -> u8 {
    let h = m.video_stream_handle(0).unwrap();
    let pts = Pts90khz::new(93_600);
    let dts = Pts90khz::new(90_000);
    match (wire, with_dts) {
        (false, false) => m.push_video_to(h, au, pts, true),
        (false, true) => m.push_video_to_with_dts(h, au, pts, dts, true),
        (true, false) => m.push_video_wire_to(h, au, pts, true),
        (true, true) => m.push_video_wire_to_with_dts(h, au, pts, dts, true),
    }
    .unwrap();
    let pkt = first_video_packet(m);
    assert_ne!(pkt[3] & 0x20, 0, "the first packet has an adaptation field");
    pkt[5]
}

/// A temporal delimiter, then a frame-header OBU (type 3) and a tile group.
const FRAME_HEADER_AU: [u8; 8] = [0x12, 0x00, 0x1A, 0x01, 0xAA, 0x22, 0x01, 0xAA];
/// A temporal delimiter and a sequence header: no frame data at all.
const NO_FRAME_AU: [u8; 5] = [0x12, 0x00, 0x0A, 0x01, 0xAA];

/// An elementary AU whose binding-framed form has the start code of its
/// frame OBU at `offset`: a padding OBU of the right size, then a frame.
fn au_with_frame_start_at(offset: usize) -> Vec<u8> {
    // Framed: `00 00 01`, the padding OBU's header, its LEB128 size, its
    // body, then `00 00 01 32 01 AA`.
    let size = if offset - 5 < 0x80 {
        offset - 5
    } else {
        offset - 6
    };
    let mut au = vec![0x7A];
    if size < 0x80 {
        au.push(size as u8);
    } else {
        au.extend([0x80 | (size & 0x7F) as u8, (size >> 7) as u8]);
    }
    au.extend(core::iter::repeat_n(0xAA, size));
    au.extend([0x32, 0x01, 0xAA]);
    au
}

#[test]
fn av1_binding_key_frame_sets_es_priority_with_rai() {
    // The AV1 MPEG-2 TS binding: the adaptation field that carries the
    // random access indicator of a key frame also sets
    // elementary_stream_priority_indicator. Here with the PCR as well.
    for au in [&TWO_OBU_AU[..], &FRAME_HEADER_AU] {
        let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
        let flags = key_frame_flags(&mut m, au, false, false);
        assert_eq!(flags & 0x60, 0x60, "{au:02x?}");
        assert_eq!(flags, 0x70, "{au:02x?}");
    }
}

#[test]
fn av1_non_key_frame_does_not_set_es_priority() {
    let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
    let h = m.video_stream_handle(0).unwrap();
    m.push_video_to(h, &TWO_OBU_AU, Pts90khz::new(90_000), false)
        .unwrap();
    assert_eq!(first_video_packet(&mut m)[5], 0x10);
}

#[test]
fn av1_interop_mode_never_sets_es_priority() {
    // Raw OBUs carry no start codes; this unit's metadata body happens to
    // hold `00 00 01 32`, which only the binding's framing would mean.
    let au = [
        0x12, 0x00, 0x2A, 0x04, 0x00, 0x00, 0x01, 0x32, 0x32, 0x01, 0xAA,
    ];
    for wire in [false, true] {
        let mut m = av1_muxer(Av1CarriageMode::InteropRawObu, true);
        assert_eq!(
            key_frame_flags(&mut m, &au, wire, false),
            0x50,
            "wire={wire}"
        );
    }
}

/// The first packet's payload room for the PES: 184 bytes less the
/// adaptation field (8 with a PCR, 2 without) and the PES header.
fn first_packet_window(video_is_pcr_pid: bool, with_dts: bool) -> usize {
    184 - if video_is_pcr_pid { 8 } else { 2 } - if with_dts { 19 } else { 14 }
}

#[test]
fn av1_key_frame_whose_frame_obu_start_code_ends_the_first_packet_sets_es_priority() {
    for video_is_pcr_pid in [true, false] {
        for with_dts in [false, true] {
            let window = first_packet_window(video_is_pcr_pid, with_dts);
            let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, video_is_pcr_pid);
            let au = au_with_frame_start_at(window - 3);
            let flags = key_frame_flags(&mut m, &au, false, with_dts);
            assert_eq!(flags & 0x20, 0x20, "pcr={video_is_pcr_pid} dts={with_dts}");
        }
    }
}

#[test]
fn av1_key_frame_whose_frame_obu_starts_beyond_the_first_packet_does_not_set_es_priority() {
    // One byte later the start code straddles two packets; further on it
    // lies wholly in the second. The bit is only ever on the first packet.
    for video_is_pcr_pid in [true, false] {
        for with_dts in [false, true] {
            let window = first_packet_window(video_is_pcr_pid, with_dts);
            for offset in [window - 2, window + 40] {
                let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, video_is_pcr_pid);
                let au = au_with_frame_start_at(offset);
                let flags = key_frame_flags(&mut m, &au, false, with_dts);
                assert_eq!(
                    flags & 0x60,
                    0x40,
                    "pcr={video_is_pcr_pid} dts={with_dts} at {offset}"
                );
            }
        }
    }
}

#[test]
fn av1_key_frame_flag_without_a_frame_obu_does_not_set_es_priority() {
    let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
    assert_eq!(key_frame_flags(&mut m, &NO_FRAME_AU, false, false), 0x50);
}

#[test]
fn av1_wire_key_frame_follows_the_same_rule() {
    let window = first_packet_window(true, false);
    let mut beyond = vec![0x00, 0x00, 0x01, 0x7A, 0x80 | 0x7F, 0x01];
    beyond.extend(core::iter::repeat_n(0xAA, 0xFF));
    beyond.extend([0x00, 0x00, 0x01, 0x32, 0x01, 0xAA]);
    assert!(beyond.len() - 6 > window);
    for (wire, set) in [
        (&[0x00, 0x00, 0x01, 0x32, 0x01, 0xAA][..], true),
        (
            &[
                0x00, 0x00, 0x01, 0x12, 0x00, 0x00, 0x00, 0x01, 0x1A, 0x01, 0xAA,
            ],
            true,
        ),
        (
            &[
                0x00, 0x00, 0x01, 0x12, 0x00, 0x00, 0x00, 0x01, 0x0A, 0x01, 0xAA,
            ],
            false,
        ),
        (&beyond, false),
    ] {
        let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
        let flags = key_frame_flags(&mut m, wire, true, false);
        assert_eq!(flags, if set { 0x70 } else { 0x50 }, "{wire:02x?}");
    }
}

#[test]
fn av1_key_frame_shorter_than_three_bytes_does_not_set_es_priority() {
    for wire in [&[][..], &[0x00], &[0x00, 0x00], &[0x00, 0x01]] {
        let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
        assert_eq!(
            key_frame_flags(&mut m, wire, true, false),
            0x50,
            "{wire:02x?}"
        );
    }
    // A two-byte temporal delimiter is framed to five bytes, still no frame.
    let mut m = av1_muxer(Av1CarriageMode::Mpeg2TsBinding, true);
    assert_eq!(key_frame_flags(&mut m, &[0x12, 0x00], false, false), 0x50);
}
