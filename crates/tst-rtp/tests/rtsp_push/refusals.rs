//! What the server refuses, as the push sees it: each refusal is
//! `RtspError::Protocol` with the server's status, and leaves the mount
//! without a publisher.

use tst_core::transport::{EndReason, Transport};
use tst_rtp::{
    MountPolicy, MountRule, PushDescription, RefuseStatus, RtspClientBuilder, RtspError, RtspPush,
    VideoTrack,
};

use crate::common::*;
use crate::fixtures::raw_rtsp::make_muxer_cfg;

/// The status of the refusal `connect` meets.
fn refused(builder: RtspClientBuilder, desc: &PushDescription) -> u16 {
    match RtspPush::connect(builder, desc) {
        Err(RtspError::Protocol { code, .. }) => code,
        other => panic!("expected a refusal, got {:?}", other.map(|_| "a push")),
    }
}

#[test]
fn an_h265_push_is_refused_415() {
    let s = Served::new();
    let video = VideoTrack::H265 {
        vps: vec![0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF],
        sps: vec![0x42, 0x01, 0x01, 0x01, 0x60, 0x00],
        pps: vec![0x44, 0x01, 0xC1, 0x72],
    };
    let desc = PushDescription::Elementary { video, klv: false };
    assert_eq!(refused(s.builder(), &desc), 415);
    assert!(s.mount.publisher().is_none());
}

#[test]
fn a_second_publisher_is_refused_403() {
    let s = Served::new();
    let first = s.push(&PushDescription::Mp2t);
    assert_eq!(refused(s.builder(), &PushDescription::Mp2t), 403);
    // The first push still holds the mount.
    assert!(first.is_session_alive());
    assert!(s.mount.publisher().is_some());
    first.close().unwrap();
}

#[test]
fn a_push_to_no_such_mount_is_refused_404() {
    let s = Served::new();
    let elsewhere =
        RtspClientBuilder::new(&format!("rtsp://127.0.0.1:{}/nothing", s.port)).unwrap();
    assert_eq!(refused(elsewhere, &PushDescription::Mp2t), 404);
}

#[test]
fn a_mount_rule_refusing_publish_refuses_the_announce_403() {
    let s = Served::new();
    let refuse = MountPolicy::Refuse(RefuseStatus::Forbidden);
    s.server
        .mount_rules()
        .set(PATH, MountRule::new(refuse, MountPolicy::Inherit))
        .unwrap();
    assert_eq!(refused(s.builder(), &PushDescription::Mp2t), 403);
    assert!(s.mount.publisher().is_none());
}

/// After `stop()` the server holds no connection and takes none: a
/// connection opened before the stop is closed under the ANNOUNCE, and a
/// push gets no RTSP answer, because the listening socket is closed when
/// `stop()` returns. The port is no longer the test's then: the connect is
/// refused, or reaches whatever took the port, which answers no RTSP;
/// either way the push reads an I/O error, never a status.
#[test]
fn a_push_after_stop_finds_no_server() {
    let s = Served::new();
    let mut open_before = s.builder().connect().unwrap();
    open_before.options().unwrap();
    s.server.stop().unwrap();
    let r = open_before.announce(&PushDescription::Mp2t);
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
    let r = RtspPush::connect(s.builder(), &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::Io(_))),
        "{:?}",
        r.map(|_| "a push")
    );
    assert!(s.mount.publisher().is_none());
}

#[test]
fn a_push_to_a_local_muxer_mount_is_refused_461() {
    let s = Served::new();
    let _local = s.server.add_mount("/local", make_muxer_cfg()).unwrap();
    let local = RtspClientBuilder::new(&format!("rtsp://127.0.0.1:{}/local", s.port)).unwrap();
    assert_eq!(refused(local, &PushDescription::Mp2t), 461);
}

/// A rule set during RECORD answers the push's next gated request, which
/// is its TEARDOWN: media keeps flowing until then, and the close reports
/// the refusal.
#[test]
fn a_mount_rule_refusal_during_record_surfaces_at_close_as_a_refused_teardown() {
    let s = Served::new();
    let rules = s.server.mount_rules();
    let refuse = || {
        let forbidden = MountPolicy::Refuse(RefuseStatus::Forbidden);
        rules
            .set(PATH, MountRule::new(forbidden, MountPolicy::Inherit))
            .unwrap();
    };
    let ts = ts_packets(7, 0x11);

    // `close` returns the refusal.
    let mut push = s.push(&PushDescription::Mp2t);
    refuse();
    for sent in 1..=3 {
        push.send_ts(&ts).unwrap();
        s.stats_when("the packet after the rule", |st| {
            st.rtp_packets_received >= sent
        });
    }
    assert!(push.is_session_alive());
    let r = push.close();
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 403, .. })),
        "{r:?}"
    );
    s.until_no_publisher();

    // The transport's close records the refused TEARDOWN as the caller's
    // end.
    assert!(rules.remove(PATH).unwrap());
    let mut push = s.push(&PushDescription::Mp2t);
    refuse();
    push.send_ts(&ts).unwrap();
    Transport::close(&mut push);
    assert_eq!(push.end_reason(), Some(EndReason::Cancelled));
    s.until_no_publisher();
}
