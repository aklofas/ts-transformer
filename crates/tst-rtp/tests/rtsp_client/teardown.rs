use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn drop_sends_teardown_best_effort() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();
    // Drop without explicit teardown — Drop impl should send TEARDOWN.
    drop(client);
    // No assertion — drop completes without panicking.
    drop(h);
}

#[test]
fn explicit_teardown_clears_session() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let _s = client.setup_mp2t_auto(&sdp).unwrap();
    client.teardown().unwrap();
    drop(client);
    drop(h);
}

/// A dropped interleaved client on a quiet session still writes TEARDOWN
/// when its `read_timeout` is longer than Drop's 500 ms bound: no read in
/// progress holds the TEARDOWN back.
#[test]
fn drop_writes_teardown_on_a_quiet_interleaved_session_with_a_long_read_timeout() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;
    use crate::fixtures::raw_rtsp_server::{
        answer_describe_and_setup, cseq_of, method_of, read_request,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "PLAY", "unexpected request: {req}");
        let cseq = cseq_of(&req);
        sock.write_all(
            format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n").as_bytes(),
        )
        .unwrap();
        // Send nothing more. Record what the client writes next, until a
        // TEARDOWN or the connection ends.
        let mut rest = leftover;
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&rest).contains("TEARDOWN ") {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&buf[..n]),
            }
        }
        let _ = seen_tx.send(String::from_utf8_lossy(&rest).into_owned());
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(Duration::from_secs(1))
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    // The pause leaves the session quiet for a while before Drop; whichever
    // way that goes, Drop must write TEARDOWN.
    client.play().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(client);

    let seen = seen_rx
        .recv_timeout(scaled(Duration::from_secs(10)))
        .expect("the peer saw a TEARDOWN or the connection end");
    assert!(
        seen.starts_with("TEARDOWN "),
        "the dropped client must write TEARDOWN; the peer saw {seen:?}"
    );
    drop(session);
    peer.join().unwrap();
}
