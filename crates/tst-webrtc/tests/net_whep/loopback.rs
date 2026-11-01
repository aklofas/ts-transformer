//! A server on loopback played by str0m viewers: media and KLV delivery,
//! one shared media port, keyframe gating, the reaper, and every answer
//! the WHEP endpoint gives.

use std::collections::HashSet;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::url::WhepUrl;
use tst_webrtc::{
    KlvCarriage, ManualClock, MountConfig, MountKind, PushError, VideoCodec, WhepServer,
    WhepServerError,
};

use crate::fixtures::ts::{FIRST_PTS, TICKS_PER_FRAME, build_ts, record};
use crate::fixtures::{
    TestViewer, connect_all, eventually, h264_gop, http, join, klv_record, pump_all, read_response,
    whep_post,
};

const CONNECT: Duration = Duration::from_secs(5);
const DELIVER: Duration = Duration::from_secs(10);

fn pts(i: usize) -> Pts90khz {
    Pts90khz::new(i as i64 * 3000)
}

/// `true` when the access unit (4-byte start codes, as str0m and the
/// fixture frame it) holds an IDR slice.
fn has_idr(au: &[u8]) -> bool {
    au.windows(5)
        .any(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 5)
}

#[test]
fn one_viewer_gets_sei_and_data_channel_klv_frame_locked() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    let (status, headers, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    assert_eq!(headers["content-type"], "application/sdp");
    assert!(headers["location"].starts_with("/cam/sessions/"));
    v.accept_answer(&answer);
    // The viewer created no `klv` channel of its own: `dc_open` is the
    // server's DCEP open, received by label.
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );

    let aus = h264_gop();
    assert_eq!(aus.len(), 30);
    for (i, (au, key)) in aus.iter().enumerate() {
        m.push_klv(&klv_record(i as u8), pts(i), 0).unwrap();
        m.push_video(au, pts(i), *key).unwrap();
    }
    let done = v.pump(|v| v.frames.len() >= 30 && v.dc_msgs.len() >= 30, DELIVER);
    assert!(done, "{}", v.trace());
    assert_eq!(v.frames.len(), 30);
    assert_eq!(v.dc_msgs.len(), 30);
    for (i, (rtp_ts, au)) in v.frames.iter().enumerate() {
        let seis = tst_webrtc::sei::extract_klv_seis(au, VideoCodec::H264);
        assert_eq!(seis.len(), 1, "frame {i}");
        assert_eq!(seis[0].klv, klv_record(i as u8));
        assert_eq!(seis[0].klv_pts_90k, i as u64 * 3000);
        let dc = &v.dc_msgs[i];
        assert_eq!(dc[0], 1, "message {i}: format version");
        assert_eq!(u32::from_be_bytes(dc[1..5].try_into().unwrap()), *rtp_ts);
        assert_eq!(
            u64::from_be_bytes(dc[5..13].try_into().unwrap()),
            i as u64 * 3000
        );
        assert_eq!(&dc[13..], &klv_record(i as u8)[..]);
    }
    assert!(has_idr(&v.frames[0].1));
    let s = srv.stats();
    assert_eq!((s.mounts, s.active_sessions, s.sessions_total), (1, 1, 1));
    assert!(s.rtp_packets_sent >= 30);
    let ms = srv.mount_stats("/cam").unwrap();
    assert_eq!(ms.klv_sei_inserted, 30);
    assert_eq!(ms.klv_dc_messages_sent, 30);
    assert_eq!(ms.klv_dc_dropped_viewer, 0);
    assert_eq!(ms.viewers, 1);
}

#[test]
fn ten_viewers_on_one_socket_share_payload_bytes_with_distinct_ssrcs() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut viewers: Vec<TestViewer> = (0..10).map(|_| TestViewer::new()).collect();
    for v in &mut viewers {
        join(srv.local_addr(), "/cam", v, false);
    }
    connect_all(&m, &mut viewers);

    let aus = h264_gop();
    for (i, (au, key)) in aus.iter().take(10).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    let ok = pump_all(
        &mut viewers,
        |vs| vs.iter().all(|v| v.frames.len() >= 10),
        DELIVER,
    );
    let traces: Vec<String> = viewers.iter().map(TestViewer::trace).collect();
    assert!(ok, "{traces:?}");

    let first = viewers[0].frames.clone();
    let mut ssrcs = HashSet::new();
    for v in &mut viewers {
        assert_eq!(v.frames.len(), 10);
        // Each viewer gets the same access units and RTP timestamps.
        assert_eq!(v.frames, first);
        // Every datagram came from the one media socket.
        assert_eq!(v.sources, HashSet::from([srv.media_addr()]));
        ssrcs.insert(v.video_ssrc().expect("a receive stream"));
    }
    assert_eq!(ssrcs.len(), 10, "one SSRC per viewer: {ssrcs:?}");
    assert_eq!(srv.stats().active_sessions, 10);
}

