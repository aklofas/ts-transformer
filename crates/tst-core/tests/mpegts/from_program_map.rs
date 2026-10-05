//! `MuxerConfig::from_program_map` — rebuilding a single-program muxer
//! config from a demuxed `ProgramMap`.
//!
//! Covers the strict-by-default offender contract (DVB-subtitle streams
//! error unless their kinds are passed in `drop`), the Unknown→Data
//! pass-through mapping (PMT descriptors preserved verbatim, `carries_pts`
//! always true), the PCR copy rule (the demuxed `pcr_pid` is copied only
//! when it lands on a kept video or audio stream — KLV, data, and
//! subtitle PIDs are PCR-ineligible),
//! exact descriptor preservation for all typed stream kinds,
//! and real mux→demux→`from_program_map` round-trips.

use tst_core::MuxError;
use tst_core::mpegts::common::{Pts90khz, StreamTypeCode};
use tst_core::mpegts::demux::{
    AudioCodec as DemuxAudio, DemuxEvent, Demuxer, ProgramMap, StreamInfo, StreamKind as DemuxKind,
    StreamKindTag, SubtitleCodec as DemuxSub, VideoCodec as DemuxVideo,
};
use tst_core::mpegts::descriptors::RawDescriptor;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, StreamSpec,
    SubtitleCodec, VideoCodec,
};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// `StreamInfo` literal with no descriptors (program 1).
fn stream(pid: u16, stream_type: u8, kind: DemuxKind) -> StreamInfo {
    StreamInfo {
        pid,
        stream_type: StreamTypeCode::from_byte(stream_type),
        kind,
        program_number: 1,
        raw_descriptors: Vec::new(),
    }
}

/// `ProgramMap` literal for program 1 with no KLV links.
fn pm(pmt_pid: u16, pcr_pid: u16, streams: Vec<StreamInfo>) -> ProgramMap {
    ProgramMap {
        program_number: 1,
        pcr_pid,
        pmt_pid,
        streams,
        klv_links: Vec::new(),
    }
}

/// PID of a mux-side `StreamSpec` regardless of variant.
fn spec_pid(s: &StreamSpec) -> u16 {
    match s {
        StreamSpec::Video { pid, .. }
        | StreamSpec::Klv { pid, .. }
        | StreamSpec::Audio { pid, .. }
        | StreamSpec::Subtitle { pid, .. }
        | StreamSpec::Data { pid, .. } => *pid,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversion matrix (synthetic ProgramMap literals)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn happy_path_video_sync_klv_audio_with_language() {
    let mut audio = stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    audio.raw_descriptors = vec![RawDescriptor {
        tag: 0x0A,
        data: b"eng\x00".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(
                0x102,
                0x15,
                DemuxKind::KlvSync {
                    declared_link: Some(0x101),
                },
            ),
            audio,
        ],
    );

    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("fully representable program map");
    assert_eq!(cfg.programs.len(), 1, "single-program conversion");
    let prog = &cfg.programs[0];
    assert_eq!(prog.program_number, 1);
    assert_eq!(prog.pmt_pid, 0x100);
    // Demuxed pcr_pid lands on the (kept, non-KLV) video stream → copied.
    assert_eq!(prog.pcr_pid, Some(0x101));
    assert_eq!(prog.streams.len(), 3);
    assert!(prog.streams.contains(&StreamSpec::Video {
        pid: 0x101,
        codec: VideoCodec::H264,
    }));
    assert!(prog.streams.contains(&StreamSpec::Klv {
        pid: 0x102,
        stream_type: KlvStreamType::SynchronousMetadata,
        carries_pts: true,
    }));
    // Descriptors are preserved verbatim for all typed kinds:
    // add_audio is used (language: None) and the 0x0A descriptor passes
    // through stream_descriptors_for_audio instead of the language field.
    assert!(prog.streams.contains(&StreamSpec::Audio {
        pid: 0x103,
        codec: AudioCodec::Aac,
        language: None,
    }));
    let audio_idx = prog
        .streams
        .iter()
        .position(|s| spec_pid(s) == 0x103)
        .unwrap();
    assert_eq!(
        prog.stream_descriptors[audio_idx],
        vec![vec![0x0Au8, 0x04, b'e', b'n', b'g', 0x00]],
        "ISO-639 0x0A descriptor preserved verbatim in stream_descriptors"
    );
}

#[test]
fn async_klv_maps_to_private_data_with_pts() {
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x102, 0x06, DemuxKind::KlvAsync),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("async KLV is representable");
    // carries_pts defaults to true: it is a PES-level property the PMT
    // cannot declare, and PTS-carrying KLV is the STANAG 4609 norm.
    assert!(cfg.programs[0].streams.contains(&StreamSpec::Klv {
        pid: 0x102,
        stream_type: KlvStreamType::PrivateData,
        carries_pts: true,
    }));
}

#[test]
fn unknown_stream_maps_to_data_spec_unless_dropped() {
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x104, 0xC0, DemuxKind::Unknown(0xC0)),
        ],
    );

    // Deliberate pre-1.0 behavior change (private-data W2): unknown stream
    // types are no longer offenders — they map to `StreamSpec::Data` PES
    // pass-through streams keeping the raw stream_type byte.
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("unknown streams now map to Data specs");
    let prog = &cfg.programs[0];
    assert!(prog.streams.contains(&StreamSpec::Data {
        pid: 0x104,
        stream_type: 0xC0,
        carries_pts: true,
    }));
    // The source stream declared no descriptors; none may be invented.
    let i = prog
        .streams
        .iter()
        .position(|s| spec_pid(s) == 0x104)
        .unwrap();
    assert!(prog.stream_descriptors[i].is_empty());

    // Opt-out via `drop` still excludes the stream entirely.
    let cfg = MuxerConfig::from_program_map(&p, &[StreamKindTag::Unknown])
        .expect("dropping Unknown excludes the stream");
    let prog = &cfg.programs[0];
    assert!(
        prog.streams.iter().all(|s| spec_pid(s) != 0x104),
        "dropped stream must be absent"
    );
    assert_eq!(prog.streams.len(), 1, "only the video stream remains");
}

