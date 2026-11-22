//! `RtspServerBuilder::max_readers`: a reader slot is taken at SETUP and
//! held until the reader's role ends; a reader over the cap is answered
//! `453 Not Enough Bandwidth` and its connection is closed. Publishers and
//! connections without a role are not readers.
//!
//! Raw clients: every wait is on a reply or on a condition with a deadline.

use std::io::Read as _;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use secrecy::SecretString;
use tst_rtp::{MountPolicy, MountRule, RefuseStatus, RtspServer, RtspServerBuilder};

use crate::fixtures::raw_rtsp::{in_session, make_muxer_cfg, request, scaled, session_id};
use crate::fixtures::raw_rtsp_publisher::{RawPublisher, SDP_MP2T};

fn status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn connect(port: u16) -> TcpStream {
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    tcp
}

/// A reader SETUP of `path` over TCP-interleaved, with `Session: sid` when
/// `sid` is not empty; returns the response head.
fn setup(tcp: &mut TcpStream, port: u16, path: &str, cseq: u32, sid: &str) -> String {
    let session = if sid.is_empty() {
        String::new()
    } else {
        format!("Session: {sid}\r\n")
    };
    request(
        tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}{path}/trackID=0 RTSP/1.0\r\nCSeq: {cseq}\r\n\
             {session}Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
}

/// A reader of `path` on a fresh connection that has set up; returns the
/// connection and its session id.
fn reader(port: u16, path: &str) -> (TcpStream, String) {
    let mut tcp = connect(port);
    let r = setup(&mut tcp, port, path, 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    (tcp, sid)
}

/// After a reply, the server closes the connection: a read returns the end
/// of the stream, or a reset where the platform reports one.
fn assert_closed(tcp: &mut TcpStream) {
    let mut b = [0u8; 64];
    match tcp.read(&mut b) {
        Ok(0) => {}
        Ok(n) => panic!(
            "the server sent more after the refusal: {:?}",
            String::from_utf8_lossy(&b[..n])
        ),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
            "the connection was not closed after the refusal: {e:?}"
        ),
    }
}

/// SETUP `path` on fresh connections until one reads 200 (a slot released
/// by a reader's end may be released a moment after its peer saw it end);
/// returns that connection and its session id.
fn setup_until_admitted(port: u16, path: &str) -> (TcpStream, String) {
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    loop {
        let mut tcp = connect(port);
        let r = setup(&mut tcp, port, path, 1, "");
        match status(&r) {
            200 => return (tcp, session_id(&r)),
            453 => assert!(Instant::now() < deadline, "the slot was never released"),
            other => panic!("unexpected {other}: {r}"),
        }
        std::thread::yield_now();
    }
}

/// A started plain server, its builder configured by `configure`, with
/// two local mounts (`/live`, `/other`) and a publish mount (`/ingest`).
fn start(configure: impl FnOnce(&mut RtspServerBuilder)) -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    configure(&mut b);
    let server = b.build().unwrap();
    server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.add_mount("/other", make_muxer_cfg()).unwrap();
    server.add_publish_mount("/ingest").unwrap();
    server.start().unwrap();
    server
}

/// The one slot of a `max_readers(1)` server is held: another reader's
/// SETUP reads 453.
fn assert_full(port: u16) {
    let mut c = connect(port);
    let r = setup(&mut c, port, "/other", 1, "");
    assert_eq!(status(&r), 453, "the one slot is held: {r}");
}

/// Readers at the cap leave the rest of `max_sessions` to publishers: a
/// third reader is refused and a publisher still records.
#[test]
fn readers_at_the_cap_do_not_shut_a_publisher_out() {
    let server = start(|b| {
        b.max_sessions(4).max_readers(2);
    });
    let port = server.local_addr().unwrap().port();
    let _a = reader(port, "/live");
    let _b = reader(port, "/live");
    let mut c = connect(port);
    let r = setup(&mut c, port, "/live", 1, "");
    assert_eq!(status(&r), 453, "{r}");

    let mut publisher = RawPublisher::connect(port);
    assert_eq!(publisher.announce("/ingest", SDP_MP2T), 200);
    assert_eq!(publisher.setup_interleaved("/ingest", "streamid=0").0, 200);
    assert_eq!(publisher.record("/ingest"), 200);
    server.stop().unwrap();
}