#[test]
fn mid_gop_joiner_waits_for_the_next_keyframe() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let aus = h264_gop();
    let mut a = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut a, false);
    connect_all(&m, std::slice::from_mut(&mut a));

    // IDR + 5 P frames reach A alone.
    for (i, (au, key)) in aus.iter().take(6).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(a.pump(|a| a.frames.len() >= 6, DELIVER), "{}", a.trace());

    let mut b = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut b, false);
    let mut both = [a, b];
    connect_all(&m, &mut both);
    // 4 more P frames, then an IDR and 2 P frames.
    let rest = aus[6..10].iter().chain([&aus[0], &aus[1], &aus[2]]);
    for (k, (au, key)) in rest.enumerate() {
        m.push_video(au, pts(6 + k), *key).unwrap();
    }
    let ok = pump_all(
        &mut both,
        |vs| vs[0].frames.len() >= 13 && vs[1].frames.len() >= 3,
        DELIVER,
    );
    let [a, b] = &both;
    assert!(ok, "{} / {}", a.trace(), b.trace());
    assert_eq!(a.frames.len(), 13);
    assert_eq!(b.frames.len(), 3);
    assert!(has_idr(&b.frames[0].1), "B starts at the second IDR");
    assert_eq!(b.frames[0], a.frames[10]);
    assert_eq!(m.stats().viewers_waiting_keyframe, 0);
}

#[test]
fn klv_goes_to_the_server_opened_channel_whatever_the_viewer_calls_its_own() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    // The viewer's own channel in the offer is called `klv` too.
    let offer = v.offer_with_channel_label("klv");
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );
    // And a second one, opened after the server's: the server must not
    // take it for its own.
    v.open_channel("klv");
    let srv_sessions = || m.sessions().into_iter().next().expect("one session");
    assert!(
        eventually(|| srv_sessions().remote_addr.is_some(), CONNECT),
        "{}",
        v.trace()
    );
    // Let the second open reach the server before the first frame.
    v.pump(|_| false, Duration::from_millis(200));

    for (i, (au, key)) in h264_gop().iter().take(5).enumerate() {
        m.push_klv(&klv_record(i as u8), pts(i), 0).unwrap();
        m.push_video(au, pts(i), *key).unwrap();
    }
    let done = v.pump(|v| v.frames.len() >= 5 && v.dc_msgs.len() >= 5, DELIVER);
    assert!(done, "{}", v.trace());
    assert_eq!(v.dc_msgs.len(), 5);
    assert_eq!(v.own_channel_msgs, 0, "{}", v.trace());
    assert_eq!(m.stats().klv_dc_dropped_viewer, 0);
    assert_eq!(srv_sessions().dc_messages, 5);
}

#[test]
fn viewer_without_data_channel_still_gets_sei() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    // No data-channel section in the offer: the server has no SCTP
    // association to open its channel on.
    let offer = v.offer(false);
    assert!(!offer.contains("m=application"), "{offer}");
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    assert!(!answer.contains("m=application"), "{answer}");
    v.accept_answer(&answer);
    connect_all(&m, std::slice::from_mut(&mut v));

    for (i, (au, key)) in h264_gop().iter().take(5).enumerate() {
        m.push_klv(&klv_record(i as u8), pts(i), 0).unwrap();
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(v.pump(|v| v.frames.len() >= 5, DELIVER), "{}", v.trace());
    for (i, (_, au)) in v.frames.iter().enumerate() {
        let seis = tst_webrtc::sei::extract_klv_seis(au, VideoCodec::H264);
        assert_eq!(seis.len(), 1, "frame {i}");
        assert_eq!(seis[0].klv, klv_record(i as u8));
    }
    assert!(v.dc_msgs.is_empty());
    assert!(!v.dc_open);
    assert_eq!(v.error, None);
    assert_eq!(srv.stats().active_sessions, 1);
    // A viewer that never opened the channel is not a refused write.
    assert_eq!(m.stats().klv_dc_dropped_viewer, 0);
    let sessions = m.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].dc_messages_dropped, 0);
}