#[test]
fn pcr_on_unknown_pid_falls_back() {
    // pcr_pid lands on the data PID. Data streams are PCR-ineligible
    // (caller-paced pushes have no cadence guarantee, and validate rejects
    // explicit PCR-on-data), so the conversion leaves pcr_pid None — the
    // builder default (first video) applies at validate time.
    let p = pm(
        0x100,
        0x104,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x104, 0xF0, DemuxKind::Unknown(0xF0)),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("PCR-on-data falls back, not errors");
    assert_eq!(cfg.programs[0].pcr_pid, None);
}

#[test]
fn pcr_on_subtitle_pid_falls_back() {
    // pcr_pid lands on the kept CEA-708 subtitle PID. Subtitles must not
    // carry PCR (ETSI EN 300 472 §4.0 / EN 300 743 §6.1 — same rationale
    // as KLV/data; validate rejects explicit PCR-on-subtitle), so the
    // conversion leaves pcr_pid None — the builder default (first video)
    // applies at validate time.
    let p = pm(
        0x100,
        0x105,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::Cea708Standalone)),
        ],
    );
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("PCR-on-subtitle falls back, not errors");
    assert_eq!(cfg.programs[0].pcr_pid, None);
}

#[test]
fn oversized_descriptor_on_unknown_stream_errors() {
    // Only a hand-built ProgramMap can carry a >255-byte descriptor body
    // (PMT parsing is bounded by the one-byte length field). Re-encoding
    // it would truncate the TLV length byte, so the conversion errors
    // naming the pid instead.
    let mut unknown = stream(0x104, 0xF0, DemuxKind::Unknown(0xF0));
    unknown.raw_descriptors = vec![RawDescriptor {
        tag: 0xFF,
        data: vec![0u8; 256],
    }];
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            unknown,
        ],
    );
    match MuxerConfig::from_program_map(&p, &[]) {
        Err(MuxError::ConfigInvalid { reason }) => {
            assert!(reason.contains("0x0104"), "reason names the pid: {reason}");
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn dvb_subtitle_errors_cea708_and_webvtt_map() {
    // DVB subtitling: per-stream parameters (language, page IDs) are not
    // recoverable from the PMT entry alone → offender.
    let dvb = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::DvbSubtitling)),
        ],
    );
    match MuxerConfig::from_program_map(&dvb, &[]) {
        Err(MuxError::ConfigInvalid { reason }) => {
            assert!(reason.contains("0x0105"), "reason names the pid: {reason}");
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }

    // CEA-708 standalone: parameter-free demux variant maps to the
    // parameter-free mux variant.
    let cea = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::Cea708Standalone)),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&cea, &[]).expect("CEA-708 is representable");
    assert!(cfg.programs[0].streams.contains(&StreamSpec::Subtitle {
        pid: 0x105,
        codec: SubtitleCodec::Cea708Standalone,
    }));

    // WebVTT-in-TS: likewise parameter-free → mapped.
    let vtt = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::WebVttInTs)),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&vtt, &[]).expect("WebVTT is representable");
    assert!(cfg.programs[0].streams.contains(&StreamSpec::Subtitle {
        pid: 0x105,
        codec: SubtitleCodec::WebVttInTs,
    }));
}

