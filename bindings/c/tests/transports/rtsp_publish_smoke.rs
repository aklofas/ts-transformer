//! Smoke tests for the RTSP publisher role over the C ABI: publish mounts
//! (`tst_rtsp_server_add_publish_mount` + the `tst_rtsp_publish_mount_*`
//! family), the on-demand queue (`tst_rtsp_server_next_publisher`),
//! `tst_rtsp_server_remove_mount`, and the server's publisher counters.
//!
//! The publisher side is a raw std `TcpStream` speaking ANNOUNCE / SETUP
//! (`mode=record`, TCP-interleaved) / RECORD by hand: the library's RTSP
//! client only plays. Every read is bounded by a 2 s socket timeout and every
//! blocking C call that could park has a watchdog, so a regression fails
//! instead of hanging the suite.
#![cfg(feature = "rtp")]

use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

use tstrans::error::TstError;
use tstrans::event::{TstEvent, TstEventKind};
use tstrans::rtp::{
    tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close, tst_rtp_demux_receiver_next_event,
};
use tstrans::stats::TstServerStats;
use tstrans::{
    TstRtspClockAlignment, TstRtspPublishMountStats, TstRtspPublishShape, TstRtspPublisherInfo,
    test_last_error_code, tst_rtsp_publish_mount_cancel, tst_rtsp_publish_mount_free,
    tst_rtsp_publish_mount_generation, tst_rtsp_publish_mount_get_stats,
    tst_rtsp_publish_mount_into_demux_receiver, tst_rtsp_publish_mount_path,
    tst_rtsp_publish_mount_peer_count, tst_rtsp_publish_mount_publisher_info,
    tst_rtsp_server_active_publishers, tst_rtsp_server_add_publish_mount,
    tst_rtsp_server_builder_accept_unregistered_publishers, tst_rtsp_server_builder_new,
    tst_rtsp_server_builder_start, tst_rtsp_server_free, tst_rtsp_server_get_stats,
    tst_rtsp_server_local_addr, tst_rtsp_server_next_publisher, tst_rtsp_server_remove_mount,
    tst_rtsp_server_stop, tst_rtsp_server_total_rtp_bytes_received,
    tst_rtsp_server_total_rtp_packets_received,
};

const SDP_MP2T: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\na=control:streamid=0\r\n";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Start a server on a kernel-picked loopback port, retrying if the port is
/// taken between the probe and the bind. Returns the server and its port.
/// (`rtsp_publish_server_local_addr` covers the `:0` bind read back through
/// `tst_rtsp_server_local_addr`.)
fn start_server(accept_unregistered: bool) -> (*mut tstrans::TstRtspServer, u16) {
    for _ in 0..5 {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("probe port")
            .port();
        let url = CString::new(format!("rtsp://127.0.0.1:{port}")).unwrap();
        let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
        assert!(!b.is_null(), "builder_new failed");
        unsafe { tst_rtsp_server_builder_accept_unregistered_publishers(b, accept_unregistered) };
        let s = unsafe { tst_rtsp_server_builder_start(b) };
        if !s.is_null() {
            return (s, port);
        }
    }
    panic!("could not start an RTSP server on a loopback port");
}

/// Write one request and read the response head (up to CRLFCRLF).
fn request(tcp: &mut TcpStream, req: &str) -> String {
    tcp.write_all(req.as_bytes()).expect("request written");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tcp.read(&mut chunk).expect("server answered in time");
        assert!(n > 0, "server closed before answering {req:?}");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {resp:?}"))
}