#[test]
fn stalled_viewer_is_dropped_and_others_continue() {
    let clock = Arc::new(ManualClock::new());
    let url = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    let srv = WhepServer::bind_with_clock(url, clock.clone()).unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut viewers = [TestViewer::new(), TestViewer::new()];
    for v in &mut viewers {
        join(srv.local_addr(), "/cam", v, false);
    }
    connect_all(&m, &mut viewers);
    let [a, _b] = &mut viewers;

    // B stops: no consent checks reach the server any more. A keeps
    // answering. Each 10 s step is followed by two consent responses to A,
    // so at least one of A's checks reached the server after the clock
    // moved and A's consent stays fresh (an ice-lite server keeps a pair
    // 15 s after its last check). B's pair lapses, B is disconnected, and
    // 30 s later the reaper closes it.
    let mut steps = 0;
    while srv.stats().sessions_reaped_disconnected == 0 {
        steps += 1;
        assert!(steps <= 8, "B was never reaped: {:?}", m.sessions());
        clock.advance(Duration::from_secs(10));
        let seen = a.stun_responses;
        assert!(
            a.pump(|a| a.stun_responses >= seen + 2, CONNECT),
            "A's consent checks went unanswered: {}",
            a.trace()
        );
    }
    let s = srv.stats();
    assert_eq!(s.sessions_reaped_disconnected, 1);
    assert_eq!(s.sessions_reaped_ice_timeout, 0);
    assert_eq!(s.active_sessions, 1);

    for (i, (au, key)) in h264_gop().iter().take(3).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(a.pump(|a| a.frames.len() >= 3, DELIVER), "{}", a.trace());
}

#[test]
fn a_full_data_channel_drops_klv_and_keeps_the_video() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    // Data channel only: the frames stay small, so a viewer that stops
    // reading does not overflow its own socket buffer with video.
    let mut cfg = MountConfig::default();
    cfg.klv = KlvCarriage::DataChannelOnly;
    let m = srv.add_mount("/cam", cfg).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut v, true);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );

    // The viewer stops reading, so nothing acknowledges the data channel.
    // Five 100 000-byte records fill the 512 KiB send buffer; the rest are
    // refused and dropped, while the frames carrying them still go out.
    let record = vec![0x5a; 100_000];
    let aus = h264_gop();
    for (i, (au, key)) in aus.iter().take(8).enumerate() {
        m.push_klv(&record, pts(i), 0).unwrap();
        m.push_video(au, pts(i), *key).unwrap();
    }
    let dropped = eventually(
        || {
            let ms = m.stats();
            ms.klv_dc_messages_sent + ms.klv_dc_dropped_viewer == 8
        },
        DELIVER,
    );
    let ms = m.stats();
    assert!(dropped, "{ms:?}");
    assert!(ms.klv_dc_dropped_viewer >= 3, "{ms:?}");
    let session = m.sessions().pop().expect("the session");
    assert_eq!(session.dc_messages_dropped, ms.klv_dc_dropped_viewer);

    // Reading again, the viewer gets every frame and every message the
    // channel took.
    let sent = ms.klv_dc_messages_sent as usize;
    let caught_up = v.pump(|v| v.frames.len() >= 8 && v.dc_msgs.len() >= sent, DELIVER);
    assert!(caught_up, "{}", v.trace());
    assert_eq!(v.frames.len(), 8);
    assert_eq!(v.dc_msgs.len(), sent);
}

#[test]
fn codec_mismatch_is_406_and_leaks_nothing() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::H265;
    srv.add_mount("/cam", cfg).unwrap();
    let mut v = TestViewer::h264_only();
    let offer = v.offer(true);
    let (status, _, _) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 406);
    let s = srv.stats();
    assert_eq!((s.sessions_total, s.active_sessions), (0, 0));
    assert_eq!(s.sessions_rejected_codec, 1);
}