#[test]
fn drop_video_from_audio_klv_program_validates_with_audio_pcr() {
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac)),
            stream(
                0x102,
                0x15,
                DemuxKind::KlvSync {
                    declared_link: None,
                },
            ),
        ],
    );
    // Dropping the video also drops the demuxed PCR carrier, so the rebuilt
    // program has `pcr_pid: None`. The PCR fallback (first video → first
    // audio; KLV is never auto-selected) resolves to the audio PID 0x103 —
    // the program validates instead of failing for want of a PCR PID.
    let cfg =
        MuxerConfig::from_program_map(&p, &[StreamKindTag::Video]).expect("audio PCR resolves");
    let prog = &cfg.programs[0];
    assert!(
        prog.streams.iter().all(|s| spec_pid(s) != 0x101),
        "dropped video PID 0x101 must be absent"
    );
    // pcr_pid is None (the demuxed PCR was on the dropped video PID);
    // the fallback must pick the audio PID 0x103. Confirm the audio stream
    // is present and is the only non-KLV stream remaining.
    assert_eq!(prog.pcr_pid, None);
    let audio_pid = prog
        .streams
        .iter()
        .find_map(|s| match s {
            StreamSpec::Audio { pid, .. } => Some(*pid),
            _ => None,
        })
        .expect("audio stream must be present after drop");
    assert_eq!(
        audio_pid, 0x103,
        "audio PID must be 0x103 (fallback PCR carrier)"
    );
}

#[test]
fn klv_only_program_has_no_pcr_eligible_stream() {
    // A program with only a KLV stream (no video or audio) has no
    // PCR-eligible carrier — KLV cadence cannot promise the ETSI TR 101 290
    // §5.6.1 100 ms ceiling. Validate rejects it with NoPcrEligibleStream.
    let p = pm(
        0x100,
        0x102,
        vec![stream(
            0x102,
            0x15,
            DemuxKind::KlvSync {
                declared_link: None,
            },
        )],
    );
    match MuxerConfig::from_program_map(&p, &[]) {
        Err(MuxError::NoPcrEligibleStream { program_number }) => {
            assert_eq!(program_number, 1);
        }
        other => panic!("expected NoPcrEligibleStream, got {other:?}"),
    }
}

#[test]
fn drop_video_from_audio_only_program_validates_with_pcr_none() {
    // Without a KLV stream the default-PCR fallback reaches the audio
    // stream, so dropping the video yields a valid audio-only program.
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac)),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[StreamKindTag::Video])
        .expect("audio-only program resolves PCR to the audio stream");
    let prog = &cfg.programs[0];
    assert!(
        prog.streams.iter().all(|s| spec_pid(s) != 0x101),
        "dropped video must be absent"
    );
    // The demuxed pcr_pid (the dropped video PID) is not on any kept
    // stream → not copied.
    assert_eq!(prog.pcr_pid, None);
}

#[test]
fn pcr_pid_not_on_any_stream_is_not_copied() {
    // pcr_pid 0x1FFF (the null-packet PID — some encoders declare it when
    // no PCR is carried in-program) matches no stream → leave None.
    let p = pm(
        0x100,
        0x1FFF,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac)),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("unmatched pcr_pid is not an error");
    assert_eq!(cfg.programs[0].pcr_pid, None);
}

