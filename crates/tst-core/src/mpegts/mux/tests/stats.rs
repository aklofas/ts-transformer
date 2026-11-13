//! `MuxerStats` reporting, per-stream counters, `reset_stats`, codec-specific
//! stats discrimination, and PCR/RA correctness.

use super::*;
use crate::mpegts::common::Pts90khz;

#[test]
fn stats_starts_with_per_stream_entries_for_configured_streams() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let m = Muxer::new(cfg).unwrap();
    let st = m.stats();
    assert_eq!(st.ts_packets_emitted, 0);
    assert_eq!(st.ts_bytes_emitted, 0);
    assert_eq!(st.per_stream.len(), 2);
    assert!(st.per_stream.contains_key(&0x100));
    assert!(st.per_stream.contains_key(&0x101));
    assert_eq!(
        st.per_stream[&0x100].stream_type,
        StreamTypeCode::from_byte(0x1B)
    );
    assert_eq!(
        st.per_stream[&0x101].stream_type,
        StreamTypeCode::from_byte(0x06)
    );
    assert_eq!(st.per_stream[&0x100].items, 0);
}

#[test]
fn stats_count_pushed_items_and_pulled_packets() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB, 0xCC];
    m.push_video(nal, Pts90khz::new(0), true).unwrap();
    let klv: &[u8] = &[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ];
    m.push_klv(klv, Pts90khz::new(0), 0x00).unwrap();
    let mut buf = vec![0u8; 64 * 188];
    let n = m.pull(&mut buf);
    let st = m.stats();
    assert_eq!(st.per_stream[&0x100].items, 1);
    assert_eq!(st.per_stream[&0x100].bytes, nal.len() as u64);
    assert_eq!(st.per_stream[&0x101].items, 1);
    assert_eq!(st.per_stream[&0x101].bytes, klv.len() as u64);
    assert_eq!(st.ts_bytes_emitted, n as u64);
    assert_eq!(st.ts_packets_emitted, (n / 188) as u64);
}

#[test]
fn reset_stats_zeros_counters_keeps_entries() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
    m.push_video(nal, Pts90khz::new(0), true).unwrap();
    m.reset_stats();
    let st = m.stats();
    assert_eq!(st.ts_packets_emitted, 0);
    assert_eq!(st.per_stream.len(), 2);
    assert_eq!(st.per_stream[&0x100].items, 0);
    assert_eq!(st.per_stream[&0x100].bytes, 0);
}

#[test]
fn h266_video_per_stream_stats_records_stream_type_0x33() {
    // Exercises the VideoCodec::H266 -> StreamType::H266 mapping arm in
    // Muxer::new's per_stream stats setup (the second of two H266 sites
    // in mux/mod.rs that previously panicked with unimplemented!()).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H266);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let m = Muxer::new(cfg).unwrap();
    let st = m.stats();
    assert_eq!(
        st.per_stream[&0x101].stream_type,
        StreamTypeCode::from_byte(0x33)
    );
}

/// Per H.222.0 V9 §2.4.3.5: "In the PCR_PID the random_access_indicator
/// may only be set to '1' in a transport stream packet containing the PCR
/// fields." Prior code unconditionally set RA=1 on a key-frame's first TS
/// packet whether or not that packet also carried a PCR — emitted when
/// key-frame timing landed between PCR ticks.
///
/// This test pushes two key-frames close enough that the second is not
/// pcr_due, and asserts the second key-frame's first packet either carries
/// a PCR (forced emission) or has RA=0.
#[test]
fn random_access_indicator_only_on_packets_with_pcr() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Synthetic Annex-B H.264 IDR access unit.
    let nal: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21,
        0xff, // start_code + IDR header + filler
    ];

    // First key-frame at PTS=0 — the program has no clock yet, so a PCR is due. This
    // first packet should carry both PCR and RA.
    mux.push_video(nal, Pts90khz::new(0), true).unwrap();
    // Second key-frame at PTS=10ms (= 900 90kHz ticks) — well below the
    // 40ms PCR threshold. PCR is NOT due. After the fix we force PCR
    // emission on PCR_PID + key_frame; the buggy code would set RA=1
    // without a PCR.
    mux.push_video(nal, Pts90khz::new(900), true).unwrap();

    let mut all = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }

    // Walk all PUSI packets on PID 0x1011. Skip the first (it's the
    // first key-frame and has PCR by virtue of being the first).
    let pusi_packets: Vec<&[u8]> = all
        .chunks_exact(188)
        .filter(|p| {
            p[0] == 0x47
                && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x1011
                && (p[1] & 0x40) != 0
        })
        .collect();
    assert!(
        pusi_packets.len() >= 2,
        "expected at least two PUSI packets on video PID, got {}",
        pusi_packets.len(),
    );

    let second = pusi_packets[1];
    // adaptation_field_control: bits 5-4 of byte 3.
    let afc = (second[3] >> 4) & 0b11;
    assert!(
        afc == 0b11 || afc == 0b10,
        "second key-frame packet must carry adaptation field; afc = {afc:#b}",
    );
    let af_length = second[4] as usize;
    // af_length = 0 means just the length byte itself, no flags. With RA we
    // expect a flags byte at byte 5.
    assert!(
        af_length >= 1,
        "second key-frame AF must include flags; len = {af_length}",
    );
    let af_flags = second[5];
    let random_access = (af_flags & 0b0100_0000) != 0;
    let pcr_present = (af_flags & 0b0001_0000) != 0;
    assert!(
        random_access,
        "second key-frame should still indicate random_access (it's an IDR)",
    );
    assert!(
        pcr_present,
        "spec rule: RA on PCR_PID must coincide with PCR — \
         second key-frame has RA but no PCR (af_flags = {af_flags:#b})",
    );
}

