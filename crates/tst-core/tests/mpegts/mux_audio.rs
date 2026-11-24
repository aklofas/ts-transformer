//! Integration tests for sender-side audio carriage in `mpegts::mux`.

use crate::common::fixtures::audio_fixture_path;
use crate::common::mux_demux::{demux_bytes, drain_mux, es_entries};
use crate::common::synthetic::build_adts_frame;
use tst_core::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{AudioCodec as DemuxAudioCodec, DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

#[test]
fn audio_only_program_mux_produces_pat_pmt_audio_pes() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_audio(0x300, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let frames = vec![
        0xFF, 0xFD, 0x00, 0x10, /* MP2 frame header */ 0xDE, 0xAD, 0xBE, 0xEF,
    ];
    muxer.push_audio(&frames, Pts90khz::new(90_000)).unwrap();

    let mut buf = vec![0u8; 188 * 64];
    let n = muxer.pull(&mut buf);
    assert!(n > 0);
    // PAT, PMT, audio PES all present.
    let packets: Vec<&[u8]> = buf[..n].chunks_exact(188).collect();
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0)
    ); // PAT
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1000)
    ); // PMT
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x300)
    ); // audio PES
}

#[test]
fn three_stream_program_audio_video_klv_routing() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.add_klv(0x200, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();

    let video_handle = muxer.video_handles()[0];
    let audio_handle = muxer.audio_handles()[0];
    let klv_handle = muxer.klv_handles()[0];

    let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F]; // tiny SPS
    muxer
        .push_video_to(video_handle, &nal, Pts90khz::new(90_000), true)
        .unwrap();
    muxer
        .push_audio_to(audio_handle, Pts90khz::new(90_000), b"audio_payload_bytes")
        .unwrap();
    muxer
        .push_klv_to(klv_handle, b"klv_record_bytes", Pts90khz::new(90_000), 0x00)
        .unwrap();

    let mut buf = vec![0u8; 188 * 256];
    let n = muxer.pull(&mut buf);
    assert!(n > 0);

    // Confirm PESs land on the correct PIDs.
    let pids: std::collections::BTreeSet<u16> = buf[..n]
        .chunks_exact(188)
        .map(|p| ((p[1] as u16 & 0x1F) << 8) | p[2] as u16)
        .collect();
    assert!(pids.contains(&0x100), "video PID present");
    assert!(pids.contains(&0x200), "klv PID present");
    assert!(pids.contains(&0x300), "audio PID present");
}

#[test]
fn each_audio_codec_has_correct_pmt_stream_type() {
    let codecs = [
        (AudioCodec::Mp2, 0x03),
        (AudioCodec::Aac, 0x0F),
        (AudioCodec::AacLatm, 0x11),
        (AudioCodec::Ac3, 0x81),
    ];
    for (codec, expected_st) in codecs {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x300, codec);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = Muxer::new(cfg).unwrap();
        muxer.push_audio(b"x", Pts90khz::new(90_000)).unwrap();

        let mut buf = vec![0u8; 188 * 16];
        let n = muxer.pull(&mut buf);
        let pmt = buf[..n]
            .chunks_exact(188)
            .find(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1000)
            .expect("a PMT packet on PID 0x1000");
        let st = es_entries(pmt)
            .find(|(pid, _)| *pid == 0x300)
            .expect("PID 0x300 in the PMT")
            .1;
        assert_eq!(st, expected_st, "codec {codec:?} stream_type mismatch");
    }
}

// ── Random access signalling on audio PES starts ────────────────────────────

const VIDEO_PID: u16 = 0x100;
const AUDIO_PID: u16 = 0x300;
/// One AAC frame (1 024 samples) at 48 kHz, in 90 kHz ticks.
const AAC_FRAME_TICKS: i64 = 1_920;

/// One TS packet as the audio tests read it.
struct AudioPkt {
    pid: u16,
    pusi: bool,
    /// Adaptation-field flags byte; `None` without an adaptation field or
    /// with one of length 0 (a single stuffing byte, no flags).
    af_flags: Option<u8>,
    /// 33-bit PCR base when `PCR_flag` is set.
    pcr_base: Option<u64>,
    /// PTS of the PES that starts in this packet.
    pts: Option<u64>,
}

