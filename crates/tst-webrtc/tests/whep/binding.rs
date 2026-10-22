//! A mount binds KLV records to video frames, carries them in SEI and on
//! the data channel, caches parameter sets and keeps the RTP clock
//! continuous. Driven through the mount's test constructor, with no server.

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::mount::MediaMsg;
use tst_webrtc::{KlvCarriage, MountConfig, MountHandle, PushError, VideoCodec};

const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00];
const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80];
const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00];
const P: &[u8] = &[0, 0, 1, 0x41, 0x9A, 0x01];

fn au(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn next_frame(
    rx: &std::sync::mpsc::Receiver<MediaMsg>,
) -> std::sync::Arc<tst_webrtc::mount::FramePacket> {
    loop {
        match rx.try_recv().expect("message") {
            MediaMsg::Frame { packet, .. } => return packet,
            _ => continue,
        }
    }
}

/// The frame's payloads re-joined into an Annex-B access unit (every test
/// AU is small enough to travel as single NAL unit payloads).
fn rejoin(f: &tst_webrtc::mount::FramePacket) -> Vec<u8> {
    f.payloads
        .iter()
        .flat_map(|p| [0u8, 0, 1].into_iter().chain(p.payload.iter().copied()))
        .collect()
}

#[test]
fn sync_klv_with_frame_pts_lands_in_that_frame_as_sei_and_dc() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_klv(b"\x06\x0e\x2b\x34rec1", Pts90khz::new(9000), 0)
        .unwrap();
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(9000), true)
        .unwrap();
    let f = next_frame(&rx);
    assert!(f.keyframe);
    let au_bytes = rejoin(&f);
    assert_eq!(
        tst_webrtc::sei::extract_klv_seis(&au_bytes, VideoCodec::H264).len(),
        1
    );
    assert!(
        matches!(rx.try_recv().unwrap(), MediaMsg::Klv { msg, .. } if msg.klv_pts_90k == 9000 && msg.rtp_ts == f.rtp_ts)
    );
    assert_eq!(m.stats().klv_sei_inserted, 1);
}

#[test]
fn async_klv_binds_to_first_frame_at_or_after_its_pts() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let _ = next_frame(&rx);
    m.push_klv(b"rec", Pts90khz::new(3500), 0).unwrap();
    // 3000 < 3500: the record does not bind here.
    m.push_video(&au(&[P]), Pts90khz::new(3000), false).unwrap();
    let f1 = next_frame(&rx);
    assert!(rx.try_recv().is_err());
    // The first frame at or after 3500.
    m.push_video(&au(&[P]), Pts90khz::new(6000), false).unwrap();
    let f2 = next_frame(&rx);
    assert!(matches!(rx.try_recv().unwrap(), MediaMsg::Klv { msg, .. } if msg.rtp_ts == f2.rtp_ts));
    assert_ne!(f1.rtp_ts, f2.rtp_ts);
}

#[test]
fn late_klv_binds_to_the_next_frame_never_dropped() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(90_000), true)
        .unwrap();
    let _ = next_frame(&rx);
    // Older than every frame.
    m.push_klv(b"old", Pts90khz::new(1000), 0).unwrap();
    m.push_video(&au(&[P]), Pts90khz::new(93_000), false)
        .unwrap();
    let f = next_frame(&rx);
    assert!(
        matches!(rx.try_recv().unwrap(), MediaMsg::Klv { msg, .. } if msg.klv_pts_90k == 1000 && msg.rtp_ts == f.rtp_ts)
    );
}

#[test]
fn oversize_record_skips_sei_but_rides_the_data_channel() {
    let mut cfg = MountConfig::default();
    cfg.sei_budget_bytes = 1024;
    let (m, rx) = MountHandle::for_test(cfg);
    m.push_klv(&vec![7u8; 100 * 1024], Pts90khz::new(0), 0)
        .unwrap();
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let f = next_frame(&rx);
    let au_bytes = rejoin(&f);
    assert!(tst_webrtc::sei::extract_klv_seis(&au_bytes, VideoCodec::H264).is_empty());
    assert!(
        matches!(rx.try_recv().unwrap(), MediaMsg::Klv { msg, .. } if msg.klv.len() == 100 * 1024)
    );
    assert_eq!(m.stats().klv_sei_dropped_oversize, 1);
}