#[test]
fn pcr_on_klv_pid_falls_back_to_default() {
    // Explicit PCR-on-KLV is rejected by MuxerConfig::validate, so the
    // conversion leaves pcr_pid None — the builder default (first video)
    // applies and the program validates.
    let p = pm(
        0x100,
        0x102,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            stream(
                0x102,
                0x15,
                DemuxKind::KlvSync {
                    declared_link: None,
                },
            ),
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("PCR-on-KLV falls back, not errors");
    assert_eq!(cfg.programs[0].pcr_pid, None);
}

#[test]
fn all_streams_dropped_is_an_error() {
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264))],
    );
    // Dropping every stream leaves an empty program; the builder's existing
    // validation rejects it (don't over-pin the exact variant).
    assert!(
        MuxerConfig::from_program_map(&p, &[StreamKindTag::Video]).is_err(),
        "empty program must not validate"
    );
}

#[test]
fn audio_descriptors_always_language_none_verbatim_pass_through() {
    // Descriptors are always preserved verbatim:
    // from_program_map uses add_audio (language: None) for every audio
    // stream, regardless of what the 0x0A descriptor contains. Both of
    // these streams have language: None in the stream spec; their raw
    // PMT descriptors pass through verbatim in stream_descriptors_for_audio.
    let mut short = stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    // Tag 0x0A with fewer than 3 data bytes (a truncated/malformed descriptor):
    // still passes through verbatim — the conversion never inspects the body.
    short.raw_descriptors = vec![RawDescriptor {
        tag: 0x0A,
        data: vec![b'e', b'n'],
    }];
    // Uppercase code: passes through verbatim too (no normalization).
    let mut upper = stream(0x104, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    upper.raw_descriptors = vec![RawDescriptor {
        tag: 0x0A,
        data: b"ENG\x00".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![
            stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)),
            short,
            upper,
        ],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[])
        .expect("any 0x0A descriptor passes through verbatim");
    let prog = &cfg.programs[0];
    // Both audio streams have language: None (add_audio always used).
    assert!(prog.streams.contains(&StreamSpec::Audio {
        pid: 0x103,
        codec: AudioCodec::Aac,
        language: None,
    }));
    assert!(prog.streams.contains(&StreamSpec::Audio {
        pid: 0x104,
        codec: AudioCodec::Aac,
        language: None,
    }));
    // Descriptors land verbatim in stream_descriptors.
    let i_short = prog
        .streams
        .iter()
        .position(|s| spec_pid(s) == 0x103)
        .unwrap();
    let i_upper = prog
        .streams
        .iter()
        .position(|s| spec_pid(s) == 0x104)
        .unwrap();
    assert_eq!(
        prog.stream_descriptors[i_short],
        vec![vec![0x0Au8, 0x02, b'e', b'n']],
        "truncated 0x0A passes through verbatim"
    );
    assert_eq!(
        prog.stream_descriptors[i_upper],
        vec![vec![0x0Au8, 0x04, b'E', b'N', b'G', 0x00]],
        "uppercase 0x0A passes through verbatim (no normalization)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration round-trip: mux → demux → from_program_map
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal Annex-B H.264 access unit: SPS + PPS + IDR slice.
fn synthetic_h264_au() -> Vec<u8> {
    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        v.push((0b11 << 5) | nal_type); // high nal_ref_idc
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(nal(7, &[0x42, 0xC0, 0x28, 0xD9])); // SPS
    au.extend(nal(8, &[0xCE, 0x38, 0x80])); // PPS
    au.extend(nal(5, &[0x88, 0x84, 0x0A, 0x7C, 0x11])); // IDR slice
    au
}

/// Minimal KLV LS: 16-byte ST 0601 UL + BER short-form length + 4 body bytes.
fn synthetic_klv_ls() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    v.push(0x04);
    v.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    v
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

#[test]
fn mux_demux_program_map_rebuilds_the_config() {
    // Config A: H.264 video + sync KLV, pcr_pid left None (auto-resolve).
    let cfg_a = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let mut mux = Muxer::new(cfg_a.clone()).unwrap();
    for i in 0..3i64 {
        let pts = Pts90khz::new(90_000 + i * 3_000);
        mux.push_video(&synthetic_h264_au(), pts, true)
            .expect("push_video");
        mux.push_klv(&synthetic_klv_ls(), pts, 0x00)
            .expect("push_klv");
    }
    let ts = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts).unwrap();
    let mut last_pm = None;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::ProgramMap(m) = evt {
            last_pm = Some(m);
        }
    }
    let demuxed = last_pm.expect("PMT discovery must emit a ProgramMap");

    let rebuilt = MuxerConfig::from_program_map(&demuxed, &[]).expect("rebuild from demuxed map");
    assert_eq!(rebuilt.programs.len(), 1);
    let (a, b) = (&cfg_a.programs[0], &rebuilt.programs[0]);
    assert_eq!(b.program_number, a.program_number);
    assert_eq!(b.pmt_pid, a.pmt_pid);
    assert_eq!(b.streams, a.streams, "stream specs survive the round-trip");
    // Asymmetry: config A left pcr_pid None, which the muxer resolved to
    // the first video PID (0x101) at mux time. The PMT declares that
    // resolved PID, so the demuxed map carries pcr_pid == 0x101 and the
    // rebuilt config pins it explicitly. Both configs drive the same wire
    // behavior.
    assert_eq!(a.pcr_pid, None);
    assert_eq!(demuxed.pcr_pid, 0x101);
    assert_eq!(b.pcr_pid, Some(0x101));
}

