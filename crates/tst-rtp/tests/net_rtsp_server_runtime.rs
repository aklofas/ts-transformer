//! The RTSP server's private runtime: two named worker threads, and a
//! `stop()` that waits for its sessions instead of sleeping.
//!
//! This is its own binary on purpose: the thread probe counts OS threads
//! by name before and after `build()`, so no other test in the process may
//! build a server at the same time. The tests here share one lock.
#![cfg(feature = "rtsp-server")]

use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tst_rtp::RtspServerBuilder;

static SERIAL: Mutex<()> = Mutex::new(());

const PROC_TASKS: &str = "/proc/self/task";

fn count_named(name: &str) -> usize {
    std::fs::read_dir(PROC_TASKS)
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.unwrap().path().join("comm")).ok())
        .filter(|c| c.trim_end() == name)
        .count()
}

fn server_with_drain(drain: Duration) -> tst_rtp::RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.graceful_shutdown_drain(drain);
    b.build().unwrap()
}

/// With no session open, nothing is left to wait for. The old fixed sleep
/// was the drain plus one second, so a 5 s drain made it 6 s; the bound
/// here is the drain itself, not a tighter wall-clock guess.
#[test]
fn stop_with_no_sessions_does_not_sleep_the_drain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let drain = Duration::from_secs(5);
    let server = server_with_drain(drain);
    server.start().unwrap();
    let t = Instant::now();
    server.stop().unwrap();
    assert!(t.elapsed() < drain, "stop took {:?}", t.elapsed());
}

/// A session that ends on cancel lets `stop()` return as soon as it has
/// ended, well inside the drain.
#[test]
fn stop_returns_once_the_session_has_ended() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let drain = Duration::from_secs(5);
    let server = server_with_drain(drain);
    server.start().unwrap();
    let peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
    let wait = Instant::now();
    while server.stats().active_sessions == 0 {
        assert!(
            wait.elapsed() < Duration::from_secs(10),
            "session never registered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let t = Instant::now();
    server.stop().unwrap();
    assert!(t.elapsed() < drain, "stop took {:?}", t.elapsed());
    drop(peer);
}

/// The runtime has exactly two workers, named `tst-rtp-server`, and they
/// are gone once the server is dropped.
#[test]
fn runtime_has_two_workers_named_tst_rtp_server() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if !std::path::Path::new(PROC_TASKS).is_dir() {
        eprintln!("skipped: {PROC_TASKS} is not available on this platform");
        return;
    }
    let before = count_named("tst-rtp-server");
    let server = server_with_drain(Duration::from_millis(100));
    server.start().unwrap();
    // Worker threads are spawned lazily by tokio; give them work.
    drop(TcpStream::connect(server.local_addr().unwrap()).unwrap());
    let wait = Instant::now();
    while count_named("tst-rtp-server") < before + 2 && wait.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(count_named("tst-rtp-server"), before + 2);
    server.stop().unwrap();
    drop(server);
    // The kernel wakes a joiner a moment before it removes the exited
    // task's /proc entry, so poll for the count to settle.
    let wait = Instant::now();
    while count_named("tst-rtp-server") != before && wait.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(count_named("tst-rtp-server"), before);
}
