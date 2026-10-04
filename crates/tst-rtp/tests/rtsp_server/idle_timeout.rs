//! Review 9 (int R9-03): the server advertised `Session: <id>;timeout=60`
//! but re-armed a fixed 30 s read-idle timer per request, so a conformant
//! client pinging at timeout/2 (ours, ffmpeg) lost the race at the second
//! ping. Only the orphan fanout (R9-02) hid it: media kept flowing after
//! the reap. The idle bound now derives from what SETUP advertised.

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tst_rtp::RtspServerBuilder;

use crate::fixtures::raw_rtsp::{header, make_muxer_cfg, request, session_id};

fn interleaved_session(timeout_secs: u64) -> (tst_rtp::RtspServer, TcpStream, String, String) {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.session_timeout(Duration::from_secs(timeout_secs));
    let server = b.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // Per-response read bound only — generous for the slow macOS/Windows
    // runners; no test below asserts anything through its duration.
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    assert!(
        header(&setup, "Session").ends_with(&format!(";timeout={timeout_secs}")),
        "{setup}"
    );
    let sid = session_id(&setup);
    let play = request(
        &mut tcp,
        &format!("PLAY {url} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");
    (server, tcp, url, sid)
}

/// RED today: the connection stays open for the fixed 30 s.
#[test]
fn an_idle_session_is_reaped_at_the_advertised_timeout_plus_grace() {
    let (server, mut tcp, _url, _sid) = interleaved_session(1);
    // No keepalive. The bound is 1 s + max(0.5 s, 2 s) = 3 s; the 10 s
    // deadline (checked at each 5 s read timeout) is far above it and far
    // below the old 30 s, so no timing is asserted.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 4096];
    loop {
        match tcp.read(&mut buf) {
            Ok(0) => break, // FIN: the server shut the write half (R9-02)
            Ok(_) => {}     // a stray interleaved frame; keep reading
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                break;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                assert!(
                    Instant::now() < deadline,
                    "connection still open 10 s after PLAY with no keepalive (advertised timeout 1 s)"
                );
            }
            Err(e) => panic!("unexpected read error: {e}"),
        }
    }
    server.stop().ok();
}

/// Guard against over-tightening: a client that keeps pinging inside the
/// advertised timeout stays alive across several ping intervals (10 s
/// advertised → 15 s bound; six pings 1 s apart span ≈ 6 s, so a runner
/// stall of several seconds cannot reach the bound). Green before and
/// after the fix.
#[test]
fn keepalive_pings_inside_the_timeout_keep_the_session_alive() {
    let (server, mut tcp, url, sid) = interleaved_session(10);
    for cseq in 3..9 {
        std::thread::sleep(Duration::from_secs(1));
        let pong = request(
            &mut tcp,
            &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"),
        );
        assert!(
            pong.starts_with("RTSP/1.0 200"),
            "ping {cseq} was not answered: {pong}"
        );
    }
    let bye = request(
        &mut tcp,
        &format!("TEARDOWN {url} RTSP/1.0\r\nCSeq: 9\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(bye.starts_with("RTSP/1.0 200"), "{bye}");
    server.stop().ok();
}