// ─────────────────────────────────────────────────────────────────────────────
// Unknown→Data mapping on a real demuxed map (private-data W2)
// ─────────────────────────────────────────────────────────────────────────────

/// Mux the corpus-shaped private-stream mix (video + sync KLV + data
/// 0xF0-with-descriptor + 0xF1 + bare 0x06) and demux it back to its
/// `ProgramMap` — the input shape for the Unknown→Data conversion tests.
fn demuxed_private_mix_pm() -> ProgramMap {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1021, KlvStreamType::SynchronousMetadata, true);
        prog.add_data(0x1100, 0xF0, /*carries_pts=*/ true);
        prog.stream_descriptors_for_data(0, vec![b"\xFF\x0ASERIAL_ADF".to_vec()])
            .unwrap();
        prog.add_data(0x1101, 0xF1, /*carries_pts=*/ true);
        // carries_pts=false on the wire — the conversion must still come
        // back carries_pts=true (PMT can't declare the PES-level property).
        prog.add_data(0x1102, 0x06, /*carries_pts=*/ false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    // One video AU + one KLV LS so PSI/PCR pacing starts and the PMT flows.
    mux.push_video(&synthetic_h264_au(), Pts90khz::new(900_000), true)
        .expect("push_video");
    mux.push_klv(&synthetic_klv_ls(), Pts90khz::new(900_000), 0x00)
        .expect("push_klv");
    let ts = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts).unwrap();
    let mut last_pm = None;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::ProgramMap(m) = evt {
            last_pm = Some(m);
        }
    }
    last_pm.expect("PMT discovery must emit a ProgramMap")
}

#[test]
fn unknown_streams_map_to_data_specs_with_descriptors() {
    let pm = demuxed_private_mix_pm();
    let cfg =
        MuxerConfig::from_program_map(&pm, &[]).expect("unknown streams now map to Data specs");
    let prog = &cfg.programs[0];
    assert_eq!(prog.streams.len(), 5, "video + KLV + three data streams");

    // carries_pts is ALWAYS true — a PES-level property the PMT cannot
    // declare; even the carries_pts=false source stream (0x1102) comes
    // back true (KLV-rule parity).
    assert!(prog.streams.contains(&StreamSpec::Data {
        pid: 0x1100,
        stream_type: 0xF0,
        carries_pts: true,
    }));
    assert!(prog.streams.contains(&StreamSpec::Data {
        pid: 0x1101,
        stream_type: 0xF1,
        carries_pts: true,
    }));
    assert!(prog.streams.contains(&StreamSpec::Data {
        pid: 0x1102,
        stream_type: 0x06,
        carries_pts: true,
    }));

    // Descriptors preserved byte-identical (TLV form), nothing invented.
    let idx = |pid: u16| {
        prog.streams
            .iter()
            .position(|s| spec_pid(s) == pid)
            .unwrap_or_else(|| panic!("stream for pid 0x{pid:04X}"))
    };
    assert_eq!(
        prog.stream_descriptors[idx(0x1100)],
        vec![b"\xFF\x0ASERIAL_ADF".to_vec()]
    );
    assert!(prog.stream_descriptors[idx(0x1101)].is_empty());
    assert!(prog.stream_descriptors[idx(0x1102)].is_empty());
}

