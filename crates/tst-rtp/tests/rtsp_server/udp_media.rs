//! A server that does not allow media over UDP answers `461 Unsupported
//! Transport` to every SETUP whose transport is not TCP-interleaved, reader
//! or publisher, and only after the request was admitted and its mount
//! found. A plain `rtsp://` server allows UDP media unless told otherwise;
//! the `rtsps://` twins are in `tls.rs`.

use std::net::UdpSocket;

use secrecy::SecretString;
use tst_rtp::{MountCredential, MountPolicy, MountRule, RtspServer, RtspServerBuilder};

use crate::fixtures::raw_rtsp::{make_muxer_cfg, try_header};
use crate::fixtures::raw_rtsp_publisher::{RawPublisher, SDP_MP2T};

fn status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// One reader SETUP of `path` asking for `transport`, on a fresh
/// connection; returns the response head.
pub(crate) fn reader_setup(mut p: RawPublisher, path: &str, transport: &str) -> String {
    let req = format!(
        "SETUP {}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: {transport}\r\n\r\n",
        p.uri(path)
    );
    p.write_raw(req.as_bytes());
    p.read_responses(1)
}

/// A started server with a unicast local mount `/live` and two publish
/// mounts, `/pub` and `/pub2`.
pub(crate) fn started(b: RtspServerBuilder) -> RtspServer {
    let server = b.build().unwrap();
    server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.add_publish_mount("/pub").unwrap();
    server.add_publish_mount("/pub2").unwrap();
    server.start().unwrap();
    server
}

/// What every way of asking for media is answered, each on a fresh
/// connection made by `connect`: (what was asked, the status, whether the
/// reply names a `server_port`).
pub(crate) fn every_spelling(connect: impl Fn() -> RawPublisher) -> Vec<(&'static str, u16, bool)> {
    // A port nobody sends to: only its number goes into the requests.
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let cp = client.local_addr().unwrap().port();
    let cp = format!("{cp}-{}", cp.saturating_add(1));
    let row = |what, r: &str| {
        (
            what,
            status(r),
            try_header(r, "Transport").is_some_and(|t| t.contains("server_port")),
        )
    };
    let mut rows = vec![
        row(
            "reader unicast UDP",
            &reader_setup(
                connect(),
                "/live",
                &format!("RTP/AVP;unicast;client_port={cp}"),
            ),
        ),
        row(
            "reader multicast word with client_port, unicast mount",
            &reader_setup(
                connect(),
                "/live",
                &format!("RTP/AVP;multicast;client_port={cp}"),
            ),
        ),
        row(
            "reader interleaved",
            &reader_setup(connect(), "/live", "RTP/AVP/TCP;unicast;interleaved=0-1"),
        ),
    ];
    let mut udp_publisher = connect();
    assert_eq!(udp_publisher.announce("/pub", SDP_MP2T), 200);
    let (st, ports) =
        udp_publisher.setup_udp("/pub", "streamid=0", client.local_addr().unwrap().port());
    rows.push(("publisher UDP mode=record", st, ports.is_some()));
    // The refused SETUP leaves the first ANNOUNCE holding `/pub`.
    let mut tcp_publisher = connect();
    assert_eq!(tcp_publisher.announce("/pub2", SDP_MP2T), 200);
    let (st, _) = tcp_publisher.setup_interleaved("/pub2", "streamid=0");
    rows.push(("publisher interleaved mode=record", st, false));
    rows
}

/// What a server that does not allow UDP media answers.
pub(crate) const REFUSED: [(&str, u16, bool); 5] = [
    ("reader unicast UDP", 461, false),
    (
        "reader multicast word with client_port, unicast mount",
        461,
        false,
    ),
    ("reader interleaved", 200, false),
    ("publisher UDP mode=record", 461, false),
    ("publisher interleaved mode=record", 200, false),
];

#[test]
fn a_server_without_udp_media_refuses_it_in_every_spelling() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.allow_udp_media(false);
    let server = started(b);
    let port = server.local_addr().unwrap().port();
    assert_eq!(every_spelling(|| RawPublisher::connect(port)), REFUSED);
    server.stop().unwrap();
}

/// A peer that may not see a path learns nothing from the refusal: a
/// request without the credential a rule asks for reads 401, a path with no
/// mount reads 404, before the transport is looked at.
#[test]
fn the_udp_refusal_comes_after_admission_and_after_the_lookup() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.allow_udp_media(false);
    let server = started(b);
    server.add_mount("/locked", make_muxer_cfg()).unwrap();
    server
        .mount_rules()
        .set(
            "/locked",
            MountRule::both(MountPolicy::Require(vec![MountCredential::new(
                "viewer",
                SecretString::new("pw".into()),
            )])),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let udp = "RTP/AVP;unicast;client_port=5000-5001";
    let locked = reader_setup(RawPublisher::connect(port), "/locked", udp);
    let missing = reader_setup(RawPublisher::connect(port), "/nope", udp);
    assert_eq!(
        (status(&locked), status(&missing)),
        (401, 404),
        "{locked}\n{missing}"
    );
    server.stop().unwrap();
}

#[test]
fn a_plain_server_allows_udp_media_by_default() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let port = server.local_addr().unwrap().port();
    let r = reader_setup(
        RawPublisher::connect(port),
        "/live",
        "RTP/AVP;unicast;client_port=5000-5001",
    );
    assert_eq!(status(&r), 200, "{r}");
    assert!(
        try_header(&r, "Transport").is_some_and(|t| t.contains("server_port")),
        "{r}"
    );
    server.stop().unwrap();
}
