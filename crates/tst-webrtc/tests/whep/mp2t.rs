//! An MP2T mount demultiplexes pushed TS bytes into the elementary video and
//! KLV paths. The TS is built here with the muxer; no sockets are involved.

use std::sync::mpsc::Receiver;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_webrtc::mount::MediaMsg;
use tst_webrtc::{KlvCarriage, MountConfig, MountHandle, MountKind, PushError, VideoCodec};

/// The 30-frame GOP: IDR first, then 29 P frames, each opened by an AUD.
const GOP: &[u8] = include_bytes!("../fixtures/h264/gop.h264");

const AU_COUNT: usize = 10;
const TICKS_PER_FRAME: i64 = 3000;

/// The first `n` access units of the GOP, split at each access unit
/// delimiter (NAL type 9).
fn access_units(n: usize) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < GOP.len() {
        let sc = if GOP[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if GOP[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if GOP[i + sc] & 0x1F == 9 {
            starts.push(i);
        }
        i += sc;
    }
    starts.push(GOP.len());
    starts
        .windows(2)
        .take(n)
        .map(|w| GOP[w[0]..w[1]].to_vec())
        .collect()
}

fn record(i: usize) -> Vec<u8> {
    format!("klv-record-{i:03}").into_bytes()
}

/// A TS of `n` access units, each preceded by its KLV record at the access
/// unit's PTS.
fn build_ts(n: usize) -> Vec<u8> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, MuxVideoCodec::H264);
    prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    let mut cfg = MuxerConfig::builder();
    cfg.add_program(prog.build());
    let mut mux = Muxer::new(cfg.build().unwrap()).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    for (i, au) in access_units(n).iter().enumerate() {
        let pts = Pts90khz::new(90_000 + i as i64 * TICKS_PER_FRAME);
        mux.push_klv(&record(i), pts, 0).unwrap();
        mux.push_video(au, pts, i == 0).unwrap();
        loop {
            let got = mux.pull(&mut buf);
            if got == 0 {
                break;
            }
            out.extend_from_slice(&buf[..got]);
        }
    }
    out
}

fn mp2t_config() -> MountConfig {
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    cfg.klv = KlvCarriage::SeiAndDataChannel;
    cfg
}

struct Drained {
    /// (rtp_ts, keyframe, Annex-B access unit) per frame.
    frames: Vec<(u32, bool, Vec<u8>)>,
    /// (rtp_ts, record) per data-channel message.
    klvs: Vec<(u32, Vec<u8>)>,
}

fn drain(rx: &Receiver<MediaMsg>) -> Drained {
    let mut d = Drained {
        frames: Vec::new(),
        klvs: Vec::new(),
    };
    while let Ok(msg) = rx.try_recv() {
        match msg {
            MediaMsg::Frame { packet, .. } => {
                let au = packet
                    .payloads
                    .iter()
                    .flat_map(|p| [0u8, 0, 1].into_iter().chain(p.payload.iter().copied()))
                    .collect();
                d.frames.push((packet.rtp_ts, packet.keyframe, au));
            }
            MediaMsg::Klv { msg, .. } => d.klvs.push((msg.rtp_ts, msg.klv.clone())),
            _ => {}
        }
    }
    d
}

#[test]
fn ts_bytes_become_frames_with_their_klv_bound() {
    // One access unit more than asserted: the demuxer emits an access unit
    // when the next one starts, so the last one waits for more bytes.
    let ts = build_ts(AU_COUNT + 1);
    let (m, rx) = MountHandle::for_test(mp2t_config());
    for slice in ts.chunks(1316) {
        m.push_ts(slice).unwrap();
    }
    let d = drain(&rx);
    assert_eq!(d.frames.len(), AU_COUNT);
    assert_eq!(d.klvs.len(), AU_COUNT);
    assert!(d.frames[0].1, "the IDR starts the stream");
    for (i, ((frame_ts, _, au), (klv_ts, klv))) in d.frames.iter().zip(&d.klvs).enumerate() {
        assert_eq!(frame_ts, klv_ts, "record {i} binds to frame {i}");
        assert_eq!(klv, &record(i));
        let seis = tst_webrtc::sei::extract_klv_seis(au, VideoCodec::H264);
        assert_eq!(seis.len(), 1, "frame {i}");
        assert_eq!(seis[0].klv, record(i));
        assert_eq!(
            seis[0].klv_pts_90k,
            90_000 + i as u64 * TICKS_PER_FRAME as u64
        );
    }
    let rtp: Vec<u32> = d.frames.iter().map(|f| f.0).collect();
    assert!(
        rtp.windows(2)
            .all(|w| w[1] - w[0] == TICKS_PER_FRAME as u32)
    );
    let stats = m.stats();
    assert_eq!(stats.frames_pushed, AU_COUNT as u64);
    // The last record waits unbound for a frame that has not been emitted.
    assert_eq!(stats.klv_records_pushed, AU_COUNT as u64 + 1);
    assert_eq!(stats.klv_sei_inserted, AU_COUNT as u64);
    assert_eq!(stats.demux_nonconformant, 0);
}