#[test]
fn record_over_data_channel_limit_is_refused() {
    let (m, _rx) = MountHandle::for_test(MountConfig::default());
    assert!(matches!(
        m.push_klv(&vec![0u8; 256 * 1024 + 1], Pts90khz::new(0), 0),
        Err(PushError::KlvTooLarge {
            len: 262_145,
            limit: 262_144
        })
    ));
    assert_eq!(m.stats().klv_dc_dropped_oversize, 1);
    m.push_klv(&vec![0u8; 256 * 1024], Pts90khz::new(0), 0)
        .expect("a record at the limit is accepted");
    assert_eq!(m.stats().klv_records_pushed, 1);
}

#[test]
fn keyframe_without_parameter_sets_ever_is_not_an_error_and_not_a_keyframe() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[IDR]), Pts90khz::new(0), true).unwrap();
    assert!(!next_frame(&rx).keyframe);
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(3000), true)
        .unwrap();
    assert!(next_frame(&rx).keyframe);
}

#[test]
fn cached_parameter_sets_are_prepended_to_a_bare_idr() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let _ = next_frame(&rx);
    m.push_video(&au(&[IDR]), Pts90khz::new(3000), true)
        .unwrap();
    let f = next_frame(&rx);
    assert_eq!(f.payloads[0].payload[0], 0x67);
    assert_eq!(f.payloads[1].payload[0], 0x68);
    assert!(f.keyframe);
}

#[test]
fn a_pts_step_backwards_within_the_rebase_window_is_counted_not_rebased() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let a = next_frame(&rx);
    m.push_video(&au(&[P]), Pts90khz::new(6000), false).unwrap();
    let b = next_frame(&rx);
    // A B-frame: presented before the frame pushed before it.
    m.push_video(&au(&[P]), Pts90khz::new(3000), false).unwrap();
    let c = next_frame(&rx);
    assert_eq!(b.rtp_ts.wrapping_sub(a.rtp_ts), 6000);
    assert_eq!(c.rtp_ts.wrapping_sub(a.rtp_ts), 3000);
    let s = m.stats();
    assert_eq!(s.frames_pts_reordered, 1);
    assert_eq!(s.rtp_clock_rebases, 0);
    // The same PTS again is not a step backwards.
    m.push_video(&au(&[P]), Pts90khz::new(3000), false).unwrap();
    let _ = next_frame(&rx);
    assert_eq!(m.stats().frames_pts_reordered, 1);
}

#[test]
fn idr_nal_with_false_flag_counts_as_keyframe_and_mismatch() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), false)
        .unwrap();
    assert!(next_frame(&rx).keyframe);
    assert_eq!(m.stats().keyframe_flag_mismatch, 1);
}

#[test]
fn pts_jump_backwards_rebases_the_rtp_clock_continuously() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(8_000_000_000), true)
        .unwrap();
    let a = next_frame(&rx);
    // The source wrapped or restarted.
    m.push_video(&au(&[P]), Pts90khz::new(0), false).unwrap();
    let b = next_frame(&rx);
    assert_eq!(b.rtp_ts.wrapping_sub(a.rtp_ts), 3000);
    let s = m.stats();
    assert_eq!(s.rtp_clock_rebases, 1);
    // A rebase is a restart, not a reordered frame.
    assert_eq!(s.frames_pts_reordered, 0);
}

#[test]
fn empty_au_is_invalid_and_closed_mount_refuses() {
    let (m, _rx) = MountHandle::for_test(MountConfig::default());
    assert!(matches!(
        m.push_video(&[], Pts90khz::new(0), false),
        Err(PushError::InvalidAccessUnit { .. })
    ));
    m.close_for_test();
    assert!(matches!(
        m.push_video(&au(&[P]), Pts90khz::new(0), false),
        Err(PushError::Closed)
    ));
}

#[test]
fn elementary_mount_refuses_push_ts() {
    let (m, _rx) = MountHandle::for_test(MountConfig::default());
    assert!(matches!(
        m.push_ts(&[0x47; 188]),
        Err(PushError::WrongMountKind)
    ));
}