#[test]
fn viewer_cap_is_503_with_retry_after() {
    let srv = WhepServer::bind("whep://127.0.0.1:0?max_viewers=1").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let (first, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(first, 201);
    let (second, headers, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(second, 503);
    assert_eq!(headers["retry-after"], "5");
    let s = srv.stats();
    assert_eq!((s.active_sessions, s.sessions_rejected_cap), (1, 1));
}

#[test]
fn bearer_token_is_enforced() {
    let srv = WhepServer::bind("whep://127.0.0.1:0?token=abc").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let offer = TestViewer::new().offer(true);
    let (none, headers, _) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(none, 401);
    assert_eq!(headers["www-authenticate"], "Bearer");
    let (wrong, _, _) = whep_post(srv.local_addr(), "/cam", &offer, Some("abd"));
    assert_eq!(wrong, 401);
    let (right, headers, _) = whep_post(srv.local_addr(), "/cam", &offer, Some("abc"));
    assert_eq!(right, 201);
    // Ending the session needs the token too.
    let location = &headers["location"];
    let (delete, _, _) = http(srv.local_addr(), "DELETE", location, &[], b"");
    assert_eq!(delete, 401);
    let auth = [("Authorization", "Bearer abc")];
    let (delete, _, _) = http(srv.local_addr(), "DELETE", location, &auth, b"");
    assert_eq!(delete, 200);
    assert_eq!(srv.stats().sessions_rejected_auth, 3);
}

#[test]
fn delete_ends_the_session() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let location = join(srv.local_addr(), "/cam", &mut v, true);
    assert_eq!(srv.stats().active_sessions, 1);
    let (status, _, _) = http(srv.local_addr(), "DELETE", &location, &[], b"");
    assert_eq!(status, 200);
    // The delete is answered by the media thread after the session is
    // gone, so the next read sees it.
    assert_eq!(srv.stats().active_sessions, 0);
    assert_eq!(srv.mount_stats("/cam").unwrap().viewers, 0);
    let (again, _, _) = http(srv.local_addr(), "DELETE", &location, &[], b"");
    assert_eq!(again, 404);
    let id = location.rsplit('/').next().unwrap();
    let (other, _, _) = http(
        srv.local_addr(),
        "DELETE",
        &format!("/nope/sessions/{id}"),
        &[],
        b"",
    );
    assert_eq!(other, 404);
}

/// A trickle-ICE fragment as a WHEP player sends it.
const SDP_FRAG: &[u8] = b"a=ice-ufrag:0000\r\na=ice-pwd:00000000000000000000000000\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0\r\n\
a=candidate:1 1 UDP 2122252543 192.0.2.1 50000 typ host\r\n";

/// An ICE-restart fragment: new credentials, sent with `If-Match: *`.
const ICE_RESTART_FRAG: &[u8] = b"a=ice-ufrag:1111\r\na=ice-pwd:11111111111111111111111111\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0\r\n";

/// Neither trickle ICE nor ICE restart is supported, so a session resource
/// refuses PATCH whatever it carries, and the refusal leaves the session
/// as it was.
#[test]
fn patch_on_a_live_session_is_405_and_the_session_survives() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let location = join(srv.local_addr(), "/cam", &mut v, true);
    for (headers, body) in [
        (
            &[("Content-Type", "application/trickle-ice-sdpfrag")][..],
            SDP_FRAG,
        ),
        (
            &[("Content-Type", "text/plain")][..],
            &b"\x00not a fragment"[..],
        ),
        (
            &[
                ("Content-Type", "application/trickle-ice-sdpfrag"),
                ("If-Match", "*"),
            ][..],
            ICE_RESTART_FRAG,
        ),
    ] {
        let (status, resp_headers, _) = http(srv.local_addr(), "PATCH", &location, headers, body);
        assert_eq!(status, 405);
        assert_eq!(resp_headers["allow"], "DELETE, OPTIONS");
        assert_eq!(srv.stats().active_sessions, 1);
    }
    connect_all(&m, std::slice::from_mut(&mut v));
    for (i, (au, key)) in h264_gop().iter().take(3).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(v.pump(|v| v.frames.len() >= 3, DELIVER), "{}", v.trace());
}

/// The method check comes before the bearer check: a PATCH is refused for
/// what it is, and does not count as a failed authentication.
#[test]
fn a_method_the_target_does_not_take_is_refused_before_auth() {
    let srv = WhepServer::bind("whep://127.0.0.1:0?token=abc").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let offer = TestViewer::new().offer(true);
    let (status, headers, _) = whep_post(srv.local_addr(), "/cam", &offer, Some("abc"));
    assert_eq!(status, 201);
    let location = &headers["location"];
    for (method, target, body, allow) in [
        ("PATCH", location.as_str(), SDP_FRAG, "DELETE, OPTIONS"),
        (
            "POST",
            location.as_str(),
            offer.as_bytes(),
            "DELETE, OPTIONS",
        ),
        ("DELETE", "/cam", &b""[..], "POST, OPTIONS"),
    ] {
        let (status, headers, _) = http(srv.local_addr(), method, target, &[], body);
        assert_eq!(status, 405, "{method} {target}");
        assert_eq!(headers["allow"], allow, "{method} {target}");
    }
    assert_eq!(srv.stats().sessions_rejected_auth, 0);
    assert_eq!(srv.stats().active_sessions, 1);
}