/// A minimal MP2T publisher: ANNOUNCE, SETUP `mode=record` over
/// TCP-interleaved, RECORD. Returns the open control connection (the
/// publisher holds the mount for as long as it stays open) and the RTP
/// channel the server granted.
fn publish_mp2t(port: u16, path: &str) -> (TcpStream, u8) {
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).expect("publisher connects");
    tcp.set_nodelay(true).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let uri = format!("rtsp://127.0.0.1:{port}{path}");
    let r = request(
        &mut tcp,
        &format!(
            "ANNOUNCE {uri} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP_MP2T}",
            SDP_MP2T.len()
        ),
    );
    assert_eq!(status(&r), 200, "ANNOUNCE refused: {r}");
    let r = request(
        &mut tcp,
        &format!(
            "SETUP {uri}/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 200, "SETUP refused: {r}");
    let header = |name: &str| -> String {
        r.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("no {name} header in {r:?}"))
    };
    let session = header("Session")
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_owned();
    let channel: u8 = header("Transport")
        .split(';')
        .find_map(|p| p.trim().strip_prefix("interleaved="))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse().ok())
        .expect("interleaved channel in the SETUP answer");
    let r = request(
        &mut tcp,
        &format!("RECORD {uri} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"),
    );
    assert_eq!(status(&r), 200, "RECORD refused: {r}");
    (tcp, channel)
}

/// `n` RTP packets (PT 33), each carrying a bundle of up to 7 TS packets
/// pulled from a real muxer fed synthetic H.264 access units, so the
/// demuxer sees PAT, PMT and video.
fn mp2t_rtp_packets(n: usize) -> Vec<Vec<u8>> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut cfg = MuxerConfig::builder();
    cfg.add_program(prog.build());
    let mut mux = tst_core::mpegts::mux::Muxer::new(cfg.build().unwrap()).expect("muxer");
    let mut bundles = Vec::new();
    let mut buf = [0u8; 7 * 188];
    let mut i: i64 = 0;
    while bundles.len() < n {
        let mut au = vec![0u8, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }];
        au.extend((0..295).map(|k| (k as u8).wrapping_add(i as u8) | 1));
        mux.push_video(&au, Pts90khz::new(i * 3003), i == 0)
            .expect("push AU");
        loop {
            let got = mux.pull(&mut buf);
            if got == 0 || bundles.len() == n {
                break;
            }
            bundles.push(buf[..got].to_vec());
        }
        i += 1;
    }
    bundles
        .into_iter()
        .enumerate()
        .map(|(seq, ts)| {
            let mut p = vec![0x80u8, 33];
            p.extend_from_slice(&(seq as u16).to_be_bytes());
            p.extend_from_slice(&(seq as u32 * 3003).to_be_bytes());
            p.extend_from_slice(&0x1234_5678u32.to_be_bytes());
            p.extend_from_slice(&ts);
            p
        })
        .collect()
}

/// One RFC 2326 §10.12 interleaved frame.
fn send_frame(tcp: &mut TcpStream, ch: u8, payload: &[u8]) {
    let mut f = vec![b'$', ch];
    f.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    f.extend_from_slice(payload);
    tcp.write_all(&f).expect("frame written");
}

/// Poll `cond` every 20 ms until it holds or `limit` passes.
fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// Cancels a demux receiver if the caller has not finished within `limit`.
/// Join it (via `finish`) before closing the receiver.
struct Watchdog {
    done: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Watchdog {
    fn arm(rx: *mut tstrans::rtp::TstRtpDemuxReceiver, limit: Duration) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let addr = rx as usize;
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                if flag.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !flag.load(Ordering::Acquire) {
                unsafe { tst_rtp_demux_receiver_cancel(addr as *mut _) };
            }
        });
        Self { done, thread }
    }

    fn finish(self) {
        self.done.store(true, Ordering::Release);
        self.thread.join().expect("watchdog thread panicked");
    }
}

