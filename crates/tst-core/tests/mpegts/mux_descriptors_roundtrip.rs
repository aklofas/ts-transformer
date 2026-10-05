//! Round-trip caller-supplied PMT descriptors through Muxer → Demuxer.
//!
//! Asserts:
//!   - Each StreamInfo carries the descriptors the caller pushed in
//!     (correct count, correct tags, correct payloads).
//!   - extract_user_label decodes user_private labels.
//!   - Auto-emit suppression on caller-supplied KLVA Registration works.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::low_level::extract_user_label;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::descriptors;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn drive_psi(cfg: MuxerConfig) -> Vec<u8> {
    let mut mux = Muxer::new(cfg).unwrap();
    // Push one tiny video NAL to advance PTS past the PSI threshold.
    mux.push_video(&[0, 0, 0, 1, 0x09, 0x10], Pts90khz::new(9000), true)
        .ok();
    let mut buf = vec![0u8; 188 * 32];
    let n = mux.pull(&mut buf);
    buf.truncate(n);
    buf
}

fn drain_events(bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut demuxer = Demuxer::new();
    demuxer.feed(bytes).unwrap();
    let mut out = Vec::new();
    while let Some(ev) = demuxer.next_event() {
        out.push(ev);
    }
    out
}

#[test]
fn family_b_klv_descriptor_stack_round_trips() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.stream_descriptors_for_video(
            0,
            vec![descriptors::user_private(b"EO 1080p").expect("label within cap")],
        )
        .unwrap();
        prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
        prog.stream_descriptors_for_klv(
            0,
            vec![
                descriptors::metadata_klva(0x00),
                descriptors::metadata_std(0, 0, 0),
                descriptors::user_private(b"KLV_SYNC").expect("label within cap"),
            ],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    // Video PID — one tag 0xFF descriptor.
    let video = pm.streams.iter().find(|s| s.pid == 0x100).unwrap();
    assert_eq!(video.raw_descriptors.len(), 1);
    assert_eq!(video.raw_descriptors[0].tag, 0xFF);
    assert_eq!(
        extract_user_label(&video.raw_descriptors).as_deref(),
        Some("EO 1080p")
    );

    // KLV PID — four descriptors: auto-emitted KLVA Registration first,
    // then caller order: 0x26, 0x27, 0xFF.
    let klv = pm.streams.iter().find(|s| s.pid == 0x102).unwrap();
    assert_eq!(klv.raw_descriptors.len(), 4);
    assert_eq!(klv.raw_descriptors[0].tag, 0x05); // auto-emitted KLVA
    assert_eq!(&klv.raw_descriptors[0].data[..4], b"KLVA");
    assert_eq!(klv.raw_descriptors[1].tag, 0x26);
    assert_eq!(klv.raw_descriptors[2].tag, 0x27);
    assert_eq!(klv.raw_descriptors[3].tag, 0xFF);
    // Metadata descriptor wins over user_private per priority order.
    assert_eq!(
        extract_user_label(&klv.raw_descriptors).as_deref(),
        Some("KLV")
    );
}