#[test]
fn full_round_trip_preserves_unknown_streams() {
    // PMT-level fidelity proof: convert the demuxed map, mux through the
    // converted config, demux again — the second ProgramMap classifies the
    // same PIDs Unknown with the same descriptors. (Sample-payload
    // fidelity is pinned by the W1 golden in mux_data.rs.)
    let first = demuxed_private_mix_pm();
    let cfg = MuxerConfig::from_program_map(&first, &[]).expect("convert the demuxed map");

    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&synthetic_h264_au(), Pts90khz::new(900_000), true)
        .expect("push_video");
    let handles = mux.data_handles();
    assert_eq!(handles.len(), 3, "all three data streams survived");
    for (i, h) in handles.iter().enumerate() {
        // carries_pts=true on every converted stream → a PTS is required.
        mux.push_data_to(*h, &[0xA5; 16], Pts90khz::new(900_000 + i as i64 * 3_000))
            .expect("push_data_to");
    }
    let ts = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts).unwrap();
    let mut last_pm = None;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::ProgramMap(m) = evt {
            last_pm = Some(m);
        }
    }
    let second = last_pm.expect("PMT discovery must emit a ProgramMap");

    for (pid, st) in [(0x1100u16, 0xF0u8), (0x1101, 0xF1), (0x1102, 0x06)] {
        let a = first.streams.iter().find(|s| s.pid == pid).unwrap();
        let b = second
            .streams
            .iter()
            .find(|s| s.pid == pid)
            .unwrap_or_else(|| panic!("second PMT entry for pid 0x{pid:04X}"));
        assert_eq!(b.kind, DemuxKind::Unknown(st));
        assert_eq!(
            b.raw_descriptors, a.raw_descriptors,
            "descriptors survive the second trip verbatim (pid 0x{pid:04X})"
        );
    }
}

#[test]
fn drop_unknown_still_excludes() {
    // Regression pin: `drop=[Unknown]` semantics are untouched by the
    // Unknown→Data mapping — the filter check precedes the kind match.
    let pm = demuxed_private_mix_pm();
    let cfg = MuxerConfig::from_program_map(&pm, &[StreamKindTag::Unknown])
        .expect("dropping Unknown excludes the data streams");
    let prog = &cfg.programs[0];
    assert!(
        prog.streams
            .iter()
            .all(|s| !matches!(s, StreamSpec::Data { .. })),
        "no Data streams when Unknown is dropped"
    );
    assert_eq!(prog.streams.len(), 2, "video + KLV remain");
}

// ─────────────────────────────────────────────────────────────────────────────
// Exact descriptor preservation across all typed stream kinds
// ─────────────────────────────────────────────────────────────────────────────

/// Config-level helper: index of the stream with a given PID.
fn idx_for(prog: &tst_core::mpegts::mux::MuxerProgramConfig, pid: u16) -> usize {
    prog.streams
        .iter()
        .position(|s| spec_pid(s) == pid)
        .unwrap_or_else(|| panic!("no stream with pid 0x{pid:04X}"))
}

/// Build a private TLV with the given tag and body bytes (for use as a
/// caller-supplied descriptor in ProgramMap.raw_descriptors tests).
fn private_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    assert!(body.len() <= 255, "private_tlv body too long");
    let mut v = vec![tag, body.len() as u8];
    v.extend_from_slice(body);
    v
}

#[test]
fn descriptor_preservation_video() {
    // A video stream carrying one extra private descriptor (tag 0xE0) must
    // land verbatim in the muxer config's stream_descriptors after
    // from_program_map.
    let mut vid = stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264));
    vid.raw_descriptors = vec![RawDescriptor {
        tag: 0xE0,
        data: b"VEND".to_vec(),
    }];
    let p = pm(0x100, 0x101, vec![vid]);
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("video with descriptor is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x101);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0xE0, b"VEND")],
        "video descriptor preserved verbatim"
    );
}

#[test]
fn descriptor_preservation_audio() {
    // An audio stream carrying one extra private descriptor (tag 0xE0, no
    // ISO-639) must land verbatim in stream_descriptors.
    let mut aud = stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    aud.raw_descriptors = vec![RawDescriptor {
        tag: 0xE0,
        data: b"AEXT".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), aud],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[])
        .expect("audio with private descriptor is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x103);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0xE0, b"AEXT")],
        "audio private descriptor preserved verbatim"
    );
}