#[test]
fn delete_on_another_mount_is_404() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    srv.add_mount("/other", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let location = join(srv.local_addr(), "/cam", &mut v, true);
    let id = location.rsplit('/').next().unwrap();
    // The other mount exists, but the session is not one of its own.
    let (status, _, _) = http(
        srv.local_addr(),
        "DELETE",
        &format!("/other/sessions/{id}"),
        &[],
        b"",
    );
    assert_eq!(status, 404);
    assert_eq!(srv.stats().active_sessions, 1);
    assert_eq!(srv.mount_stats("/cam").unwrap().viewers, 1);
    let (own, _, _) = http(srv.local_addr(), "DELETE", &location, &[], b"");
    assert_eq!(own, 200);
    assert_eq!(srv.stats().active_sessions, 0);
}

#[test]
fn preflight_patch_and_unknown_mount() {
    let srv = WhepServer::bind("whep://127.0.0.1:0?cors_origin=https://ops.example").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let addr = srv.local_addr();

    let (status, headers, _) = http(addr, "OPTIONS", "/cam", &[], b"");
    assert_eq!(status, 204);
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://ops.example"
    );
    assert_eq!(
        headers["access-control-allow-methods"],
        "POST, DELETE, OPTIONS"
    );
    // mediamtx-style players send the trickle PATCH with `If-Match`, so a
    // cross-origin preflight must allow it.
    assert_eq!(
        headers["access-control-allow-headers"],
        "Authorization, Content-Type, If-Match"
    );
    assert_eq!(headers["access-control-expose-headers"], "Location");

    // A 405 on the endpoint lists the endpoint's own methods.
    for method in ["PATCH", "GET"] {
        let (status, headers, _) = http(addr, method, "/cam", &[], b"");
        assert_eq!(status, 405, "{method}");
        assert_eq!(headers["allow"], "POST, OPTIONS");
    }
    let offer = TestViewer::new().offer(true);
    let (status, _, _) = whep_post(addr, "/nope", &offer, None);
    assert_eq!(status, 404);
    let (status, _, _) = whep_post(addr, "/", &offer, None);
    assert_eq!(status, 404);
    let (status, _, _) = whep_post(addr, "/cam", "v=0 this is no offer", None);
    assert_eq!(status, 400);
    assert_eq!(srv.stats().sessions_total, 0);
}

/// A `recvonly` offer whose `rtx` payload 125 names both VP8 and VP9 as
/// its `apt`: two formats would claim one payload number.
const SHARED_RTX_OFFER: &str = "v=0\r\n\
o=- 1 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=fingerprint:sha-256 AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 120 121 125\r\n\
c=IN IP4 0.0.0.0\r\n\
a=recvonly\r\n\
a=mid:0\r\n\
a=ice-ufrag:00000000\r\n\
a=ice-pwd:00000000000000000000000000000000\r\n\
a=setup:actpass\r\n\
a=rtcp-mux\r\n\
a=rtpmap:120 VP8/90000\r\n\
a=rtpmap:121 VP9/90000\r\n\
a=rtpmap:125 rtx/90000\r\n\
a=fmtp:125 apt=120\r\n\
a=fmtp:125 apt=121\r\n";

#[test]
fn an_offer_mapping_one_payload_type_to_two_formats_is_400_and_the_server_keeps_serving() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let (status, _, _) = whep_post(srv.local_addr(), "/cam", SHARED_RTX_OFFER, None);
    assert_eq!(status, 400);
    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(status, 201);
    assert_eq!(srv.stats().sessions_total, 1);
}

/// Chrome's offer with a one-entry BUNDLE group and no `a=mid` on its
/// second section.
fn mid_less_section_offer() -> String {
    let chrome = include_str!("../fixtures/offers/chrome-155.sdp");
    let cut = chrome.replace("a=group:BUNDLE 0 1", "a=group:BUNDLE 0");
    assert!(cut.contains("a=mid:1"));
    cut.lines()
        .filter(|l| l.trim_end() != "a=mid:1")
        .map(|l| format!("{l}\r\n"))
        .collect()
}