#[test]
fn klva_auto_emit_suppressed_when_caller_supplies_registration() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.stream_descriptors_for_klv(
            0,
            vec![descriptors::registration(*b"KLVA", &[]).expect("within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let klv = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    // Exactly ONE Registration descriptor — auto-emit suppressed because
    // the caller supplied their own. Two would mean we duplicated.
    let regs: Vec<_> = klv
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x05)
        .collect();
    assert_eq!(
        regs.len(),
        1,
        "auto-emit was not suppressed (found {} Registration descriptors)",
        regs.len()
    );
}

#[test]
fn family_a_hdmv_video_registration_round_trips() {
    // Replicate a real-world capture shape: video PID with
    // Registration "HDMV" + 4 trailing bytes.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.stream_descriptors_for_video(
            0,
            vec![
                descriptors::registration(*b"HDMV", &[0xFF, 0x1B, 0x44, 0x3F])
                    .expect("4-byte trailer within cap"),
            ],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");
    let video = pm.streams.iter().find(|s| s.pid == 0x100).unwrap();
    assert_eq!(video.raw_descriptors.len(), 1);
    let r = &video.raw_descriptors[0];
    assert_eq!(r.tag, 0x05);
    assert_eq!(&r.data[..4], b"HDMV");
    assert_eq!(&r.data[4..], &[0xFF, 0x1B, 0x44, 0x3F]);
}

#[tracing_test::traced_test]
#[test]
fn non_klva_registration_on_klv_pid_logs_warning() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.stream_descriptors_for_klv(
            0,
            vec![descriptors::registration(*b"VEND", &[]).expect("within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let _ = Muxer::new(cfg).unwrap();
    assert!(logs_contain("non-KLVA format_identifier"));
}

#[test]
fn ac3_registration_descriptor_auto_emits_on_pmt() {
    // Build a config with one video + one AC-3 audio stream, drive PSI,
    // demux it, find the PMT entry for the audio PID, and assert the
    // raw_descriptors carry tag 0x05 with format_identifier "AC-3".
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Ac3);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let ac3_regs: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x05 && d.data.starts_with(b"AC-3"))
        .collect();
    assert_eq!(
        ac3_regs.len(),
        1,
        "expected one AC-3 Registration descriptor on audio PID, found {}",
        ac3_regs.len()
    );
}

#[test]
fn ac3_auto_emit_suppressed_when_caller_supplies_registration() {
    // Caller pre-supplies their own AC-3 registration. Assert exactly one
    // tag-0x05 descriptor with format_identifier "AC-3" — no duplication.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Ac3);
        prog.stream_descriptors_for_audio(
            0,
            vec![descriptors::registration(*b"AC-3", &[]).expect("within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let regs: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x05)
        .collect();
    assert_eq!(
        regs.len(),
        1,
        "auto-emit was not suppressed (found {} Registration descriptors)",
        regs.len()
    );
    assert_eq!(
        &regs[0].data[..4],
        b"AC-3",
        "Registration descriptor should have AC-3 format_identifier"
    );
}

#[test]
fn audio_language_descriptor_auto_emits_when_set() {
    // Build a config with one video + one audio with language=Some(*b"eng"),
    // drive PSI, demux it, find the PMT entry for the audio PID, and assert
    // raw_descriptors carries tag 0x0A with body [b'e', b'n', b'g', 0x00].
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio_with_language(0x101, AudioCodec::Aac, *b"eng");
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let lang_descs: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x0A)
        .collect();
    assert_eq!(
        lang_descs.len(),
        1,
        "expected one ISO 639 language descriptor on audio PID, found {}",
        lang_descs.len()
    );
    // Body: 3-byte lang code + 1-byte audio_type=0x00.
    assert_eq!(
        lang_descs[0].data.as_slice(),
        &[b'e', b'n', b'g', 0x00],
        "ISO 639 descriptor body should be eng + 0x00 audio_type"
    );
}

#[test]
fn audio_language_descriptor_absent_when_unset() {
    // add_audio (without language) — no tag-0x0A descriptor should appear.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let lang_descs: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x0A)
        .collect();
    assert_eq!(
        lang_descs.len(),
        0,
        "no ISO 639 descriptor expected on audio PID without language, found {}",
        lang_descs.len()
    );
}

#[test]
fn klva_auto_emits_on_sync_metadata_too() {
    // stream_type 0x15 SynchronousMetadata — should also auto-emit KLVA
    // Registration descriptor. ffmpeg mpegtsenc.c:817-818 emits KLVA on
    // the metadata stream_type path too — receivers gate KLV classification
    // on the descriptor regardless of stream_type.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let klv = pm.streams.iter().find(|s| s.pid == 0x102).unwrap();
    let klva_regs: Vec<_> = klv
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x05 && d.data.starts_with(b"KLVA"))
        .collect();
    assert_eq!(
        klva_regs.len(),
        1,
        "expected one KLVA Registration descriptor on SynchronousMetadata KLV PID, found {}",
        klva_regs.len()
    );
}

#[test]
fn audio_language_auto_emit_suppressed_when_caller_supplies() {
    // Caller pre-supplies their own ISO 639 language descriptor via
    // stream_descriptors_for_audio with a different language ("fra"). Assert
    // exactly one tag-0x0A descriptor with caller's language code (not "eng").
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio_with_language(0x101, AudioCodec::Aac, *b"eng");
        prog.stream_descriptors_for_audio(0, vec![descriptors::iso_639_language(*b"fra", 0x00)])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let lang_descs: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x0A)
        .collect();
    assert_eq!(
        lang_descs.len(),
        1,
        "auto-emit was not suppressed (found {} language descriptors)",
        lang_descs.len()
    );
    // Caller's "fra" wins over auto-emit "eng".
    assert_eq!(
        lang_descs[0].data.as_slice(),
        &[b'f', b'r', b'a', 0x00],
        "caller's language code (fra) should win over auto-emit (eng)"
    );
}

#[test]
fn ac3_audio_stream_descriptor_auto_emits_on_pmt() {
    // ATSC A/52:2018 §A.4.3 mandates the AC-3 audio
    // descriptor (tag 0x81) on every AC-3 PMT entry. The muxer should
    // emit it automatically alongside the AC-3 Registration descriptor.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Ac3);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let ac3_audio: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x81)
        .collect();
    assert_eq!(
        ac3_audio.len(),
        1,
        "expected one AC-3 audio stream descriptor (tag 0x81), found {}",
        ac3_audio.len()
    );
    // 3-byte body per the minimal-conformant shape (Table A4.1, fields
    // up to and including the first termination point after langcod).
    assert_eq!(ac3_audio[0].data.len(), 3);
    // byte0 high 3 bits = sample_rate_code (0b111 = any per Table A4.2);
    // bsid = 8 in low 5 bits → (7 << 5) | 8 = 0xE8.
    assert_eq!(ac3_audio[0].data[0], 0xE8);
}

#[test]
fn ac3_audio_descriptor_suppressed_when_caller_supplies() {
    // Caller pre-supplies their own tag-0x81 descriptor; the muxer
    // should honor it verbatim and NOT auto-emit a second one.
    let caller_desc = descriptors::ac3_audio_stream_descriptor(
        0,      // sample_rate_code: 48kHz exact
        8,      // bsid
        10,     // bit_rate_code: 192 kbps exact
        0,      // surround_mode
        0,      // bsmod: CM
        0b0010, // num_channels: 2/0 stereo
        true,   // full_svc
    );

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Ac3);
        prog.stream_descriptors_for_audio(0, vec![caller_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let bytes = drive_psi(cfg);
    let events = drain_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");

    let audio = pm.streams.iter().find(|s| s.pid == 0x101).unwrap();
    let ac3_audio: Vec<_> = audio
        .raw_descriptors
        .iter()
        .filter(|d| d.tag == 0x81)
        .collect();
    assert_eq!(
        ac3_audio.len(),
        1,
        "auto-emit was not suppressed (found {} tag-0x81 descriptors)",
        ac3_audio.len()
    );
    // Caller's payload survives verbatim (body bytes 0xE8 are the byte0
    // shape; caller's emits byte0 = (0<<5)|8 = 0x08).
    assert_eq!(ac3_audio[0].data[0], 0x08);
}
