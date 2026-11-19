//! An H.264 + KLV push: the server aligns the two tracks from the push's
//! sender reports, and every KLV unit comes out of the mount's stream
//! whole, at the PTS it was sent with on the video's timeline.

use tst_core::mpegts::common::Pts90khz;
use tst_rtp::ClockAlignment;

use crate::common::*;
use crate::fixtures::publish_mount::{klv_events, video_samples};
use crate::fixtures::raw_rtsp_publisher::klv_unit;

#[test]
fn an_h264_and_klv_push_delivers_its_access_units_and_klv_units() {
    const UNITS: u64 = 30;
    let s = Served::new();
    let mut push = s.push(&elementary(true));
    for i in 0..UNITS {
        let pts = Pts90khz::new(i as i64 * 3003);
        // A key frame with its parameter sets first, then P slices, each
        // large enough to be fragmented.
        let mut au = Vec::new();
        let slice_type = if i == 0 { 0x65 } else { 0x41 };
        let slice: Vec<u8> = std::iter::once(slice_type)
            .chain((0..3_000).map(|k| (k % 250) as u8 + 1))
            .collect();
        let (sps, pps) = (sps(), pps());
        let nals: &[&[u8]] = if i == 0 {
            &[&sps, &pps, &slice]
        } else {
            &[&slice]
        };
        for nal in nals {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(nal);
        }
        push.send_video(&au, pts).unwrap();
        push.send_klv(&[0x06, 0x0E, 0x2B, 0x34, i as u8], pts)
            .unwrap();
    }
    // The last access unit is held until the next one starts.
    let stats = s.stats_when("every access unit and KLV unit but the last", |st| {
        st.aus_emitted >= UNITS - 1 && st.klv_units_emitted >= UNITS - 1
    });
    assert_eq!(stats.malformed_packets, 0);
    push.close().unwrap();
}

/// KLV units half a frame after their access unit, so the server has to
/// place them from the reports, not on a video PTS.
const KLV_OFFSET: i64 = AU_STEP / 2;

/// Push `aus` access units with one KLV unit each, KLV first when
/// `klv_first`, to a mount that ends with the push, and check what the
/// mount put out: the mount aligned the tracks from the reports, and every
/// KLV unit is the unit sent, `KLV_OFFSET` after the PTS of its access
/// unit, measured from the first video sample.
///
/// The server maps a KLV time to the video clock through the reports' NTP
/// times in 32.32 fixed point and rounds down to a whole tick, so a unit
/// may land one tick early; the push's own NTP times are within one NTP
/// unit of exact, far too little to move it more.
fn push_and_check_klv_alignment(aus: usize, klv_first: bool) {
    let s = Served::ending_on_exit();
    let app = s.app();
    let mut push = s.push(&elementary(true));
    let units: Vec<Vec<u8>> = (0..aus as u32).map(klv_unit).collect();
    for (i, unit) in units.iter().enumerate() {
        let at = 9_000 + i as i64 * AU_STEP;
        if klv_first {
            push.send_klv(unit, pts(at + KLV_OFFSET)).unwrap();
            push.send_video(&access_unit(i), pts(at)).unwrap();
        } else {
            push.send_video(&access_unit(i), pts(at)).unwrap();
            push.send_klv(unit, pts(at + KLV_OFFSET)).unwrap();
        }
    }
    s.stats_when("both tracks' reports", |st| {
        st.alignment == ClockAlignment::SenderReport
    });
    push.close().unwrap();
    let events = demux_to_end(app);
    let stats = s.mount.stats();
    assert_eq!(stats.alignment, ClockAlignment::SenderReport);
    assert_eq!(stats.alignment_steps, 0);
    assert_eq!(stats.klv_units_emitted, aus as u64);
    assert_eq!(stats.klv_units_dropped, 0);
    assert_eq!(stats.malformed_packets, 0);

    let video: Vec<i64> = video_samples(&events).iter().map(|v| v.0).collect();
    let klv = klv_events(&events);
    assert_eq!(video.len(), aus, "video samples demuxed");
    assert_eq!(klv.len(), aus, "KLV units demuxed");
    for (i, ((pts, payload), unit)) in klv.iter().zip(&units).enumerate() {
        let sent = i as i64 * AU_STEP + KLV_OFFSET;
        let early = sent - (pts - video[0]);
        assert!(
            (0..=1).contains(&early),
            "KLV unit {i} at {} past the first video sample, sent at {sent}",
            pts - video[0]
        );
        assert_eq!(payload, unit, "bytes of KLV unit {i}");
    }
}

#[test]
fn h264_and_klv_reach_sender_report_alignment_and_every_klv_unit_keeps_its_pts() {
    push_and_check_klv_alignment(30, false);
}

#[test]
fn a_push_whose_first_call_is_send_klv_still_aligns() {
    push_and_check_klv_alignment(30, true);
}

/// A KLV unit as large as one RTP packet over the connection takes:
/// sent as one packet, it comes out as one unit.
#[test]
fn a_klv_unit_of_60000_bytes_arrives_whole() {
    const LEN: usize = 60_000;
    // A universal set key, a four-byte BER length, and the value.
    let value_len = LEN - 16 - 4;
    let mut unit = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x83,
    ];
    unit.extend_from_slice(&(value_len as u32).to_be_bytes()[1..]);
    unit.extend((0..value_len).map(|k| (k % 251) as u8));
    assert_eq!(unit.len(), LEN);

    let s = Served::ending_on_exit();
    let app = s.app();
    let mut push = s.push(&elementary(true));
    push.send_video(&access_unit(0), pts(9_000)).unwrap();
    push.send_klv(&unit, pts(9_000)).unwrap();
    for i in 1..5 {
        push.send_video(&access_unit(i), pts(9_000 + i as i64 * AU_STEP))
            .unwrap();
    }
    push.close().unwrap();
    let klv = klv_events(&demux_to_end(app));
    assert_eq!(klv.len(), 1, "KLV units demuxed");
    assert_same_bytes(&klv[0].1, &unit, "the KLV unit");
    assert_eq!(s.mount.stats().klv_units_dropped, 0);
}
