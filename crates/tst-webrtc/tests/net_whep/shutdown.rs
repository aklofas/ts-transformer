//! Stopping the server: bounded with a viewer streaming and a request
//! half sent, idempotent, and the ports free when it returns. Dropping the
//! server cancels without waiting; the ports free soon after.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::url::WhepUrl;
use tst_webrtc::{Clock, MountConfig, MountHandle, PushError, WhepServer, WhepServerError};

use crate::fixtures::{STOP_REPORT, TestViewer, eventually, h264_gop, join, parse_response};

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

/// Rebinds a server on exactly these two ports.
fn rebind(http_port: u16, media_port: u16) -> Result<WhepServer, WhepServerError> {
    WhepServer::bind(&format!(
        "whep://127.0.0.1:{http_port}?media_port={media_port}"
    ))
}

#[test]
fn stop_frees_both_ports() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let (http_port, media_port) = (srv.local_addr().port(), srv.media_addr().port());
    srv.stop();
    assert!(srv.threads_joined_for_test());
    let again = rebind(http_port, media_port).expect("both ports are free after stop");
    assert_eq!(again.local_addr().port(), http_port);
    assert_eq!(again.media_addr().port(), media_port);
}

#[test]
fn drop_closes_mounts_at_once_and_frees_the_ports_soon() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let (http_port, media_port) = (srv.local_addr().port(), srv.media_addr().port());
    drop(srv);
    assert!(matches!(
        m.push_video(&h264_gop()[0].0, Pts90khz::new(0), true),
        Err(PushError::Closed)
    ));
    assert!(
        eventually(
            || rebind(http_port, media_port).is_ok(),
            Duration::from_secs(5)
        ),
        "both ports free after drop"
    );
}

/// A clock that, once armed, holds the media thread inside `now()` until
/// the gate opens, and reports on `parked` when it first does.
struct GatedClock {
    /// (armed, open)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
    parked: Mutex<Option<mpsc::Sender<()>>>,
}

impl GatedClock {
    fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let clock = Arc::new(Self {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
            parked: Mutex::new(Some(tx)),
        });
        (clock, rx)
    }

    fn arm(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).0 = true;
    }

    fn open(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.changed.notify_all();
    }
}

impl Clock for GatedClock {
    fn now(&self) -> Instant {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 && !state.1 {
            if let Some(tx) = self
                .parked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let _ = tx.send(());
            }
            while !state.1 {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        Instant::now()
    }
}

/// A server on `clock` with one mount whose media thread is held inside
/// the clock when this returns.
fn parked_server(
    clock: &Arc<GatedClock>,
    parked: &mpsc::Receiver<()>,
) -> (WhepServer, MountHandle) {
    let url = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    let srv = WhepServer::bind_with_clock(url, clock.clone()).unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    clock.arm();
    // The push wakes the media thread, which then reads the clock.
    m.push_video(&h264_gop()[0].0, Pts90khz::new(0), true)
        .unwrap();
    parked
        .recv_timeout(Duration::from_secs(5))
        .expect("the media thread parked in the clock");
    (srv, m)
}

#[test]
fn drop_does_not_wait_for_a_thread_that_cannot_exit() {
    let (clock, parked) = GatedClock::new();
    let (srv, _m) = parked_server(&clock, &parked);
    let t0 = Instant::now();
    drop(srv);
    let took = t0.elapsed();
    clock.open();
    assert!(took < Duration::from_secs(1), "drop took {took:?}");
}

#[test]
fn a_detached_media_thread_stops_when_its_queue_is_full() {
    let (clock, parked) = GatedClock::new();
    let (srv, m) = parked_server(&clock, &parked);
    let media_port = srv.media_addr().port();
    // Fill the media queue so the drop's stop message cannot be queued.
    let idr = &h264_gop()[0].0;
    let mut pts = 0;
    while m.stats().viewer_packets_dropped_queue_full == 0 {
        pts += 3000;
        m.push_video(idr, Pts90khz::new(pts), true).unwrap();
    }
    drop(srv);
    clock.open();
    assert!(
        eventually(
            || WhepServer::bind(&format!("whep://127.0.0.1:0?media_port={media_port}")).is_ok(),
            Duration::from_secs(5)
        ),
        "the media port is free once the media thread runs again"
    );
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

#[test]
fn a_mount_added_while_stop_runs_is_refused_or_closed() {
    const ADDERS: usize = 4;
    for round in 0..50 {
        let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
        let added = AtomicUsize::new(0);
        let start = Barrier::new(ADDERS + 1);
        let handles = thread::scope(|s| {
            let adders: Vec<_> = (0..ADDERS)
                .map(|t| {
                    let (srv, added, start) = (&srv, &added, &start);
                    s.spawn(move || {
                        start.wait();
                        let mut mine = Vec::new();
                        for k in 0.. {
                            match srv.add_mount(&format!("/r{t}-{k}"), MountConfig::default()) {
                                Ok(m) => {
                                    mine.push(m);
                                    added.fetch_add(1, Ordering::Release);
                                }
                                Err(WhepServerError::Shutdown) => break,
                                Err(e) => panic!("unexpected add_mount error: {e}"),
                            }
                        }
                        mine
                    })
                })
                .collect();
            start.wait();
            // Stop once at least ADDERS mounts were added, so the adders are
            // running and some add_mount calls straddle the moment stop begins.
            while added.load(Ordering::Acquire) < ADDERS {
                thread::yield_now();
            }
            srv.stop();
            adders
                .into_iter()
                .flat_map(|a| a.join().unwrap())
                .collect::<Vec<_>>()
        });
        // Every mount add_mount handed out is closed by stop: a push on it
        // is refused, never buffered for a server that is gone.
        for m in &handles {
            assert!(
                matches!(
                    m.push_klv(&[0x06], Pts90khz::new(0), 0),
                    Err(PushError::Closed)
                ),
                "round {round}: {} was added during stop and left open",
                m.mount_path()
            );
        }
    }
}