#[test]
fn misp_timestamp_sei_in_the_source_au_is_preserved_bytewise() {
    use tst_core::codec::misp_time::{MispTimestamp, build_sei_nal, extract};
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    let misp = build_sei_nal(
        VideoCodec::H264,
        &MispTimestamp::micros(1_700_000_000_000_000, 0x1F),
    )
    .unwrap();
    let mut src = au(&[SPS, PPS]);
    src.extend([0, 0, 1]);
    src.extend(&misp);
    src.extend(IDR);
    m.push_klv(b"rec", Pts90khz::new(0), 0).unwrap();
    m.push_video(&src, Pts90khz::new(0), true).unwrap();
    let f = next_frame(&rx);
    let au_bytes = rejoin(&f);
    assert_eq!(
        au_bytes
            .windows(misp.len())
            .filter(|w| *w == misp.as_slice())
            .count(),
        1
    );
    assert_eq!(
        extract(&au_bytes, VideoCodec::H264).unwrap().unwrap().value,
        1_700_000_000_000_000
    );
    assert_eq!(
        tst_webrtc::sei::extract_klv_seis(&au_bytes, VideoCodec::H264).len(),
        1
    );
}

#[test]
fn sei_only_carriage_sends_no_data_channel_message() {
    let mut cfg = MountConfig::default();
    cfg.klv = KlvCarriage::SeiOnly;
    let (m, rx) = MountHandle::for_test(cfg);
    m.push_klv(b"rec", Pts90khz::new(0), 0).unwrap();
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let _ = next_frame(&rx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_mount_without_a_server_has_no_sessions_and_drops_audio() {
    let (m, _rx) = MountHandle::for_test(MountConfig::default());
    assert_eq!(m.mount_path(), "test");
    assert_eq!(m.config().video, VideoCodec::H264);
    assert!(m.sessions().is_empty());
    assert_eq!(m.stats(), tst_webrtc::MountStats::default());
    m.push_audio(&[0xFC, 0xFF], Pts90khz::new(0)).unwrap();
    assert_eq!(m.stats().audio_frames_dropped, 1);
}

const PPS2: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];

#[test]
fn a_partial_parameter_set_replaces_only_its_own_kind() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let _ = next_frame(&rx);
    // Only a new PPS: the cached SPS is prepended, the new PPS kept once.
    m.push_video(&au(&[PPS2, IDR]), Pts90khz::new(3000), true)
        .unwrap();
    let f = next_frame(&rx);
    assert!(f.keyframe);
    let firsts: Vec<&[u8]> = f.payloads.iter().map(|p| p.payload.as_slice()).collect();
    assert_eq!(firsts, vec![&SPS[4..], &PPS2[4..], &IDR[3..]]);
    // A bare IDR now gets the SPS and the new PPS.
    m.push_video(&au(&[IDR]), Pts90khz::new(6000), true)
        .unwrap();
    let f = next_frame(&rx);
    let firsts: Vec<&[u8]> = f.payloads.iter().map(|p| p.payload.as_slice()).collect();
    assert_eq!(firsts, vec![&SPS[4..], &PPS2[4..], &IDR[3..]]);
}