#[test]
fn descriptor_preservation_klv_sync() {
    // A KLV-sync stream carrying an extra private descriptor (tag 0xE1) must
    // land verbatim in stream_descriptors. The KLVA Registration is
    // auto-emitted by the muxer — but that happens at mux time, not in the
    // config itself, so the config-level check only has the private descriptor.
    let mut klv = stream(
        0x102,
        0x15,
        DemuxKind::KlvSync {
            declared_link: None,
        },
    );
    klv.raw_descriptors = vec![RawDescriptor {
        tag: 0xE1,
        data: b"KPRIV".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), klv],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[])
        .expect("KLV-sync with private descriptor is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x102);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0xE1, b"KPRIV")],
        "KLV-sync private descriptor preserved verbatim"
    );
}

#[test]
fn descriptor_preservation_klv_async() {
    // A KLV-async stream carrying an extra private descriptor must land
    // verbatim in stream_descriptors.
    let mut klv = stream(0x102, 0x06, DemuxKind::KlvAsync);
    klv.raw_descriptors = vec![RawDescriptor {
        tag: 0xE1,
        data: b"APRV".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), klv],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[]).expect("KLV-async with private descriptor");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x102);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0xE1, b"APRV")],
        "KLV-async private descriptor preserved verbatim"
    );
}

#[test]
fn descriptor_preservation_subtitle_cea708() {
    // A CEA-708 subtitle stream carrying its GA94 Registration descriptor
    // plus an extra private descriptor must both land verbatim in
    // stream_descriptors. The muxer's auto-emit suppression
    // fires when the caller supplies any recognized subtitle descriptor
    // (GA94 registration tag 0x05 with format_identifier GA94), so the
    // output PMT has the caller's GA94 first, then the private 0xE2.
    let mut sub = stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::Cea708Standalone));
    sub.raw_descriptors = vec![
        RawDescriptor {
            tag: 0x05,
            data: b"GA94".to_vec(),
        },
        RawDescriptor {
            tag: 0xE2,
            data: b"CEXT".to_vec(),
        },
    ];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), sub],
    );
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("CEA-708 with descriptors is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x105);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0x05, b"GA94"), private_tlv(0xE2, b"CEXT")],
        "CEA-708 subtitle descriptors preserved verbatim"
    );
}

#[test]
fn descriptor_preservation_subtitle_webvtt() {
    // A WebVTT subtitle stream carrying its VTTC Registration descriptor
    // plus an extra private descriptor must both land verbatim in
    // stream_descriptors.
    let mut sub = stream(0x105, 0x06, DemuxKind::Subtitle(DemuxSub::WebVttInTs));
    sub.raw_descriptors = vec![
        RawDescriptor {
            tag: 0x05,
            data: b"VTTC".to_vec(),
        },
        RawDescriptor {
            tag: 0xE3,
            data: b"WEXT".to_vec(),
        },
    ];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), sub],
    );
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("WebVTT with descriptors is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x105);
    assert_eq!(
        prog.stream_descriptors[i],
        vec![private_tlv(0x05, b"VTTC"), private_tlv(0xE3, b"WEXT")],
        "WebVTT subtitle descriptors preserved verbatim"
    );
}