fn audio_pkts(ts: &[u8]) -> Vec<AudioPkt> {
    assert_eq!(ts.len() % 188, 0, "whole TS packets");
    let mut out = Vec::new();
    for p in ts.chunks_exact(188) {
        assert_eq!(p[0], 0x47, "TS sync byte");
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        let pusi = p[1] & 0x40 != 0;
        let mut payload_at = 4;
        let mut af_flags = None;
        let mut pcr_base = None;
        if p[3] & 0x20 != 0 {
            let af_len = p[4] as usize;
            if af_len > 0 {
                af_flags = Some(p[5]);
                if p[5] & 0x10 != 0 {
                    let b = &p[6..11];
                    pcr_base = Some(
                        b.iter().take(4).fold(0u64, |acc, x| (acc << 8) | *x as u64) << 1
                            | (b[4] as u64 >> 7),
                    );
                }
            }
            payload_at = 5 + af_len;
        }
        let pes = &p[payload_at..];
        let pts = (pusi && pes.starts_with(&[0, 0, 1]) && pes[7] & 0x80 != 0).then(|| {
            let t = &pes[9..14];
            (t[0] as u64 >> 1 & 0x07) << 30
                | (t[1] as u64) << 22
                | (t[2] as u64 >> 1) << 15
                | (t[3] as u64) << 7
                | (t[4] as u64 >> 1)
        });
        out.push(AudioPkt {
            pid,
            pusi,
            af_flags,
            pcr_base,
            pts,
        });
    }
    out
}

/// The packets that start a PES on `pid`, in wire order.
fn pes_starts(ts: &[u8], pid: u16) -> Vec<AudioPkt> {
    audio_pkts(ts)
        .into_iter()
        .filter(|p| p.pid == pid && p.pusi)
        .collect()
}

fn audio_muxer(codec: AudioCodec, with_video: bool, lead_ms: u32, buffer: usize) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    if with_video {
        prog.add_video(VIDEO_PID, VideoCodec::H264);
    }
    prog.add_audio(AUDIO_PID, codec);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.pcr_lead_ms(lead_ms);
    b.buffer_packets(buffer);
    Muxer::new(b.build().unwrap()).unwrap()
}

/// A tiny IDR access unit for the video PID (SPS NAL only: the muxer does
/// not parse it).
const TINY_IDR: [u8; 8] = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F];

/// The first two LOAS frames of the LATM fixture's first audio PES: the
/// first carries its `StreamMuxConfig` (`useSameStreamMux` = 0), the second
/// reuses it (`useSameStreamMux` = 1).
fn fixture_loas_frames() -> (Vec<u8>, Vec<u8>) {
    let bytes = std::fs::read(audio_fixture_path("aac-latm.ts")).expect("read aac-latm.ts");
    let es = demux_bytes(&bytes)
        .into_iter()
        .find_map(|e| match e {
            DemuxEvent::Sample {
                payload:
                    SamplePayload::Audio {
                        codec: DemuxAudioCodec::AacLatm,
                        frames,
                    },
                ..
            } => Some(frames.to_vec()),
            _ => None,
        })
        .expect("an AAC-LATM sample in the fixture");
    // LOAS AudioSyncStream: 11-bit sync 0x2B7, 13-bit audioMuxLengthBytes.
    let frame_at = |at: usize| {
        assert_eq!(es[at], 0x56, "LOAS sync at {at}");
        assert_eq!(es[at + 1] & 0xE0, 0xE0, "LOAS sync at {at}");
        let len = 3 + (((es[at + 1] as usize & 0x1F) << 8) | es[at + 2] as usize);
        es[at..at + len].to_vec()
    };
    let first = frame_at(0);
    let second = frame_at(first.len());
    (first, second)
}

#[test]
fn aac_pes_start_sets_random_access_indicator() {
    let mut m = audio_muxer(AudioCodec::Aac, true, 100, 10_000);
    m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
        .unwrap();
    m.push_audio(&build_adts_frame(3, 2, 200), Pts90khz::new(90_000))
        .unwrap();
    let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
    assert_eq!(starts.len(), 1);
    // The audio PID is not the PCR PID: RAI alone, no PCR.
    assert_eq!(starts[0].af_flags, Some(0x40));
}

#[test]
fn aac_rap_on_the_pcr_pid_carries_a_pcr() {
    // Audio-only: the audio PID is the PCR PID, where H.222.0 §2.4.3.5 lets
    // RAI be set only in a packet that carries a PCR.
    let mut m = audio_muxer(AudioCodec::Aac, false, 100, 10_000);
    let frame = build_adts_frame(3, 2, 200);
    m.push_audio(&frame, Pts90khz::new(90_000)).unwrap();
    // One frame later (21.3 ms), well inside the 40 ms PCR interval: no PCR
    // is due, so only the random access point can put one in.
    m.push_audio(&frame, Pts90khz::new(90_000 + AAC_FRAME_TICKS))
        .unwrap();
    let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
    assert_eq!(starts.len(), 2);
    for s in &starts {
        assert_eq!(s.af_flags.map(|f| f & 0x50), Some(0x50), "RAI and PCR");
    }
}