#[test]
fn the_1025th_waiting_record_drops_the_oldest() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    for i in 0..1025u32 {
        m.push_klv(&i.to_be_bytes(), Pts90khz::new(0), 0).unwrap();
    }
    assert_eq!(m.stats().klv_records_dropped_unbound, 1);
    assert_eq!(m.stats().klv_records_pushed, 1025);
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let _ = next_frame(&rx);
    let klvs: Vec<Vec<u8>> = rx
        .try_iter()
        .filter_map(|msg| match msg {
            MediaMsg::Klv { msg, .. } => Some(msg.klv.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(klvs.len(), 1024);
    assert_eq!(klvs[0], 1u32.to_be_bytes());
    assert_eq!(klvs[1023], 1024u32.to_be_bytes());
}

const H265_VPS: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
const H265_SPS: &[u8] = &[0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01];
const H265_PPS: &[u8] = &[0, 0, 0, 1, 0x44, 0x01, 0xC1, 0x72];
/// A CRA picture (nal_unit_type 21), an IRAP picture that is not an IDR.
const H265_CRA: &[u8] = &[0, 0, 1, 0x2A, 0x01, 0xAF, 0x05];

#[test]
fn h265_cra_is_a_keyframe_and_a_bare_cra_gets_cached_parameter_sets() {
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::H265;
    let (m, rx) = MountHandle::for_test(cfg);
    m.push_video(
        &au(&[H265_VPS, H265_SPS, H265_PPS, H265_CRA]),
        Pts90khz::new(0),
        true,
    )
    .unwrap();
    let f = next_frame(&rx);
    assert!(f.keyframe);
    assert_eq!(m.stats().keyframe_flag_mismatch, 0);
    m.push_video(&au(&[H265_CRA]), Pts90khz::new(3000), true)
        .unwrap();
    let f = next_frame(&rx);
    assert!(f.keyframe);
    let firsts: Vec<&[u8]> = f.payloads.iter().map(|p| p.payload.as_slice()).collect();
    assert_eq!(
        firsts,
        vec![
            &H265_VPS[4..],
            &H265_SPS[4..],
            &H265_PPS[4..],
            &H265_CRA[3..]
        ]
    );
}

#[test]
fn the_sei_budget_counts_the_escaped_size_of_a_record() {
    // A record of zero bytes grows by an escape byte every second zero, so
    // an unescaped size estimate would let it through a tight budget.
    let record = [0u8; 200];
    let sei = tst_webrtc::sei::build_klv_sei_nal(VideoCodec::H264, 9000, &record).unwrap();
    let actual = (sei.len() + 3) as u32;
    assert!(actual as usize > record.len() + 25 + 8, "escaping grew it");

    let run = |budget: u32| {
        let mut cfg = MountConfig::default();
        cfg.sei_budget_bytes = budget;
        let (m, _rx) = MountHandle::for_test(cfg);
        m.push_klv(&record, Pts90khz::new(9000), 0).unwrap();
        m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(9000), true)
            .unwrap();
        m.stats()
    };
    let tight = run(actual - 1);
    assert_eq!(
        (tight.klv_sei_inserted, tight.klv_sei_dropped_oversize),
        (0, 1)
    );
    let exact = run(actual);
    assert_eq!(
        (exact.klv_sei_inserted, exact.klv_sei_dropped_oversize),
        (1, 0)
    );
}

#[test]
fn records_bound_after_a_rebase_share_one_splice() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(90_000_000), true)
        .unwrap();
    let _ = next_frame(&rx);
    for i in 0..4u8 {
        m.push_klv(&[i; 8], Pts90khz::new(90_000_000 + 100 * i64::from(i)), 0)
            .unwrap();
    }
    // A PTS far behind rebases the clock and binds every waiting record.
    m.push_video(&au(&[P]), Pts90khz::new(0), false).unwrap();
    let f = next_frame(&rx);
    let seis = tst_webrtc::sei::extract_klv_seis(&rejoin(&f), VideoCodec::H264);
    assert_eq!(seis.len(), 4);
    assert_eq!(m.stats().klv_sei_inserted, 4);
}

/// Two threads pushing through clones of one handle: frames reach the media
/// queue in the order the clock stamped them. Each frame the clock saw
/// below its predecessor counts in `frames_pts_reordered`, so the number of
/// timestamp steps backwards in the queue equals that counter exactly when
/// queue order is clock order. The producers push fewer frames than the
/// queue holds and the queue is drained after they finish, so no frame is
/// dropped and nothing depends on scheduling.
#[test]
fn concurrent_pushes_are_queued_in_clock_order() {
    const PER_THREAD: u64 = 2_000;
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    m.push_video(&au(&[SPS, PPS, IDR]), Pts90khz::new(0), true)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let producers: Vec<_> = (0..2u64)
        .map(|lane| {
            let m = m.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..PER_THREAD {
                    let pts = (1 + 2 * k + lane) * 3000;
                    m.push_video(P, Pts90khz::new(pts as i64), false).unwrap();
                }
            })
        })
        .collect();
    for t in producers {
        t.join().unwrap();
    }
    assert_eq!(m.stats().viewer_packets_dropped_queue_full, 0);
    let mut stamps = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let MediaMsg::Frame { packet, .. } = msg {
            stamps.push(packet.rtp_ts);
        }
    }
    assert_eq!(stamps.len() as u64, 1 + 2 * PER_THREAD);
    let backwards = stamps
        .windows(2)
        .filter(|w| (w[1].wrapping_sub(w[0]) as i32) < 0)
        .count() as u64;
    assert_eq!(backwards, m.stats().frames_pts_reordered);
    // Each producer's own frames keep their push order. The clock's origin
    // is random, so stamps are read relative to the keyframe's (PTS 0),
    // which also keeps a 32-bit wrap inside the run from reordering them.
    let origin = stamps[0];
    for lane in 0..2u32 {
        let own: Vec<u32> = stamps
            .iter()
            .map(|ts| ts.wrapping_sub(origin))
            .filter(|rel| (rel / 3000) % 2 == 1 - lane)
            .collect();
        assert!(own.windows(2).all(|w| w[0] < w[1]), "lane {lane}");
    }
}