#[test]
fn audio_multi_language_verbatim() {
    // An audio stream with a multi-entry ISO-639 descriptor (two language
    // slots: "eng" audio_type=0x00 + "FRA" audio_type=0x01 — uppercase is
    // valid on the wire per ETSI EN 300 468 §6.2.41). Both entries must
    // survive verbatim in stream_descriptors after from_program_map.
    //
    // Tag 0x0A body layout: 4 bytes per entry (3-byte code + 1-byte
    // audio_type). Two entries → 8 bytes.
    let lang_body: Vec<u8> = b"eng\x00FRA\x01".to_vec(); // 8 bytes
    let mut aud = stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    aud.raw_descriptors = vec![RawDescriptor {
        tag: 0x0A,
        data: lang_body.clone(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), aud],
    );
    let cfg =
        MuxerConfig::from_program_map(&p, &[]).expect("multi-language audio is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x103);
    // Audio stream has language: None (add_audio, not add_audio_with_language).
    assert!(
        matches!(&prog.streams[i], StreamSpec::Audio { language: None, .. }),
        "add_audio used (no language field), language passes via stream_descriptors"
    );
    // The 0x0A descriptor is preserved verbatim — both entries, both
    // exact bytes (including uppercase "FRA" and audio_type bytes).
    let expected_tlv = {
        let mut v = vec![0x0Au8, lang_body.len() as u8];
        v.extend_from_slice(&lang_body);
        v
    };
    assert_eq!(
        prog.stream_descriptors[i],
        vec![expected_tlv],
        "multi-language 0x0A descriptor preserved verbatim (incl. uppercase + audio_type)"
    );
}

#[test]
fn audio_iso639_dedup_no_double_emit() {
    // An audio stream whose raw PMT descriptors already carry an ISO-639
    // descriptor (tag 0x0A) must produce exactly ONE 0x0A descriptor in
    // the muxer config — the caller's, verbatim — not two (no auto-emit
    // duplicate from the language field on the StreamSpec).
    let mut aud = stream(0x103, 0x0F, DemuxKind::Audio(DemuxAudio::Aac));
    aud.raw_descriptors = vec![RawDescriptor {
        tag: 0x0A,
        data: b"eng\x00".to_vec(),
    }];
    let p = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), aud],
    );
    let cfg = MuxerConfig::from_program_map(&p, &[])
        .expect("audio with 0x0A descriptor is representable");
    let prog = &cfg.programs[0];
    let i = idx_for(prog, 0x103);
    // Exactly one 0x0A TLV — the caller's verbatim. No duplicate from
    // StreamSpec::Audio.language (which is None when add_audio is used).
    let lang_descs: Vec<&Vec<u8>> = prog.stream_descriptors[i]
        .iter()
        .filter(|tlv| !tlv.is_empty() && tlv[0] == 0x0A)
        .collect();
    assert_eq!(
        lang_descs.len(),
        1,
        "exactly one 0x0A descriptor (no auto-emit duplicate)"
    );
    assert_eq!(
        lang_descs[0],
        &vec![0x0Au8, 0x04, b'e', b'n', b'g', 0x00],
        "caller's 0x0A preserved verbatim"
    );
}

#[test]
fn e2e_klva_dedup_no_auto_emit_duplicate() {
    // End-to-end acceptance: an auto-generated descriptor must not
    // DUPLICATE a retained equivalent through a full
    // from_program_map → mux → demux round-trip.
    //
    // A KLV-sync stream whose source PMT already carries the KLVA
    // Registration (tag 0x05, format_identifier "KLVA") is fed through
    // from_program_map. The converted config preserves the caller's KLVA
    // verbatim, and the muxer suppresses its own KLVA auto-emit because a
    // caller Registration is present (state.rs build_pmt_descriptor_cache).
    // The output PMT must therefore carry EXACTLY ONE KLVA Registration —
    // not two.
    let klva = RawDescriptor {
        tag: 0x05,
        data: b"KLVA".to_vec(),
    };
    let mut klv = stream(
        0x102,
        0x15,
        DemuxKind::KlvSync {
            declared_link: None,
        },
    );
    klv.raw_descriptors = vec![klva];
    let source = pm(
        0x100,
        0x101,
        vec![stream(0x101, 0x1B, DemuxKind::Video(DemuxVideo::H264)), klv],
    );

    let cfg = MuxerConfig::from_program_map(&source, &[]).expect("KLV-with-KLVA is representable");
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&synthetic_h264_au(), Pts90khz::new(900_000), true)
        .expect("push_video");
    mux.push_klv(&synthetic_klv_ls(), Pts90khz::new(900_000), 0x00)
        .expect("push_klv");
    let ts = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts).unwrap();
    let mut last_pm = None;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::ProgramMap(m) = evt {
            last_pm = Some(m);
        }
    }
    let out = last_pm.expect("PMT discovery must emit a ProgramMap");

    let klv_out = out
        .streams
        .iter()
        .find(|s| s.pid == 0x102)
        .expect("KLV stream in output PMT");
    let klva_count = klv_out
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x05 && d.data.get(..4) == Some(b"KLVA"))
        .count();
    assert_eq!(
        klva_count, 1,
        "exactly one KLVA Registration in the output PMT (auto-emit suppressed, no duplicate): {:?}",
        klv_out.raw_descriptors
    );
}