/// A reader refused by the cap is disconnected after the 453, also when it
/// passed a credential check first (which once lifted the connection's
/// admission deadline for good).
#[test]
fn a_reader_refused_by_the_cap_is_disconnected() {
    let server = start(|b| {
        b.max_readers(0);
    });
    let port = server.local_addr().unwrap().port();
    let mut c = connect(port);
    let r = setup(&mut c, port, "/live", 1, "");
    assert_eq!(status(&r), 453, "{r}");
    assert_closed(&mut c);
    server.stop().unwrap();

    let server = start(|b| {
        b.max_readers(0)
            .auth_basic("lab", "admin", SecretString::from("secret"));
    });
    let port = server.local_addr().unwrap().port();
    let mut c = connect(port);
    // "admin:secret" in base64.
    let r = request(
        &mut c,
        &format!(
            "DESCRIBE rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 1\r\n\
             Authorization: Basic YWRtaW46c2VjcmV0\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let r = request(
        &mut c,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\nCSeq: 2\r\n\
             Authorization: Basic YWRtaW46c2VjcmV0\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 453, "{r}");
    assert_closed(&mut c);
    server.stop().unwrap();
}

/// Every end of a reader's role gives its slot back: TEARDOWN (before its
/// reply), a dropped connection, a refusal on its own mount. A reader that
/// is re-pointed at another mount keeps the one slot it holds.
#[test]
fn every_way_a_reader_ends_frees_its_slot() {
    let server = start(|b| {
        b.max_readers(1);
    });
    let port = server.local_addr().unwrap().port();

    // TEARDOWN: released before the reply, so the next SETUP is admitted
    // at once.
    let (mut a, sid) = reader(port, "/live");
    assert_full(port);
    let r = in_session(&mut a, port, "TEARDOWN", "/live", 2, &sid);
    assert_eq!(status(&r), 200, "{r}");
    let b = reader(port, "/live");
    drop(b);

    // A dropped connection.
    let (a, _) = setup_until_admitted(port, "/live");
    assert_full(port);
    drop(a);

    // A refusal on the reader's own mount ends its role.
    let (mut a, sid) = setup_until_admitted(port, "/live");
    assert_full(port);
    server
        .mount_rules()
        .set(
            "/live",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::Forbidden),
            ),
        )
        .unwrap();
    let r = in_session(&mut a, port, "PLAY", "/live", 2, &sid);
    assert_eq!(status(&r), 403, "{r}");
    let (c, _) = setup_until_admitted(port, "/other");
    drop(c);
    drop(a);
    server.mount_rules().remove("/live").unwrap();

    // A re-pointed reader holds exactly one slot, and is not refused at
    // the cap for the slot it already holds.
    let (mut a, sid) = setup_until_admitted(port, "/other");
    let r = setup(&mut a, port, "/live", 2, &sid);
    assert_eq!(status(&r), 200, "re-pointing at the cap: {r}");
    assert_full(port);
    server.stop().unwrap();
}

/// Many SETUPs at once against a cap: exactly the cap's number are
/// admitted, the rest read 453.
#[test]
fn a_burst_of_setups_never_exceeds_the_reader_cap() {
    const READERS: usize = 12;
    const CAP: usize = 3;
    let server = start(|b| {
        b.max_sessions(READERS + 1).max_readers(CAP);
    });
    let port = server.local_addr().unwrap().port();
    let start = std::sync::Arc::new(std::sync::Barrier::new(READERS));
    let threads: Vec<_> = (0..READERS)
        .map(|_| {
            let start = start.clone();
            std::thread::spawn(move || {
                let mut tcp = connect(port);
                start.wait();
                let r = setup(&mut tcp, port, "/live", 1, "");
                (status(&r), tcp)
            })
        })
        .collect();
    // The connections stay open until every reply is counted, so no slot
    // is given back meanwhile.
    let replies: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let admitted = replies.iter().filter(|(s, _)| *s == 200).count();
    let refused = replies.iter().filter(|(s, _)| *s == 453).count();
    assert_eq!((admitted, refused), (CAP, READERS - CAP));
    drop(replies);
    server.stop().unwrap();
}

/// A reader of a multicast mount takes a slot like any reader. The SETUP
/// reply names the group; no media is needed, so the test runs where the
/// host has no multicast route.
#[test]
fn a_reader_of_a_multicast_mount_takes_a_slot() {
    let server = start(|b| {
        b.max_readers(1);
    });
    server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.55.55.20:55060")
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let mut m = connect(port);
    let r = request(
        &mut m,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/mc/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP;multicast\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let mut c = connect(port);
    let r = setup(&mut c, port, "/live", 1, "");
    assert_eq!(status(&r), 453, "the multicast reader holds the slot: {r}");
    server.stop().unwrap();
}

/// `max_readers(0)`: no reader is admitted, a publisher is.
#[test]
fn max_readers_zero_refuses_every_reader_and_admits_a_publisher() {
    let server = start(|b| {
        b.max_readers(0);
    });
    let port = server.local_addr().unwrap().port();
    let mut c = connect(port);
    let r = setup(&mut c, port, "/live", 1, "");
    assert_eq!(status(&r), 453, "{r}");
    let mut publisher = RawPublisher::connect(port);
    assert_eq!(publisher.announce("/ingest", SDP_MP2T), 200);
    assert_eq!(publisher.setup_interleaved("/ingest", "streamid=0").0, 200);
    assert_eq!(publisher.record("/ingest"), 200);
    server.stop().unwrap();
}

/// Without a cap, readers are limited by `max_sessions` alone.
#[test]
fn without_a_cap_nothing_changes() {
    let server = start(|b| {
        b.max_sessions(8);
    });
    let port = server.local_addr().unwrap().port();
    let readers: Vec<_> = (0..8).map(|_| reader(port, "/live")).collect();
    assert_eq!(readers.len(), 8);
    server.stop().unwrap();
}

/// A rule that refuses with the cap's own status is a rule, not the cap:
/// the connection stays open.
#[test]
fn a_rule_that_refuses_with_453_leaves_the_connection_open() {
    let server = start(|b| {
        b.max_readers(4);
    });
    server
        .mount_rules()
        .set(
            "/live",
            MountRule::both(MountPolicy::Refuse(RefuseStatus::NotEnoughBandwidth)),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let mut c = connect(port);
    let r = setup(&mut c, port, "/live", 1, "");
    assert_eq!(status(&r), 453, "{r}");
    let r = request(
        &mut c,
        &format!("OPTIONS rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 2\r\n\r\n"),
    );
    assert_eq!(status(&r), 200, "the connection stayed open: {r}");
    server.stop().unwrap();
}