#[test]
fn mp2_and_ac3_pes_starts_do_not_set_random_access_indicator() {
    for codec in [AudioCodec::Mp2, AudioCodec::Ac3] {
        for with_video in [false, true] {
            let mut m = audio_muxer(codec, with_video, 100, 10_000);
            if with_video {
                m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
                    .unwrap();
            }
            for i in 0..2 {
                m.push_audio(&[0xAB; 200], Pts90khz::new(90_000 + i * AAC_FRAME_TICKS))
                    .unwrap();
            }
            let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
            assert_eq!(starts.len(), 2);
            for (i, s) in starts.iter().enumerate() {
                let flags = s.af_flags.unwrap_or(0);
                assert_eq!(
                    flags & 0x40,
                    0,
                    "{codec:?} video={with_video} PES {i}: no RAI"
                );
                // Only the program's first push carries a PCR on an audio
                // PCR PID; the second is inside the PCR interval.
                let pcr = !with_video && i == 0;
                assert_eq!(
                    flags & 0x10 != 0,
                    pcr,
                    "{codec:?} video={with_video} PES {i}: PCR"
                );
            }
        }
    }
}

#[test]
fn a_latm_frame_that_repeats_its_config_is_not_a_random_access_point() {
    let (config_frame, same_mux_frame) = fixture_loas_frames();
    // The bit the rule reads: useSameStreamMux, first bit after the 3-byte
    // LOAS header.
    assert_eq!(config_frame[3] & 0x80, 0);
    assert_eq!(same_mux_frame[3] & 0x80, 0x80);
    let both_reversed = [same_mux_frame.clone(), config_frame.clone()].concat();
    let not_loas = build_adts_frame(3, 2, 200);
    let cases: [(&[u8], bool); 4] = [
        (&config_frame, true),
        (&same_mux_frame, false),
        // Judged on the first frame of the push only.
        (&both_reversed, false),
        (&not_loas, false),
    ];
    for (i, (frames, rap)) in cases.iter().enumerate() {
        let mut m = audio_muxer(AudioCodec::AacLatm, true, 100, 10_000);
        m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
            .unwrap();
        m.push_audio(frames, Pts90khz::new(90_000)).unwrap();
        let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
        assert_eq!(starts.len(), 1);
        let flags = starts[0].af_flags.unwrap_or(0);
        assert_eq!(flags & 0x40 != 0, *rap, "case {i}: RAI");
        assert_eq!(flags & 0x10, 0, "case {i}: not the PCR PID");
    }
}

#[test]
fn a_short_latm_push_gets_no_rai_and_does_not_panic() {
    let (config_frame, _) = fixture_loas_frames();
    for n in 0..=3 {
        let mut m = audio_muxer(AudioCodec::AacLatm, true, 100, 10_000);
        m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
            .unwrap();
        m.push_audio(&config_frame[..n], Pts90khz::new(90_000))
            .unwrap();
        let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
        assert_eq!(starts.len(), 1, "{n} bytes");
        assert_eq!(
            starts[0].af_flags.unwrap_or(0) & 0x40,
            0,
            "{n} bytes: no RAI"
        );
    }
}

#[test]
fn an_exact_fit_aac_pes_grows_by_one_packet() {
    // 14 bytes of PES header + 354 = 368 = two whole payloads; 353 leaves
    // one byte, which holds an adaptation field of length 0 and no flags.
    // The flags byte then needs a third packet.
    for len in [353u32, 354] {
        let mut m = audio_muxer(AudioCodec::Aac, true, 100, 10_000);
        m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
            .unwrap();
        m.push_audio(&build_adts_frame(3, 2, len), Pts90khz::new(90_000))
            .unwrap();
        let pkts = audio_pkts(&drain_mux(&mut m));
        let audio: Vec<&AudioPkt> = pkts.iter().filter(|p| p.pid == AUDIO_PID).collect();
        assert_eq!(audio.len(), 3, "{len} bytes");
        assert_eq!(audio[0].af_flags, Some(0x40), "{len} bytes");
    }
}

