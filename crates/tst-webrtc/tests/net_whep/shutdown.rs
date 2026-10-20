//! Stopping the server: bounded with a viewer streaming and a request
//! half sent, idempotent, and the ports free afterwards.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::{MountConfig, PushError, WhepServer, WhepServerError};

use crate::fixtures::{TestViewer, eventually, h264_gop, join, parse_response};

/// How long the test waits for `stop` to report back. `stop` itself waits
/// at most 2 s per thread; this is the test's own bound, not a timing
/// assertion on `stop`.
const STOP_REPORT: Duration = Duration::from_secs(3);

#[test]
fn stop_returns_within_bound_with_a_connected_viewer_and_a_parked_poster() {
    let srv = Arc::new(WhepServer::bind("whep://127.0.0.1:0").unwrap());
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut v, true);
    assert!(
        v.pump(|v| v.connected && v.dc_open, Duration::from_secs(5)),
        "{}",
        v.trace()
    );
    let aus = h264_gop();
    m.push_video(&aus[0].0, Pts90khz::new(0), true).unwrap();
    assert!(
        v.pump(|v| !v.frames.is_empty(), Duration::from_secs(5)),
        "{}",
        v.trace()
    );

    // A POST whose body never finishes: the accept thread waits on it.
    let addr = srv.local_addr();
    let (written_tx, written_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"POST /cam HTTP/1.1\r\nContent-Length: 1000\r\n\r\nv=0\r\n")
            .unwrap();
        written_tx.send(()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut raw = Vec::new();
        let outcome = s.read_to_end(&mut raw).map(|_| raw);
        let _ = answer_tx.send(outcome);
    });
    written_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Keep the viewer streaming while the request is parked.
    m.push_video(&aus[1].0, Pts90khz::new(3000), false).unwrap();
    assert!(
        v.pump(|v| v.frames.len() >= 2, Duration::from_secs(5)),
        "{}",
        v.trace()
    );

    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stopper = Arc::clone(&srv);
    thread::spawn(move || {
        stopper.stop();
        let _ = stopped_tx.send(());
    });
    stopped_rx
        .recv_timeout(STOP_REPORT)
        .expect("stop() returned");

    // The parked request is answered 503, or its connection is closed.
    let outcome = answer_rx
        .recv_timeout(STOP_REPORT)
        .expect("the poster finished");
    if let Ok(raw) = outcome
        && !raw.is_empty()
    {
        let (status, _, _) = parse_response(&raw).expect("an HTTP response");
        assert_eq!(status, 503);
    }
    assert!(matches!(
        m.push_video(&aus[2].0, Pts90khz::new(6000), false),
        Err(PushError::Closed)
    ));
    assert!(m.sessions().is_empty());
    srv.stop();
    assert!(matches!(
        srv.add_mount("/late", MountConfig::default()),
        Err(WhepServerError::Shutdown)
    ));
}

#[test]
fn drop_stops_and_frees_the_port() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let (http_port, media_port) = (srv.local_addr().port(), srv.media_addr().port());
    drop(srv);
    assert!(matches!(
        m.push_video(&h264_gop()[0].0, Pts90khz::new(0), true),
        Err(PushError::Closed)
    ));
    let again = WhepServer::bind(&format!(
        "whep://127.0.0.1:{http_port}?media_port={media_port}"
    ))
    .expect("both ports are free after drop");
    assert_eq!(again.local_addr().port(), http_port);
    assert_eq!(again.media_addr().port(), media_port);
}

#[test]
fn stop_before_any_viewer_is_immediate_and_idempotent() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    srv.stop();
    srv.stop();
    assert!(matches!(
        m.push_klv(&[0x06], Pts90khz::new(0), 0),
        Err(PushError::Closed)
    ));
    assert!(matches!(
        srv.add_mount("/cam2", MountConfig::default()),
        Err(WhepServerError::Shutdown)
    ));
    let s = srv.stats();
    assert_eq!((s.mounts, s.active_sessions), (1, 0));
    assert!(eventually(
        || m.sessions().is_empty(),
        Duration::from_secs(1)
    ));
}
