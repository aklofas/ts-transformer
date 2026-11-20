use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn setup_play_udp_succeeds() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let opts = client.options().unwrap();
    assert!(opts.public_methods.contains(&"PLAY".to_string()));
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    assert_eq!(session.transport_kind(), tst_rtp::RtspTransportKind::Udp);
    let info = client.play().unwrap();
    assert_eq!(info.seq, Some(1234));
    client.teardown().unwrap();
    drop(client);
    drop(h);
}

#[test]
fn setup_play_tcp_interleaved_succeeds() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let _opts = client.options().unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    assert_eq!(
        session.transport_kind(),
        tst_rtp::RtspTransportKind::TcpInterleaved
    );
    client.play().unwrap();
    client.teardown().unwrap();
    drop(client);
    drop(h);
}

/// A server answering DESCRIBE with a malformed SDP body (the smallest input
/// known to make an SDP parser assert internally) yields `BadSdp` on the
/// caller's thread rather than a panic.
#[test]
fn malformed_describe_sdp_is_bad_sdp() {
    let cfg = FixtureConfig {
        sdp_body: b"v=0\n\xff\n\0=".to_vec(),
        ..FixtureConfig::default()
    };
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let res = client.describe();
    assert!(
        matches!(res, Err(tst_rtp::RtspError::BadSdp { .. })),
        "expected BadSdp, got {res:?}"
    );
    drop(client);
    drop(h);
}

/// A request on a TCP-interleaved session with no media flowing is written
/// and answered at once: nothing holds it behind a read waiting for data,
/// whatever `read_timeout` is configured to.
#[test]
// Pins what the deprecated `read_timeout` does not bound.
#[allow(deprecated)]
fn a_request_on_a_quiet_interleaved_session_does_not_wait_for_a_read() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp::scaled;
    use crate::fixtures::raw_rtsp_server::{
        answer_describe_and_setup, cseq_of, method_of, read_request,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        // Answer PLAY and PAUSE, send no media.
        for expected in ["PLAY", "PAUSE"] {
            let req = read_request(&mut sock, &mut leftover);
            assert_eq!(method_of(&req), expected, "unexpected request: {req}");
            let cseq = cseq_of(&req);
            sock.write_all(
                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n").as_bytes(),
            )
            .unwrap();
        }
        // Hold the connection until the client hangs up.
        let _ = sock.read_to_end(&mut Vec::new());
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(Duration::from_secs(2))
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    client.play().unwrap();

    let t0 = Instant::now();
    client.pause().unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < scaled(Duration::from_secs(1)),
        "PAUSE on a quiet interleaved session took {elapsed:?}"
    );

    drop(session);
    drop(client);
    peer.join().unwrap();
}