/// str0m 0.24 rejects a media section without `a=mid` while parsing the
/// offer, so it never reaches the code that would panic on it. This pins
/// that: if a str0m bump stops rejecting it at parse, this test fails,
/// and the panic containment around `accept_offer` in the media thread is
/// what keeps every other viewer alive.
#[test]
fn an_offer_with_a_mid_less_media_line_is_400_and_the_server_keeps_serving() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let (status, _, _) = whep_post(srv.local_addr(), "/cam", &mid_less_section_offer(), None);
    assert_eq!(status, 400);
    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(status, 201);
    assert_eq!(srv.stats().sessions_total, 1);
}

/// A panic while a session is being initialised, after str0m accepted the
/// offer, refuses that offer with 500 and leaves the media thread, its
/// connected viewers and the session count alone.
#[cfg(feature = "test-hooks")]
#[test]
fn a_panic_while_initialising_a_session_is_500_and_viewers_keep_playing() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut a = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut a, false);
    connect_all(&m, std::slice::from_mut(&mut a));
    let sessions = srv.stats().sessions_total;

    let offer = TestViewer::new().offer(true);
    let poisoned = offer.replacen("a=ice-ufrag:", "a=ice-ufrag:tsthookpanic", 1);
    assert_ne!(poisoned, offer);
    let (status, _, _) = whep_post(srv.local_addr(), "/cam", &poisoned, None);
    assert_eq!(status, 500);
    assert_eq!(srv.stats().sessions_total, sessions);

    for (i, (au, key)) in h264_gop().iter().take(5).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(a.pump(|v| v.frames.len() >= 5, DELIVER), "{}", a.trace());
    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(status, 201);
    assert_eq!(srv.stats().sessions_total, sessions + 1);
}

/// A `recvonly` offer whose `m=` line lists only 120 and that maps 121,
/// a number outside it, twice: str0m claims every `a=rtpmap`, so the
/// second claim of 121 would panic it.
const UNLISTED_PT_MAPPED_TWICE_OFFER: &str = "v=0\r\n\
o=- 1 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=fingerprint:sha-256 AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 120\r\n\
c=IN IP4 0.0.0.0\r\n\
a=recvonly\r\n\
a=mid:0\r\n\
a=ice-ufrag:00000000\r\n\
a=ice-pwd:00000000000000000000000000000000\r\n\
a=setup:actpass\r\n\
a=rtcp-mux\r\n\
a=rtpmap:120 H264/90000\r\n\
a=fmtp:120 packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:121 VP8/90000\r\n\
a=rtpmap:121 VP9/90000\r\n";

#[test]
fn an_offer_mapping_an_unlisted_payload_type_twice_is_400_and_viewers_keep_playing() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut a = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut a, false);
    connect_all(&m, std::slice::from_mut(&mut a));

    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        UNLISTED_PT_MAPPED_TWICE_OFFER,
        None,
    );
    assert_eq!(status, 400);

    // The media thread is alive: the connected viewer still gets frames,
    // and the next valid offer is answered.
    for (i, (au, key)) in h264_gop().iter().take(5).enumerate() {
        m.push_video(au, pts(i), *key).unwrap();
    }
    assert!(a.pump(|v| v.frames.len() >= 5, DELIVER), "{}", a.trace());
    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/cam",
        &TestViewer::new().offer(true),
        None,
    );
    assert_eq!(status, 201);
    assert_eq!(srv.stats().sessions_total, 2);
}

#[test]
fn malformed_and_oversized_requests_are_refused() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let addr = srv.local_addr();
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"POST /cam HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
        .unwrap();
    assert_eq!(read_response(&mut s).0, 400);

    let mut s = TcpStream::connect(addr).unwrap();
    let long = format!("POST /cam HTTP/1.1\r\nX-Pad: {}\r\n", "a".repeat(17 * 1024));
    // The server answers once the head passes its limit; it may close
    // before the client has written everything.
    let _ = s.write_all(long.as_bytes());
    assert_eq!(read_response(&mut s).0, 431);
}

#[test]
fn a_request_that_never_completes_is_408() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let mut s = TcpStream::connect(srv.local_addr()).unwrap();
    s.write_all(b"POST /cam HTTP/1.1\r\nContent-Length: 10\r\n")
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(read_response(&mut s).0, 408);
}