#[test]
fn an_exact_fit_aac_pes_beside_a_full_queue_is_refused_whole() {
    let frame = build_adts_frame(3, 2, 354);
    // Fill a ten-packet queue to `fill` packets with one-packet video PESs.
    let fill_to = |m: &mut Muxer, fill: u64| {
        m.push_video(&TINY_IDR, Pts90khz::new(90_000), true)
            .unwrap();
        let mut pts = 90_000;
        while m.pending_packets() < fill {
            pts += 1;
            m.push_video(&TINY_IDR, Pts90khz::new(pts), false).unwrap();
        }
        assert_eq!(m.pending_packets(), fill);
    };
    // Three packets reserved, three written: with two free the push is
    // refused and changes nothing.
    let mut m = audio_muxer(AudioCodec::Aac, true, 100, 10);
    fill_to(&mut m, 8);
    let err = m
        .push_audio(&frame, Pts90khz::new(90_000))
        .expect_err("a queue one packet short");
    assert!(matches!(err, MuxError::BufferFull { .. }), "{err:?}");
    assert_eq!(m.pending_packets(), 8);
    let mut fresh = audio_muxer(AudioCodec::Aac, true, 100, 10);
    fill_to(&mut fresh, 8);
    assert_eq!(drain_mux(&mut m), drain_mux(&mut fresh));
    m.push_audio(&frame, Pts90khz::new(90_000)).unwrap();
    fresh.push_audio(&frame, Pts90khz::new(90_000)).unwrap();
    assert_eq!(m.pending_packets(), 3);
    assert_eq!(drain_mux(&mut m), drain_mux(&mut fresh));
    // With exactly three free the grown PES fits the queue.
    let mut m = audio_muxer(AudioCodec::Aac, true, 100, 10);
    fill_to(&mut m, 7);
    m.push_audio(&frame, Pts90khz::new(90_000)).unwrap();
    assert_eq!(m.pending_packets(), 10);
}

#[test]
fn on_an_advancing_timeline_every_aac_pcr_is_pcr_lead_below_its_pes_start() {
    for lead_ms in [0u32, 100, 1_000] {
        let mut m = audio_muxer(AudioCodec::Aac, false, lead_ms, 10_000);
        let frame = build_adts_frame(3, 2, 300);
        let mut ts = Vec::new();
        for i in 0..100 {
            m.push_audio(&frame, Pts90khz::new(900_000 + i * AAC_FRAME_TICKS))
                .unwrap();
            ts.extend(drain_mux(&mut m));
        }
        let pkts = audio_pkts(&ts);
        let pcrs = pkts.iter().filter(|p| p.pcr_base.is_some()).count();
        assert_eq!(pcrs, 100, "lead {lead_ms}: one PCR per frame, nowhere else");
        for p in pkts.iter().filter(|p| p.pid == AUDIO_PID && p.pusi) {
            let pts = p.pts.expect("PES start with a PTS");
            assert_eq!(p.af_flags.map(|f| f & 0x50), Some(0x50), "lead {lead_ms}");
            assert_eq!(
                p.pcr_base,
                Some(pts - u64::from(lead_ms) * 90),
                "lead {lead_ms}: PCR = PTS - lead"
            );
        }
    }
}

#[test]
fn a_held_clock_on_an_audio_pcr_pid_repeats_the_pcr_until_the_stamps_catch_up() {
    // A backward step of 500 ms (under the one-second restart threshold)
    // holds the program clock. Every frame is still a random access point
    // and still carries a PCR, so the held value repeats until the stamps
    // pass it again: 23 frames at 21.3 ms.
    let mut m = audio_muxer(AudioCodec::Aac, false, 100, 10_000);
    let frame = build_adts_frame(3, 2, 300);
    let mut pts_of = Vec::new();
    let mut pts = 900_000i64;
    for i in 0..200 {
        if i == 100 {
            pts -= 45_000;
        }
        pts_of.push(pts as u64);
        m.push_audio(&frame, Pts90khz::new(pts)).unwrap();
        pts += AAC_FRAME_TICKS;
    }
    let starts = pes_starts(&drain_mux(&mut m), AUDIO_PID);
    assert_eq!(starts.len(), 200);
    let pcrs: Vec<u64> = starts
        .iter()
        .map(|s| {
            assert_eq!(s.af_flags.map(|f| f & 0x50), Some(0x50));
            s.pcr_base.unwrap()
        })
        .collect();
    let held = pcrs[99];
    assert_eq!(held, pts_of[99] - 9_000);
    for (i, pcr) in pcrs.iter().enumerate() {
        let expected = if (100..=122).contains(&i) {
            held
        } else {
            pts_of[i] - 9_000
        };
        assert_eq!(*pcr, expected, "frame {i}");
    }
    assert!(pts_of[123] - 9_000 > held && pts_of[122] - 9_000 < held);
}
