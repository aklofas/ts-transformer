//! The addresses a session takes datagrams from: the address that
//! completed the handshake is pinned, and before that at most 16
//! distinct sources are admitted. The extra sources send STUN binding
//! requests built with the session's own ICE credentials, so they pass
//! str0m's routing and only the source rules stop them.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use str0m::crypto::from_feature_flags;
use str0m::ice::{StunMessage, TransId};
use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::{MountConfig, WhepServer};

use crate::fixtures::{TestViewer, connect_all, eventually, h264_gop, whep_post};

const WAIT: Duration = Duration::from_secs(5);

/// The value of the first `a=<attr>:` line of `sdp`.
fn sdp_attr<'a>(sdp: &'a str, attr: &str) -> &'a str {
    let prefix = format!("a={attr}:");
    sdp.lines()
        .find_map(|l| l.trim_end().strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no a={attr} in the SDP"))
}

/// A binding request with `USE-CANDIDATE`, as a controlling viewer sends
/// it, authenticated with the server's password from `answer`.
fn binding_request(offer: &str, answer: &str) -> Vec<u8> {
    let username = format!(
        "{}:{}",
        sdp_attr(answer, "ice-ufrag"),
        sdp_attr(offer, "ice-ufrag")
    );
    let password = sdp_attr(answer, "ice-pwd");
    let msg = StunMessage::binding_request(&username, TransId::new(), true, 1, 0x6e00_01ff, true);
    let provider = from_feature_flags();
    let mut buf = vec![0; 1500];
    let n = msg
        .to_bytes(Some(password.as_bytes()), &mut buf, |k, p| {
            provider.sha1_hmac_provider.sha1_hmac(k, p)
        })
        .expect("the request encodes");
    buf.truncate(n);
    buf
}

/// A socket on loopback with a short read timeout.
fn extra_socket() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind a socket");
    s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    s
}

/// Whether `s` has a datagram from `from` waiting.
fn got_datagram(s: &UdpSocket, from: SocketAddr) -> bool {
    let mut buf = [0u8; 1500];
    matches!(s.recv_from(&mut buf), Ok((_, src)) if src == from)
}

#[test]
fn a_connected_session_ignores_a_second_source() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let offer = v.offer(false);
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    connect_all(&m, std::slice::from_mut(&mut v));
    let viewer_addr = v.sock.local_addr().unwrap();
    let session = m.sessions().pop().expect("one session");
    assert_eq!(session.remote_addr, Some(viewer_addr));

    let unrouted = srv.stats().datagrams_unrouted;
    let forged = extra_socket();
    forged
        .send_to(&binding_request(&offer, &answer), srv.media_addr())
        .unwrap();
    assert!(
        eventually(|| srv.stats().datagrams_unrouted == unrouted + 1, WAIT),
        "the second source's request was not counted unrouted"
    );

    // Media still reaches the viewer, so the send address did not move.
    for (i, (au, key)) in h264_gop().iter().take(5).enumerate() {
        m.push_video(au, Pts90khz::new(i as i64 * 3000), *key)
            .unwrap();
    }
    assert!(v.pump(|v| v.frames.len() >= 5, WAIT), "{}", v.trace());
    let session = m.sessions().pop().expect("one session");
    assert_eq!(session.remote_addr, Some(viewer_addr));
    assert!(!got_datagram(&forged, srv.media_addr()));
}

#[test]
fn a_seventeenth_source_before_the_handshake_is_dropped() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let _m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let offer = v.offer(false);
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    let request = binding_request(&offer, &answer);
    let media = srv.media_addr();

    // Sixteen sources are admitted: each gets the server's binding response.
    let admitted: Vec<UdpSocket> = (0..16).map(|_| extra_socket()).collect();
    for s in &admitted {
        s.send_to(&request, media).unwrap();
        assert!(
            eventually(|| got_datagram(s, media), WAIT),
            "an admitted source got no response"
        );
    }
    assert_eq!(srv.stats().datagrams_unrouted, 0);

    let seventeenth = extra_socket();
    seventeenth.send_to(&request, media).unwrap();
    assert!(
        eventually(|| srv.stats().datagrams_unrouted == 1, WAIT),
        "the seventeenth source was not counted unrouted"
    );
    assert!(!got_datagram(&seventeenth, media));
    // An admitted source is still served.
    admitted[0].send_to(&request, media).unwrap();
    assert!(eventually(|| got_datagram(&admitted[0], media), WAIT));
    assert_eq!(srv.stats().datagrams_unrouted, 1);
}