#[test]
fn mount_paths_are_checked_and_removal_closes_the_mount() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    for bad in [
        "",
        "cam",
        "/a?b",
        "/a#b",
        "/",
        "/a//b",
        "/a/",
        "/a b",
        "/caf\u{e9}",
        // The URL of session `source` on mount `/cam`.
        "/cam/sessions/source",
    ] {
        assert!(
            matches!(
                srv.add_mount(bad, MountConfig::default()),
                Err(WhepServerError::InvalidMountPath { .. })
            ),
            "{bad:?}"
        );
    }
    for control in ["/a\u{7f}", "/a\nb", "/a\0"] {
        match srv.add_mount(control, MountConfig::default()) {
            Err(WhepServerError::InvalidMountPath { detail }) => {
                assert_eq!(detail, "path contains a control byte");
            }
            other => panic!("{control:?}: {other:?}"),
        }
    }
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::Av1;
    assert!(matches!(
        srv.add_mount("/av1", cfg),
        Err(WhepServerError::InvalidConfig { .. })
    ));

    let m = srv
        .add_mount("/site/cam-1", MountConfig::default())
        .unwrap();
    assert_eq!(m.mount_path(), "/site/cam-1");
    assert!(matches!(
        srv.add_mount("/site/cam-1", MountConfig::default()),
        Err(WhepServerError::MountExists { .. })
    ));
    assert_eq!(srv.stats().mounts, 1);
    srv.remove_mount("/site/cam-1").unwrap();
    assert!(matches!(
        m.push_video(&h264_gop()[0].0, pts(0), true),
        Err(PushError::Closed)
    ));
    assert!(srv.mount_stats("/site/cam-1").is_none());
    assert!(matches!(
        srv.remove_mount("/site/cam-1"),
        Err(WhepServerError::MountNotFound { .. })
    ));
    let (status, _, _) = whep_post(
        srv.local_addr(),
        "/site/cam-1",
        &TestViewer::new().offer(false),
        None,
    );
    assert_eq!(status, 404);
    // The path is free again.
    srv.add_mount("/site/cam-1", MountConfig::default())
        .unwrap();
}

#[test]
fn removing_a_mount_closes_its_sessions() {
    let clock = Arc::new(ManualClock::new());
    let url = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    let srv = WhepServer::bind_with_clock(url, clock).unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut v, false);
    connect_all(&m, std::slice::from_mut(&mut v));
    assert!(srv.wake_for_test());
    assert_eq!(m.stats().viewers, 1);
    srv.remove_mount("/cam").unwrap();
    assert!(srv.wake_for_test());
    assert!(
        eventually(|| srv.stats().active_sessions == 0, DELIVER),
        "{:?}",
        srv.stats()
    );
    assert!(m.sessions().is_empty());
    // The retained handle reads no viewers of the removed mount.
    let stats = m.stats();
    assert_eq!((stats.viewers, stats.viewers_waiting_keyframe), (0, 0));
}

#[test]
fn a_hand_edited_url_that_fails_validation_binds_no_port() {
    let mut too_many = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    too_many.max_viewers = 65;
    let mut plain_with_key = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    plain_with_key.key = Some("k.pem".into());
    for url in [too_many, plain_with_key] {
        // Hold both ports for the whole call: a bind that got past
        // validation would fail with an I/O error, not `InvalidUrl`.
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        let _udp = std::net::UdpSocket::bind(("127.0.0.1", port));
        let mut url = url;
        url.port = port;
        url.media_port = port;
        match WhepServer::bind_with(url) {
            Err(WhepServerError::InvalidUrl(_)) => {}
            other => panic!("{:?}", other.err()),
        }
    }
}