#[test]
fn slices_that_split_packets_give_the_same_frames() {
    let ts = build_ts(AU_COUNT + 1);
    let (whole, rx_whole) = MountHandle::for_test(mp2t_config());
    whole.push_ts(&ts).unwrap();
    let (split, rx_split) = MountHandle::for_test(mp2t_config());
    // 100 is neither a divisor of 188 nor of any packet run.
    for slice in ts.chunks(100) {
        split.push_ts(slice).unwrap();
    }
    let a = drain(&rx_whole);
    let b = drain(&rx_split);
    assert_eq!(a.frames.len(), AU_COUNT);
    // Each mount starts its RTP clock at its own random base: compare the
    // access units and the offsets from the first frame.
    let shape = |d: &Drained| {
        let base = d.frames[0].0;
        let frames: Vec<_> = d
            .frames
            .iter()
            .map(|(ts, key, au)| (ts.wrapping_sub(base), *key, au.clone()))
            .collect();
        let klvs: Vec<_> = d
            .klvs
            .iter()
            .map(|(ts, k)| (ts.wrapping_sub(base), k.clone()))
            .collect();
        (frames, klvs)
    };
    assert_eq!(shape(&a), shape(&b));
}

#[test]
fn a_video_codec_other_than_the_mounts_is_counted_and_skipped() {
    let ts = build_ts(AU_COUNT + 1);
    let mut cfg = mp2t_config();
    cfg.video = VideoCodec::H265;
    let (m, rx) = MountHandle::for_test(cfg);
    m.push_ts(&ts).unwrap();
    let d = drain(&rx);
    assert!(d.frames.is_empty());
    assert_eq!(m.stats().frames_pushed, 0);
    assert_eq!(m.stats().demux_nonconformant, AU_COUNT as u64);
}

#[test]
fn garbage_bytes_are_not_a_push_error() {
    let (m, rx) = MountHandle::for_test(mp2t_config());
    m.push_ts(&[0xFF; 200]).unwrap();
    assert!(drain(&rx).frames.is_empty());
}

#[test]
fn push_families_must_match_the_mount_kind() {
    let (elementary, _rx) = MountHandle::for_test(MountConfig::default());
    assert!(matches!(
        elementary.push_ts(&[0x47; 188]),
        Err(PushError::WrongMountKind)
    ));
    let (mp2t, _rx) = MountHandle::for_test(mp2t_config());
    assert!(matches!(
        mp2t.push_video(&[0, 0, 1, 0x65, 0x88], Pts90khz::new(0), true),
        Err(PushError::WrongMountKind)
    ));
    assert!(matches!(
        mp2t.push_klv(b"x", Pts90khz::new(0), 0),
        Err(PushError::WrongMountKind)
    ));
    assert!(matches!(
        mp2t.push_audio(b"x", Pts90khz::new(0)),
        Err(PushError::WrongMountKind)
    ));
}

#[test]
fn a_closed_mount_refuses_ts() {
    let (m, _rx) = MountHandle::for_test(mp2t_config());
    m.close_for_test();
    assert!(matches!(m.push_ts(&[0x47; 188]), Err(PushError::Closed)));
}

/// An ADTS AAC-LC frame (44.1 kHz, stereo) with a zero payload.
fn adts_frame() -> Vec<u8> {
    let len = 7 + 64;
    let mut f = vec![
        0xFF,
        0xF1,
        0x50,
        0x80 | ((len >> 11) & 0x3) as u8,
        ((len >> 3) & 0xFF) as u8,
        (((len & 0x7) << 5) | 0x1F) as u8,
        0xFC,
    ];
    f.extend(std::iter::repeat_n(0u8, 64));
    f
}

#[test]
fn ts_audio_is_counted_and_discarded() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, MuxVideoCodec::H264);
    prog.add_audio(0x1021, tst_core::codec::AudioCodec::Aac);
    let mut cfg = MuxerConfig::builder();
    cfg.add_program(prog.build());
    let mut mux = Muxer::new(cfg.build().unwrap()).unwrap();
    for i in 0..3 {
        mux.push_audio(&adts_frame(), Pts90khz::new(90_000 + i * 1920))
            .unwrap();
    }
    let mut ts = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    loop {
        let got = mux.pull(&mut buf);
        if got == 0 {
            break;
        }
        ts.extend_from_slice(&buf[..got]);
    }
    let (m, rx) = MountHandle::for_test(mp2t_config());
    m.push_ts(&ts).unwrap();
    // Closing the stream is not possible; a later PES start emits the
    // previous audio, so push the same TS again.
    m.push_ts(&ts).unwrap();
    let s = m.stats();
    assert!(s.audio_frames_dropped >= 1, "{s:?}");
    assert_eq!(s.audio_frames_pushed, 0);
    assert!(drain(&rx).frames.is_empty());
}