#[test]
fn muxer_stats_reports_subtitle_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.add_subtitle(
            0x201,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_subtitle_to(
        SubtitleStreamHandle::pack(0, 0),
        Pts90khz::new(90_000),
        b"x",
    )
    .unwrap();
    let s = mux.stats();
    assert_eq!(s.subtitle_streams_configured, 2);
    let stream_stat = s.per_stream.get(&0x200).unwrap();
    assert_eq!(stream_stat.label.as_deref(), Some("WebVTT-in-TS"));
    assert!(stream_stat.items >= 1);
    let teletext_stat = s.per_stream.get(&0x201).unwrap();
    assert_eq!(teletext_stat.label.as_deref(), Some("DVB-Teletext"));
}

// last_seen is a std-only field (wall clock unavailable under no_std) —
// gate this test to match, same convention as MuxSender::into_inner's tests.
#[cfg(feature = "std")]
#[test]
fn stats_last_seen_stamped_on_push_none_when_unpushed() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.add_audio(0x102, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();

    // Configured but never pushed — last_seen stays None for all three.
    let st0 = m.stats();
    assert_eq!(st0.per_stream[&0x100].last_seen, None);
    assert_eq!(st0.per_stream[&0x101].last_seen, None);
    assert_eq!(st0.per_stream[&0x102].last_seen, None);

    // Push to stream A (video, 0x100) then stream B (KLV, 0x101).
    let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
    m.push_video(nal, Pts90khz::new(0), true).unwrap();
    let klv: &[u8] = &[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ];
    m.push_klv(klv, Pts90khz::new(0), 0x00).unwrap();

    let st = m.stats();
    let a = st.per_stream[&0x100]
        .last_seen
        .expect("stream A was pushed — last_seen must be Some");
    let b = st.per_stream[&0x101]
        .last_seen
        .expect("stream B was pushed — last_seen must be Some");
    assert!(
        a <= b,
        "stream B was pushed after stream A: last_seen must be non-decreasing"
    );

    // Stream C (audio, 0x102) was configured but never pushed — still None.
    assert_eq!(st.per_stream[&0x102].last_seen, None);
}

#[test]
fn muxer_stream_codec_stats_distinguishes_configured_from_unconfigured() {
    // Default MuxerConfig configures PIDs 0x1011 (video) + 0x1031 (KLV).
    // 0x9999 isn't configured, so stream_codec_stats returns None.
    // Configured-but-never-pushed PIDs return Some(Unknown) via the
    // per_stream contains_key fallback — this locks in the eager-
    // population semantic that distinguishes the Muxer accessor from
    // the Demuxer's (where Unknown requires an event to have been
    // emitted on that PID).
    let muxer = Muxer::new(MuxerConfig::default()).expect("muxer");
    assert_eq!(muxer.stream_codec_stats(0x9999), None, "unconfigured PID");
    assert_eq!(
        muxer.stream_codec_stats(0x1011),
        Some(crate::mpegts::stats::StreamCodecStats::Unknown),
        "configured-but-never-pushed PID",
    );
}