#[cfg(not(feature = "tls"))]
#[test]
fn wheps_bind_without_the_tls_feature_is_refused() {
    match WhepServer::bind("wheps://127.0.0.1:0?cert=c.pem&key=k.pem") {
        Err(WhepServerError::Tls(detail)) => {
            assert!(detail.contains("built without the tls feature"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn records_above_64_kib_arrive_whole_on_the_data_channel() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let mut cfg = MountConfig::default();
    cfg.klv = KlvCarriage::DataChannelOnly;
    let m = srv.add_mount("/cam", cfg).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut v, true);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );

    // str0m's viewer advertises a 256 KiB max-message-size, as Chrome and
    // Safari do: both records, with their 13-byte header, are within it.
    let records: Vec<Vec<u8>> = [100 * 1024, 250 * 1024]
        .iter()
        .enumerate()
        .map(|(i, &len)| (0..len).map(|b| (b * 7 + i) as u8).collect())
        .collect();
    for (i, (au, key)) in h264_gop().iter().take(2).enumerate() {
        m.push_klv(&records[i], pts(i), 0).unwrap();
        m.push_video(au, pts(i), *key).unwrap();
    }
    let done = v.pump(|v| v.frames.len() >= 2 && v.dc_msgs.len() >= 2, DELIVER);
    assert!(done, "{} {:?}", v.trace(), m.stats());
    for (i, record) in records.iter().enumerate() {
        let dc = &v.dc_msgs[i];
        assert_eq!(dc.len(), 13 + record.len(), "message {i}");
        assert_eq!(
            u32::from_be_bytes(dc[1..5].try_into().unwrap()),
            v.frames[i].0
        );
        assert_eq!(&dc[13..], &record[..], "message {i}");
    }
    let ms = m.stats();
    assert_eq!((ms.klv_dc_messages_sent, ms.klv_dc_dropped_viewer), (2, 0));
}

#[test]
fn a_record_whose_message_exceeds_the_viewers_max_message_size_is_dropped() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let mut cfg = MountConfig::default();
    cfg.klv = KlvCarriage::DataChannelOnly;
    let m = srv.add_mount("/cam", cfg).unwrap();
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    // str0m's viewer advertises a fixed max-message-size of 262144, as
    // Chrome and Safari do.
    assert!(offer.contains("a=max-message-size:262144"), "{offer}");
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );

    // The larger record goes first, while the send buffer is empty, so the
    // message size alone refuses it: 262 132 + 13 = 262 145 bytes.
    let too_big = vec![0xa5u8; 262_132];
    let largest = vec![0x5au8; 262_131];
    let aus = h264_gop();
    m.push_klv(&too_big, pts(0), 0).unwrap();
    m.push_video(&aus[0].0, pts(0), aus[0].1).unwrap();
    m.push_klv(&largest, pts(1), 0).unwrap();
    m.push_video(&aus[1].0, pts(1), aus[1].1).unwrap();

    let done = v.pump(|v| v.frames.len() >= 2 && !v.dc_msgs.is_empty(), DELIVER);
    assert!(done, "{} {:?}", v.trace(), m.stats());
    assert_eq!(v.dc_msgs.len(), 1);
    assert_eq!(v.dc_msgs[0].len(), 13 + largest.len());
    assert_eq!(&v.dc_msgs[0][13..], &largest[..]);
    let ms = m.stats();
    assert_eq!((ms.klv_dc_messages_sent, ms.klv_dc_dropped_viewer), (1, 1));
    let session = m.sessions().pop().expect("the session");
    assert_eq!(session.dc_messages_dropped, 1);
}

#[test]
fn an_mp2t_mount_delivers_the_ts_frames_with_their_klv_to_a_viewer() {
    const FRAMES: usize = 10;
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    cfg.klv = KlvCarriage::SeiAndDataChannel;
    let m = srv.add_mount("/ts", cfg).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/ts", &mut v, true);
    connect_all(&m, std::slice::from_mut(&mut v));
    assert!(v.pump(|v| v.dc_open, CONNECT), "{}", v.trace());

    // One access unit more than asserted: the demuxer emits an access unit
    // when the next one starts.
    for slice in build_ts(FRAMES + 1).chunks(1316) {
        m.push_ts(slice).unwrap();
    }
    let done = v.pump(
        |v| v.frames.len() >= FRAMES && v.dc_msgs.len() >= FRAMES,
        DELIVER,
    );
    assert!(done, "{}", v.trace());
    assert_eq!(v.frames.len(), FRAMES);
    assert!(has_idr(&v.frames[0].1));
    for (i, (rtp_ts, au)) in v.frames.iter().enumerate() {
        let pts = (FIRST_PTS + i as i64 * TICKS_PER_FRAME) as u64;
        let seis = tst_webrtc::sei::extract_klv_seis(au, VideoCodec::H264);
        assert_eq!(seis.len(), 1, "frame {i}");
        assert_eq!(seis[0].klv, record(i));
        assert_eq!(seis[0].klv_pts_90k, pts);
        let dc = &v.dc_msgs[i];
        assert_eq!(u32::from_be_bytes(dc[1..5].try_into().unwrap()), *rtp_ts);
        assert_eq!(&dc[13..], &record(i)[..]);
    }
    let ms = m.stats();
    assert_eq!(ms.frames_pushed, FRAMES as u64);
    assert_eq!(ms.klv_sei_inserted, FRAMES as u64);
    assert_eq!(ms.demux_nonconformant, 0);
}
