//! Elementary H.264 + KLV publisher in GStreamer's shape: two tracks
//! (`stream=0` H.264, `stream=1` RFC 6597 `smpte336m/90000`), each SETUP
//! over TCP-interleaved on its own channel pair. The server re-muxes video
//! on PID 0x100 and KLV on PID 0x101, placing each KLV unit on the video
//! PTS line from the two tracks' RTCP sender reports (spec §2 "Track
//! alignment"), or by first-packet coincidence when the reports never come.

use std::time::{Duration, Instant};

use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_pipeline::DemuxReceiver;
use tst_rtp::{ClockAlignment, PublishMountHandle, PublishMountStats, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

const VIDEO_PID: u16 = 0x100;
const KLV_PID: u16 = 0x101;
const VIDEO_PT: u8 = 96;
const KLV_PT: u8 = 97;
const VIDEO_SSRC: u32 = 0x1111_0001;
const KLV_SSRC: u32 = 0x2222_0002;
/// Video RTP timestamp of AU 0 — the depacketizer's PTS zero.
const VIDEO_TS0: u32 = 1_000_000;
/// KLV RTP timestamp of unit 0 (an unrelated random origin).
const KLV_TS0: u32 = 500_000;
/// KLV units every 100 ms on their own 90 kHz clock.
const KLV_STEP: u32 = 9_000;
const AUS: usize = 28;
const NTP_SECS: u32 = 3_900_000_000;

/// Video SR: NTP `NTP_SECS.0` ↔ video RTP `VIDEO_TS0 + 1000`.
const VIDEO_SR: (u64, u32) = ((NTP_SECS as u64) << 32, VIDEO_TS0 + 1_000);
/// KLV SR: NTP `NTP_SECS.5` ↔ KLV RTP `KLV_TS0`.
const KLV_SR: (u64, u32) = (((NTP_SECS as u64) << 32) | (1 << 31), KLV_TS0);

/// Spec §2's formula, computed independently of the server:
///
/// ```text
/// ntp_k     = ntp_klv_sr + (t_k - rtp_klv_sr) / 90000
/// video_rtp = rtp_video_sr + (ntp_k - ntp_video_sr) * 90000
/// pts_k     = video_rtp - video_rtp_origin
/// ```
///
/// in 32.32 NTP fixed point, rounded down to whole ticks.
fn spec_klv_pts(t_k: u32) -> i64 {
    let (ntp_v, rtp_v) = (VIDEO_SR.0 as i128, VIDEO_SR.1 as i128);
    let (ntp_k_sr, rtp_k_sr) = (KLV_SR.0 as i128, KLV_SR.1 as i128);
    let ntp_k = ntp_k_sr + (((t_k as i128 - rtp_k_sr) << 32) / 90_000);
    let video_rtp = rtp_v + (((ntp_k - ntp_v) * 90_000) >> 32);
    (video_rtp - VIDEO_TS0 as i128) as i64
}

fn video_pts(events: &[DemuxEvent]) -> Vec<i64> {
    events
        .iter()
        .filter_map(|ev| match ev {
            DemuxEvent::Sample {
                stream,
                pts,
                payload: SamplePayload::Video { .. },
                ..
            } if stream.pid == VIDEO_PID => Some(pts.as_ticks()),
            _ => None,
        })
        .collect()
}

/// `(pts ticks, payload)` of every KLV `Metadata` event on PID 0x101.
fn klv_events(events: &[DemuxEvent]) -> Vec<(i64, Vec<u8>)> {
    events
        .iter()
        .filter_map(|ev| match ev {
            DemuxEvent::Metadata {
                stream,
                pts,
                payload,
                ..
            } if stream.pid == KLV_PID => Some((pts.as_ticks(), payload.clone())),
            _ => None,
        })
        .collect()
}

/// Poll `mount.stats()` until `pred` holds or `within` passes; returns the
/// last snapshot either way (the caller asserts on it).
fn stats_when(
    mount: &PublishMountHandle,
    within: Duration,
    pred: impl Fn(&PublishMountStats) -> bool,
) -> PublishMountStats {
    let deadline = Instant::now() + within;
    loop {
        let s = mount.stats();
        if pred(&s) || Instant::now() >= deadline {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A publisher on `/pub` that has announced [`SDP_H264_KLV`], SETUP both
/// tracks over interleaved, and RECORDed. Returns it with the video and
/// KLV `(rtp, rtcp)` channel pairs the server allocated.
fn two_track_publisher(port: u16) -> (RawPublisher, (u8, u8), (u8, u8)) {
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_H264_KLV), 200);
    let (status, video) = p.setup_interleaved_pair("/pub", "stream=0");
    assert_eq!(status, 200);
    let (status, klv) = p.setup_interleaved_pair("/pub", "stream=1");
    assert_eq!(status, 200);
    let (video, klv) = (video.expect("video pair"), klv.expect("klv pair"));
    let used = [video.0, video.1, klv.0, klv.1];
    for (i, c) in used.iter().enumerate() {
        assert!(!used[i + 1..].contains(c), "channels overlap: {used:?}");
    }
    assert_eq!(p.record("/pub"), 200);
    (p, video, klv)
}

/// Sender reports for both tracks before any media → every KLV unit
/// lands at the spec-formula PTS relative to the first video sample
/// (±1 tick), byte-identical, and the mount reports `SenderReport`.
#[test]
fn klv_is_aligned_to_video_by_sender_reports() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app_t = mount.clone().into_recv_transport().unwrap();
    app_t.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut app = DemuxReceiver::new(app_t);

    let (mut p, (v_rtp, v_rtcp), (k_rtp, k_rtcp)) = two_track_publisher(port);
    p.send_frame(v_rtcp, &sr_packet(VIDEO_SSRC, NTP_SECS, 0, VIDEO_SR.1));
    p.send_frame(k_rtcp, &sr_packet(KLV_SSRC, NTP_SECS, 1 << 31, KLV_SR.1));

    // Interleave the two tracks in presentation order, as a live sender
    // would: video packets keyed by their RTP offset from AU 0, KLV units
    // by the PTS the formula puts them at.
    let units: Vec<Vec<u8>> = (0..10).map(klv_unit).collect();
    let mut wire: Vec<(i64, u8, Vec<u8>)> =
        h264_rtp_packets(AUS, VIDEO_PT, 7, VIDEO_SSRC, VIDEO_TS0)
            .into_iter()
            .map(|pkt| {
                let ts = u32::from_be_bytes(pkt[4..8].try_into().unwrap());
                (i64::from(ts - VIDEO_TS0), v_rtp, pkt)
            })
            .collect();
    for (j, unit) in units.iter().enumerate() {
        let t_k = KLV_TS0 + j as u32 * KLV_STEP;
        wire.push((
            spec_klv_pts(t_k),
            k_rtp,
            klv_rtp_packet(900 + j as u16, t_k, KLV_SSRC, KLV_PT, unit),
        ));
    }
    wire.sort_by_key(|(at, _, _)| *at); // stable: video before KLV on a tie
    for (_, ch, pkt) in &wire {
        p.send_frame(*ch, pkt);
    }

    let events = demux_until_quiet(&mut app, Instant::now() + Duration::from_secs(20), "app");
    let video = video_pts(&events);
    assert!(video.len() >= 10, "only {} video samples", video.len());
    let klv = klv_events(&events);
    assert_eq!(klv.len(), 10, "KLV units demuxed on PID 0x101");
    for (j, (pts, payload)) in klv.iter().enumerate() {
        let t_k = KLV_TS0 + j as u32 * KLV_STEP;
        let want = spec_klv_pts(t_k);
        let got = pts - video[0];
        assert!(
            (got - want).abs() <= 1,
            "KLV unit {j}: PTS {got} past the first video sample, spec formula says {want}"
        );
        assert_eq!(payload, &units[j], "KLV unit {j} bytes");
    }

    let stats = mount.stats();
    assert_eq!(stats.alignment, ClockAlignment::SenderReport);
    assert_eq!(stats.klv_units_emitted, 10);
    assert_eq!(stats.klv_units_dropped, 0);
    assert_eq!(stats.alignment_steps, 0);
    assert_eq!(stats.aus_emitted, AUS as u64);
    assert_eq!(stats.malformed_packets, 0);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}

/// No sender reports: KLV units are held, then — once a unit arrives two
/// seconds after the first held one — placed by first-packet coincidence
/// (the first unit at the first video sample's PTS), and the mount
/// reports `Provisional`. The publisher keeps sending a unit every 250 ms
/// the way a live KLV source would, which is what drives the fallback.
/// Then the reports arrive after all: the next unit lands on the report
/// mapping, the mode becomes `SenderReport`, and the switch is one step
/// (GStreamer's first RTCP interval is randomized, so this can happen).
///
/// The first phase assumes the server handles the first five units
/// within two seconds of each other; a runner stalled longer than that
/// would engage the fallback early and fail the "held" assertion.
#[test]
fn klv_falls_back_to_provisional_alignment_without_sender_reports() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app_t = mount.clone().into_recv_transport().unwrap();
    app_t.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut app = DemuxReceiver::new(app_t);

    let (mut p, (v_rtp, v_rtcp), (k_rtp, k_rtcp)) = two_track_publisher(port);
    for pkt in &h264_rtp_packets(AUS, VIDEO_PT, 7, VIDEO_SSRC, VIDEO_TS0) {
        p.send_frame(v_rtp, pkt);
    }
    let mut units = Vec::new();
    let send_klv = |p: &mut RawPublisher, units: &mut Vec<Vec<u8>>| {
        let j = units.len() as u32;
        let unit = klv_unit(j);
        p.send_frame(
            k_rtp,
            &klv_rtp_packet(
                900 + j as u16,
                KLV_TS0 + j * KLV_STEP,
                KLV_SSRC,
                KLV_PT,
                &unit,
            ),
        );
        units.push(unit);
    };
    for _ in 0..5 {
        send_klv(&mut p, &mut units);
    }

    // Everything sent so far has been processed, and the KLV is held.
    let s = stats_when(&mount, Duration::from_secs(5), |s| {
        s.rtp_packets_received == 30 + 5
    });
    assert_eq!(s.rtp_packets_received, 35);
    assert_eq!(s.klv_units_emitted, 0, "KLV must be held without reports");
    assert_eq!(s.alignment, ClockAlignment::NotApplicable);

    // Keep the KLV source live until the two-second fallback engages.
    let deadline = Instant::now() + Duration::from_secs(5);
    while mount.stats().alignment != ClockAlignment::Provisional {
        assert!(
            Instant::now() < deadline,
            "alignment never fell back to Provisional ({} units sent)",
            units.len()
        );
        std::thread::sleep(Duration::from_millis(250));
        send_klv(&mut p, &mut units);
    }
    let sent = units.len() as u64;
    let s = stats_when(&mount, Duration::from_secs(5), |s| {
        s.klv_units_emitted == sent
    });
    assert_eq!(s.klv_units_emitted, sent, "every held unit is released");
    assert_eq!(s.klv_units_dropped, 0);
    assert_eq!(s.alignment, ClockAlignment::Provisional);

    let events = demux_until_quiet(&mut app, Instant::now() + Duration::from_secs(20), "app");
    let video = video_pts(&events);
    assert!(video.len() >= 10, "only {} video samples", video.len());
    let klv = klv_events(&events);
    assert_eq!(
        klv.len(),
        sent as usize,
        "every released KLV unit is demuxed on PID 0x101"
    );
    for (j, (pts, payload)) in klv.iter().enumerate() {
        // First-packet coincidence: unit 0 ↔ the video origin.
        assert_eq!(
            pts - video[0],
            i64::from(j as u32 * KLV_STEP),
            "KLV unit {j} PTS past the first video sample"
        );
        assert_eq!(payload, &units[j], "KLV unit {j} bytes");
    }

    // The reports arrive late; the next unit follows them.
    p.send_frame(v_rtcp, &sr_packet(VIDEO_SSRC, NTP_SECS, 0, VIDEO_SR.1));
    p.send_frame(k_rtcp, &sr_packet(KLV_SSRC, NTP_SECS, 1 << 31, KLV_SR.1));
    let j = units.len() as u32;
    send_klv(&mut p, &mut units);
    let s = stats_when(&mount, Duration::from_secs(5), |s| {
        s.klv_units_emitted == sent + 1
    });
    assert_eq!(s.klv_units_emitted, sent + 1);
    assert_eq!(s.alignment, ClockAlignment::SenderReport);
    assert_eq!(s.alignment_steps, 1);
    let events = demux_until_quiet(&mut app, Instant::now() + Duration::from_secs(20), "app");
    let klv = klv_events(&events);
    assert_eq!(klv.len(), 1);
    let want = spec_klv_pts(KLV_TS0 + j * KLV_STEP);
    assert!(
        (klv[0].0 - video[0] - want).abs() <= 1,
        "late-report KLV PTS {} past the first video sample, spec formula says {want}",
        klv[0].0 - video[0]
    );
    assert_eq!(klv[0].1, units[j as usize]);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