fn stats(m: *const tstrans::TstRtspPublishMount) -> TstRtspPublishMountStats {
    let mut out = TstRtspPublishMountStats::default();
    let rc = unsafe { tst_rtsp_publish_mount_get_stats(m, &mut out) };
    assert_eq!(rc, 0, "get_stats failed: {rc}");
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A registered publish mount end to end: the application takes the
/// transport, a publisher pushes five MP2T packets, the demux receiver sees
/// the program, the mount's and the server's counters move, and
/// `remove_mount` ends the receiver with END_OF_STREAM.
#[test]
fn rtsp_publish_mount_end_to_end() {
    let (server, port) = start_server(false);
    let path = CString::new("/pub").unwrap();
    let m = unsafe { tst_rtsp_server_add_publish_mount(server, path.as_ptr()) };
    assert!(
        !m.is_null(),
        "add_publish_mount failed: {}",
        test_last_error_code()
    );

    let p = unsafe { tst_rtsp_publish_mount_path(m) };
    assert!(!p.is_null());
    assert_eq!(unsafe { CStr::from_ptr(p) }.to_str().unwrap(), "/pub");

    // Registering the same path again is a mount error.
    let dup = unsafe { tst_rtsp_server_add_publish_mount(server, path.as_ptr()) };
    assert!(dup.is_null());
    assert_eq!(test_last_error_code(), TstError::RtspMount as i32);

    // The on-demand queue is empty (the flag is off): a timeout, not an error.
    let mut out: *mut tstrans::TstRtspPublishMount = 0x1 as *mut _;
    let rc = unsafe { tst_rtsp_server_next_publisher(server, 100, &mut out) };
    assert_eq!(
        rc,
        TstError::BufferFull as i32,
        "next_publisher timeout code"
    );
    assert!(out.is_null(), "*out must be NULL on a timeout");

    // Idle mount: no publisher yet (`present` starts true so the call must
    // clear it).
    let mut info = TstRtspPublisherInfo {
        present: true,
        ..TstRtspPublisherInfo::default()
    };
    assert_eq!(
        unsafe { tst_rtsp_publish_mount_publisher_info(m, &mut info) },
        0
    );
    assert!(!info.present, "an idle mount has no publisher");

    // Take the transport (allocates the application queue), once.
    let rx = unsafe { tst_rtsp_publish_mount_into_demux_receiver(m, std::ptr::null()) };
    assert!(
        !rx.is_null(),
        "into_demux_receiver failed: {}",
        test_last_error_code()
    );
    let again = unsafe { tst_rtsp_publish_mount_into_demux_receiver(m, std::ptr::null()) };
    assert!(again.is_null(), "the transport is take-once");
    assert_eq!(test_last_error_code(), TstError::Closed as i32);

    let (mut tcp, ch) = publish_mp2t(port, "/pub");
    for pkt in mp2t_rtp_packets(5) {
        send_frame(&mut tcp, ch, &pkt);
    }
    tcp.flush().unwrap();

    assert!(
        wait_for(Duration::from_secs(5), || stats(m).rtp_packets_received
            == 5),
        "mount never counted 5 RTP packets: {}",
        stats(m).rtp_packets_received
    );
    let s = stats(m);
    assert!(s.bytes_received > 0);
    assert!(s.frames_emitted > 0);
    assert!(matches!(s.alignment, TstRtspClockAlignment::NotApplicable));
    assert_eq!(s.generation, 0);

    let mut info = TstRtspPublisherInfo::default();
    assert_eq!(
        unsafe { tst_rtsp_publish_mount_publisher_info(m, &mut info) },
        0
    );
    assert!(info.present);
    assert!(matches!(info.shape, TstRtspPublishShape::Mp2t));
    assert!(!info.klv);
    assert_eq!(info.generation, 0);
    assert!(info.since_unix_ms > 0);
    let peer = unsafe { CStr::from_ptr(info.peer.as_ptr()) }
        .to_str()
        .unwrap();
    assert!(peer.starts_with("127.0.0.1:"), "peer = {peer:?}");

    let mut n = u64::MAX;
    assert_eq!(unsafe { tst_rtsp_publish_mount_peer_count(m, &mut n) }, 0);
    assert_eq!(n, 0, "no PLAY readers");
    assert_eq!(unsafe { tst_rtsp_publish_mount_generation(m, &mut n) }, 0);
    assert_eq!(n, 0);

    let mut v = 0u64;
    assert_eq!(
        unsafe { tst_rtsp_server_active_publishers(server, &mut v) },
        0
    );
    assert_eq!(v, 1);
    assert_eq!(
        unsafe { tst_rtsp_server_total_rtp_packets_received(server, &mut v) },
        0
    );
    assert_eq!(v, 5);
    assert_eq!(
        unsafe { tst_rtsp_server_total_rtp_bytes_received(server, &mut v) },
        0
    );
    assert_eq!(v, s.bytes_received);
    // The existing server stats struct is unchanged and still counts mounts.
    let mut ss = TstServerStats::default();
    assert_eq!(unsafe { tst_rtsp_server_get_stats(server, &mut ss) }, 0);
    assert_eq!(ss.mounts, 1);

    // The demux receiver sees the program the publisher sent.
    let dog = Watchdog::arm(rx, Duration::from_secs(5));
    let mut ev = TstEvent::default();
    let mut got_pmt = false;
    while unsafe { tst_rtp_demux_receiver_next_event(rx, &mut ev) } == 0 {
        if ev.kind == TstEventKind::ProgramMap as i32 {
            got_pmt = true;
            break;
        }
    }
    dog.finish();
    assert!(got_pmt, "no PROGRAM_MAP event from the published stream");

    // remove_mount closes the mount: the receiver drains, then reads
    // END_OF_STREAM (the mount ended; an explicit cancel would read CLOSED).
    assert_eq!(
        unsafe { tst_rtsp_server_remove_mount(server, path.as_ptr()) },
        0
    );
    assert_eq!(
        unsafe { tst_rtsp_server_remove_mount(server, path.as_ptr()) },
        TstError::RtspMount as i32,
        "removing an unknown mount"
    );
    let dog = Watchdog::arm(rx, Duration::from_secs(5));
    let end = loop {
        let rc = unsafe { tst_rtp_demux_receiver_next_event(rx, &mut ev) };
        if rc != 0 {
            break rc;
        }
    };
    dog.finish();
    assert_eq!(
        end,
        TstError::EndOfStream as i32,
        "a removed mount's receiver ends END_OF_STREAM"
    );

    // Getters keep working on the closed mount.
    assert_eq!(stats(m).rtp_packets_received, 5);
    assert_eq!(unsafe { tst_rtsp_publish_mount_cancel(m) }, 0);

    unsafe { tst_rtp_demux_receiver_close(rx) };
    unsafe { tst_rtsp_publish_mount_free(m) };
    drop(tcp);
    unsafe { tst_rtsp_server_free(server) };
}

/// The on-demand path: an ANNOUNCE to an unregistered path creates a mount
/// that `next_publisher` hands out; a call parked in `next_publisher` wakes
/// with CLOSED when another thread stops the server.
#[test]
fn rtsp_publish_next_publisher_on_demand_and_stop_wakes_parked_call() {
    let (server, port) = start_server(true);

    let (tcp, _ch) = publish_mp2t(port, "/live/cam1");
    let mut out: *mut tstrans::TstRtspPublishMount = std::ptr::null_mut();
    let rc = unsafe { tst_rtsp_server_next_publisher(server, 5_000, &mut out) };
    assert_eq!(rc, 0, "next_publisher: {rc}");
    assert!(!out.is_null());
    let p = unsafe { CStr::from_ptr(tst_rtsp_publish_mount_path(out)) };
    assert_eq!(p.to_str().unwrap(), "/live/cam1");
    let mut info = TstRtspPublisherInfo::default();
    assert_eq!(
        unsafe { tst_rtsp_publish_mount_publisher_info(out, &mut info) },
        0
    );
    assert!(
        info.present,
        "the announcing publisher holds the on-demand mount"
    );
    unsafe { tst_rtsp_publish_mount_free(out) };

    // Park a call on another thread, then stop the server from this one.
    let addr = server as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let parked = std::thread::spawn(move || {
        let mut out: *mut tstrans::TstRtspPublishMount = 0x1 as *mut _;
        ready_tx.send(()).unwrap();
        let rc = unsafe { tst_rtsp_server_next_publisher(addr as *mut _, 30_000, &mut out) };
        tx.send((rc, out.is_null())).unwrap();
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("parked thread started");
    // Ordering aid only (not asserted): give the side thread time to enter
    // the wait, so the stop below exercises the wake path rather than the
    // already-stopped check.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(unsafe { tst_rtsp_server_stop(server, 0) }, 0);
    let (rc, out_null) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stop() must wake a parked next_publisher");
    parked.join().unwrap();
    assert_eq!(rc, TstError::Closed as i32);
    assert!(out_null);

    // After stop: every server call reads CLOSED.
    let mut out: *mut tstrans::TstRtspPublishMount = std::ptr::null_mut();
    assert_eq!(
        unsafe { tst_rtsp_server_next_publisher(server, 10, &mut out) },
        TstError::Closed as i32
    );
    let path = CString::new("/after").unwrap();
    assert!(unsafe { tst_rtsp_server_add_publish_mount(server, path.as_ptr()) }.is_null());
    assert_eq!(test_last_error_code(), TstError::Closed as i32);
    assert_eq!(
        unsafe { tst_rtsp_server_remove_mount(server, path.as_ptr()) },
        TstError::Closed as i32
    );

    drop(tcp);
    unsafe { tst_rtsp_server_free(server) };
}

/// `tst_rtsp_publish_mount_cancel` wakes a demux receiver parked on an idle
/// mount (no publisher) with CLOSED, the explicit-cancel outcome; the
/// mount itself stays registered.
#[test]
fn rtsp_publish_mount_cancel_wakes_parked_receiver() {
    let (server, _port) = start_server(false);
    let path = CString::new("/idle").unwrap();
    let m = unsafe { tst_rtsp_server_add_publish_mount(server, path.as_ptr()) };
    assert!(!m.is_null());
    let rx = unsafe { tst_rtsp_publish_mount_into_demux_receiver(m, std::ptr::null()) };
    assert!(!rx.is_null());

    // Park the receiver on another thread, then cancel through the mount.
    let rx_addr = rx as usize;
    let (tx, done) = std::sync::mpsc::channel();
    let parked = std::thread::spawn(move || {
        let mut ev = TstEvent::default();
        let rc = unsafe { tst_rtp_demux_receiver_next_event(rx_addr as *mut _, &mut ev) };
        tx.send(rc).unwrap();
    });
    assert_eq!(unsafe { tst_rtsp_publish_mount_cancel(m) }, 0);
    let rc = match done.recv_timeout(Duration::from_secs(10)) {
        Ok(rc) => rc,
        Err(_) => {
            // Last resort so a regression fails instead of hanging.
            unsafe { tst_rtp_demux_receiver_cancel(rx) };
            panic!("tst_rtsp_publish_mount_cancel did not wake the parked receiver");
        }
    };
    parked.join().unwrap();
    assert_eq!(rc, TstError::Closed as i32);

    // The mount is still registered: removing it succeeds once.
    assert_eq!(
        unsafe { tst_rtsp_server_remove_mount(server, path.as_ptr()) },
        0
    );

    unsafe { tst_rtp_demux_receiver_close(rx) };
    unsafe { tst_rtsp_publish_mount_free(m) };
    unsafe { tst_rtsp_server_free(server) };
}

/// NULL arguments are `TST_E_INVALID_CONFIG` everywhere, and the free
/// functions accept NULL.
#[test]
fn rtsp_publish_null_arguments() {
    let inv = TstError::InvalidConfig as i32;
    let null_m: *mut tstrans::TstRtspPublishMount = std::ptr::null_mut();
    let null_s: *mut tstrans::TstRtspServer = std::ptr::null_mut();
    let path = CString::new("/x").unwrap();
    let mut out: *mut tstrans::TstRtspPublishMount = std::ptr::null_mut();
    let mut v = 0u64;
    let mut st = TstRtspPublishMountStats::default();
    let mut info = TstRtspPublisherInfo::default();
    unsafe {
        assert!(tst_rtsp_server_add_publish_mount(null_s, path.as_ptr()).is_null());
        assert_eq!(test_last_error_code(), inv);
        assert_eq!(tst_rtsp_server_next_publisher(null_s, 0, &mut out), inv);
        assert_eq!(tst_rtsp_server_remove_mount(null_s, path.as_ptr()), inv);
        assert_eq!(tst_rtsp_server_active_publishers(null_s, &mut v), inv);
        assert_eq!(
            tst_rtsp_server_total_rtp_packets_received(null_s, &mut v),
            inv
        );
        assert_eq!(
            tst_rtsp_server_total_rtp_bytes_received(null_s, &mut v),
            inv
        );
        assert!(tst_rtsp_publish_mount_path(null_m).is_null());
        assert_eq!(test_last_error_code(), inv);
        assert_eq!(tst_rtsp_publish_mount_peer_count(null_m, &mut v), inv);
        assert_eq!(tst_rtsp_publish_mount_generation(null_m, &mut v), inv);
        assert_eq!(tst_rtsp_publish_mount_get_stats(null_m, &mut st), inv);
        assert_eq!(
            tst_rtsp_publish_mount_publisher_info(null_m, &mut info),
            inv
        );
        assert_eq!(tst_rtsp_publish_mount_cancel(null_m), inv);
        assert!(tst_rtsp_publish_mount_into_demux_receiver(null_m, std::ptr::null()).is_null());
        assert_eq!(test_last_error_code(), inv);
        tst_rtsp_publish_mount_free(null_m);
        tst_rtsp_server_builder_accept_unregistered_publishers(std::ptr::null_mut(), true);
        assert_eq!(test_last_error_code(), inv);
    }

    // Live server, NULL path / out pointers.
    let (server, _port) = start_server(false);
    unsafe {
        assert!(tst_rtsp_server_add_publish_mount(server, std::ptr::null()).is_null());
        assert_eq!(test_last_error_code(), inv);
        assert_eq!(
            tst_rtsp_server_next_publisher(server, 0, std::ptr::null_mut()),
            inv
        );
        assert_eq!(tst_rtsp_server_remove_mount(server, std::ptr::null()), inv);
        assert_eq!(
            tst_rtsp_server_active_publishers(server, std::ptr::null_mut()),
            inv
        );
        let m = tst_rtsp_server_add_publish_mount(server, path.as_ptr());
        assert!(!m.is_null());
        assert_eq!(
            tst_rtsp_publish_mount_get_stats(m, std::ptr::null_mut()),
            inv
        );
        assert_eq!(
            tst_rtsp_publish_mount_publisher_info(m, std::ptr::null_mut()),
            inv
        );
        assert_eq!(
            tst_rtsp_publish_mount_peer_count(m, std::ptr::null_mut()),
            inv
        );
        tst_rtsp_publish_mount_free(m);
        tst_rtsp_server_free(server);
    }
}

/// `tst_rtsp_server_local_addr` reports the kernel-picked port of a `:0`
/// bind, and a publisher can connect to it; NULL / zero-length arguments are
/// `TST_E_INVALID_CONFIG` and a stopped server is `TST_E_CLOSED`.
#[test]
fn rtsp_publish_server_local_addr() {
    let url = CString::new("rtsp://127.0.0.1:0").unwrap();
    let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
    assert!(!b.is_null(), "builder_new failed");
    let server = unsafe { tst_rtsp_server_builder_start(b) };
    assert!(!server.is_null(), "start on :0 failed");

    let mut buf = [0 as std::os::raw::c_char; 64];
    let rc = unsafe { tst_rtsp_server_local_addr(server, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(rc, 0, "local_addr: {rc}");
    let addr = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!addr.is_empty());
    let port: u16 = addr
        .strip_prefix("127.0.0.1:")
        .unwrap_or_else(|| panic!("unexpected address {addr:?}"))
        .parse()
        .expect("numeric port");
    assert_ne!(port, 0);

    // The reported port is the live listener: a publisher reaches it.
    let path = CString::new("/addr").unwrap();
    let m = unsafe { tst_rtsp_server_add_publish_mount(server, path.as_ptr()) };
    assert!(!m.is_null());
    let (tcp, _ch) = publish_mp2t(port, "/addr");

    let inv = TstError::InvalidConfig as i32;
    unsafe {
        assert_eq!(
            tst_rtsp_server_local_addr(std::ptr::null(), buf.as_mut_ptr(), buf.len()),
            inv
        );
        assert_eq!(
            tst_rtsp_server_local_addr(server, std::ptr::null_mut(), 64),
            inv
        );
        assert_eq!(tst_rtsp_server_local_addr(server, buf.as_mut_ptr(), 0), inv);
    }

    drop(tcp);
    assert_eq!(unsafe { tst_rtsp_server_stop(server, 0) }, 0);
    assert_eq!(
        unsafe { tst_rtsp_server_local_addr(server, buf.as_mut_ptr(), buf.len()) },
        TstError::Closed as i32
    );
    unsafe { tst_rtsp_publish_mount_free(m) };
    unsafe { tst_rtsp_server_free(server) };
}
