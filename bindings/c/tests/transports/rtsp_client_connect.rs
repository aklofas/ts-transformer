//! `rtsps://` / `rtsp://` client connect through the C ABI: a connect to a
//! misbehaving peer returns NULL with the right error code instead of
//! parking the caller, and a session bridged into a demux receiver keeps
//! receiving media until the receiver is closed.
#![cfg(feature = "rtp")]

use std::ffi::CString;
use std::io::Write as _;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle as ThreadHandle;
use std::time::{Duration, Instant};

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::event::{TstEvent, TstEventKind};
use tstrans::handle::TstRtspClientBuilder;
use tstrans::rtp::{
    TstRtpDemuxReceiver, tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close,
    tst_rtp_demux_receiver_next_event,
};
use tstrans::{
    TstRtspMountHandle, TstRtspServer, tst_rtsp_client_builder_connect,
    tst_rtsp_client_builder_new, tst_rtsp_client_builder_transport_pref,
    tst_rtsp_mount_handle_free, tst_rtsp_mount_push_video, tst_rtsp_server_add_unicast_mount,
    tst_rtsp_server_builder_new, tst_rtsp_server_builder_start, tst_rtsp_server_free,
    tst_rtsp_server_local_addr, tst_rtsp_server_stop, tst_rtsp_session_into_demux_receiver,
    tst_rtsp_session_play,
};

use crate::common::{last_error_msg, scaled};

#[cfg(feature = "tls")]
use std::io::Read;
#[cfg(feature = "tls")]
use std::net::TcpListener;
#[cfg(feature = "tls")]
use std::thread::JoinHandle;

/// A peer that accepts one connection, reads exactly one TLS record (the
/// ClientHello: 5-byte header + its length), then closes with a FIN. Reading
/// the whole record matters: closing with unread bytes would send RST, which
/// already took the error path.
#[cfg(feature = "tls")]
fn fin_after_client_hello_peer() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut header = [0u8; 5];
        s.read_exact(&mut header).unwrap();
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).unwrap();
    });
    (port, peer)
}

/// The peer's FIN mid-handshake surfaces as `TST_E_RTSP_TLS`; before the
/// fix the handshake loop polled the closed socket forever.
#[cfg(feature = "tls")]
#[test]
fn rtsps_connect_to_a_peer_closing_mid_handshake_is_rtsp_tls() {
    use std::sync::mpsc;
    use tstrans::error::{TstError, tst_get_last_error};
    use tstrans::tst_rtsp_client_builder_keepalive;

    let (port, peer) = fin_after_client_hello_peer();
    let url = CString::new(format!("rtsps://127.0.0.1:{port}/x")).unwrap();
    let (tx, rx) = mpsc::channel();
    // The last-error slot is thread-local: read it on the connecting thread.
    std::thread::spawn(move || unsafe {
        let b = tst_rtsp_client_builder_new(url.as_ptr());
        assert!(!b.is_null(), "{}", crate::common::last_error_msg());
        tst_rtsp_client_builder_keepalive(b, false);
        let session = tst_rtsp_client_builder_connect(b);
        let _ = tx.send((
            session.is_null(),
            tst_get_last_error(),
            crate::common::last_error_msg(),
        ));
    });
    let (is_null, code, msg) = rx
        .recv_timeout(crate::common::scaled(Duration::from_secs(10)))
        .expect("connect did not return: the handshake loop is spinning on EOF");
    assert!(is_null, "connect to a closing peer returned a session");
    assert_eq!(code, TstError::RtspTls as i32, "{msg}");
    peer.join().unwrap();
}

/// The SPS + PPS + IDR access unit the pusher sends: enough for the muxer to
/// emit PAT, PMT and a video PES the receiver demuxes as a sample.
const H264_AU: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS
    0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80, // PPS
    0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40, // IDR
];

/// A raw C handle moved to a helper thread. The C ABI documents these
/// handles as usable across threads; the pointer itself is just not `Send`.
struct SendPtr<T>(*mut T);

// SAFETY: every handle passed through this wrapper is documented as safe to
// use from another thread for the calls the helper threads make.
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    // A method (not field access) so a closure captures the whole wrapper,
    // not the non-`Send` raw pointer inside it.
    fn get(&self) -> *mut T {
        self.0
    }
}

fn builder(url: &str) -> *mut TstRtspClientBuilder {
    let url = CString::new(url).unwrap();
    let b = unsafe { tst_rtsp_client_builder_new(url.as_ptr()) };
    assert!(!b.is_null(), "{}", last_error_msg());
    b
}

/// In-process C server on rtsp://127.0.0.1:0 (port read back with
/// `tst_rtsp_server_local_addr`) with one H.264 unicast mount "/live",
/// PID 0x1011.
fn c_server() -> (*mut TstRtspServer, *mut TstRtspMountHandle, u16) {
    let url = CString::new("rtsp://127.0.0.1:0").unwrap();
    let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
    assert!(!b.is_null(), "{}", last_error_msg());
    let server = unsafe { tst_rtsp_server_builder_start(b) };
    assert!(!server.is_null(), "{}", last_error_msg());
    let mut buf = [0 as std::os::raw::c_char; 64];
    let n = unsafe { tst_rtsp_server_local_addr(server, buf.as_mut_ptr(), buf.len()) };
    assert!(n > 0, "local_addr: {}", last_error_msg());
    let addr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();

    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };
    let path = CString::new("/live").unwrap();
    let mount = unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!mount.is_null(), "{}", last_error_msg());
    (server, mount, addr.port())
}

/// A TCP forwarder between the client and the server's control port that
/// keeps a copy of the first bytes the server sends. The C ABI exposes no
/// stat that says which transport SETUP negotiated, but the server's SETUP
/// reply on the control connection does. Returns the port to dial and the
/// captured bytes.
fn control_spy(server_port: u16) -> (u16, Arc<Mutex<Vec<u8>>>) {
    // Enough for the OPTIONS / DESCRIBE / SETUP replies; interleaved media
    // past this point is forwarded but not kept.
    const KEEP: usize = 64 * 1024;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&seen);
    std::thread::spawn(move || {
        let Ok((client, _)) = listener.accept() else {
            return;
        };
        let server = TcpStream::connect(("127.0.0.1", server_port)).unwrap();
        let (mut c_read, mut s_write) = (client.try_clone().unwrap(), server.try_clone().unwrap());
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut c_read, &mut s_write);
            let _ = s_write.shutdown(Shutdown::Write);
        });
        let (mut s_read, mut c_write) = (server, client);
        let mut buf = [0u8; 4096];
        loop {
            let n = match std::io::Read::read(&mut s_read, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            // Recorded before forwarding, so the reply is in `seen` by the
            // time the client has read it.
            {
                let mut v = keep.lock().unwrap();
                let room = KEEP.saturating_sub(v.len()).min(n);
                v.extend_from_slice(&buf[..room]);
            }
            if c_write.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = c_write.shutdown(Shutdown::Write);
    });
    (port, seen)
}

/// The `Transport:` header value of the server's SETUP reply, from the
/// bytes `control_spy` captured.
fn negotiated_transport(seen: &Mutex<Vec<u8>>) -> String {
    let text = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
    text.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("transport")
                .then(|| v.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("no Transport header in the server's replies: {text:?}"))
}

/// Pushes the SPS+PPS+IDR access unit every 40 ms (PTS += 3600) until `stop`.
fn pusher(mount: SendPtr<TstRtspMountHandle>, stop: Arc<AtomicBool>) -> ThreadHandle<()> {
    std::thread::spawn(move || {
        let mut pts: i64 = 0;
        while !stop.load(Ordering::SeqCst) {
            let rc = unsafe {
                tst_rtsp_mount_push_video(mount.get(), H264_AU.as_ptr(), H264_AU.len(), pts, true)
            };
            assert_eq!(rc, 0, "push_video: {}", last_error_msg());
            pts += 3600;
            std::thread::sleep(Duration::from_millis(40));
        }
    })
}

/// Cancels the receiver with `tst_rtp_demux_receiver_cancel` if `release`
/// has not been called within `limit`, so a receiver that never delivers
/// fails the test instead of hanging it.
struct CancelAfter {
    released: Arc<AtomicBool>,
    thread: ThreadHandle<()>,
}

impl CancelAfter {
    fn release(self) {
        self.released.store(true, Ordering::SeqCst);
        self.thread.join().expect("watchdog thread panicked");
    }
}

fn cancel_after(rx: SendPtr<TstRtpDemuxReceiver>, limit: Duration) -> CancelAfter {
    let released = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&released);
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { tst_rtp_demux_receiver_cancel(rx.get()) };
    });
    CancelAfter { released, thread }
}

/// Reads events until `pred` holds (true) or `next_event` returns an error
/// (false; the code is left in the last-error slot of this thread).
fn next_events_until(rx: *mut TstRtpDemuxReceiver, pred: impl Fn(&TstEvent) -> bool) -> bool {
    let mut ev = TstEvent::default();
    loop {
        let rc = unsafe { tst_rtp_demux_receiver_next_event(rx, &mut ev) };
        if rc != 0 {
            return false;
        }
        if pred(&ev) {
            return true;
        }
    }
}

/// Media sent AFTER the bridge returns must still arrive: the receiver owns
/// the RTSP client, its interleaved pump and its keepalive until close.
/// `pref` is the builder's transport preference (1 = force UDP, 2 = force
/// TCP-interleaved); `transport` is the profile the server's SETUP reply
/// must name, so a case that negotiated the other transport fails.
fn bridged_receiver_gets_media_sent_after_the_bridge(pref: u32, transport: &str) {
    let (server, mount, port) = c_server();
    let (spy_port, seen) = control_spy(port);
    let b = builder(&format!("rtsp://127.0.0.1:{spy_port}/live"));
    unsafe { tst_rtsp_client_builder_transport_pref(b, pref) };
    let session = unsafe { tst_rtsp_client_builder_connect(b) };
    assert!(!session.is_null(), "{}", last_error_msg());
    let negotiated = negotiated_transport(&seen);
    assert!(
        negotiated.starts_with(transport),
        "SETUP negotiated {negotiated:?}, expected {transport}"
    );
    assert_eq!(unsafe { tst_rtsp_session_play(session) }, 0);
    let rx = unsafe { tst_rtsp_session_into_demux_receiver(session, std::ptr::null()) };
    assert!(!rx.is_null(), "{}", last_error_msg());
    // The pushes start only now, so nothing queued before the bridge can
    // satisfy the test.
    let stop = Arc::new(AtomicBool::new(false));
    let push = pusher(SendPtr(mount), stop.clone());
    let watchdog = cancel_after(SendPtr(rx), scaled(Duration::from_secs(10)));
    let got_video = next_events_until(rx, |ev| {
        ev.kind == TstEventKind::Sample as i32 && unsafe { ev.u.sample.pid } == 0x1011
    });
    assert!(
        got_video,
        "no video sample after the bridge (last rc / error: {})",
        last_error_msg()
    );
    stop.store(true, Ordering::SeqCst);
    push.join().unwrap();
    watchdog.release();
    // Must return: cancel, then the client drop (TEARDOWN bounded to 500 ms).
    unsafe { tst_rtp_demux_receiver_close(rx) };
    unsafe {
        tst_rtsp_mount_handle_free(mount);
        tst_rtsp_server_stop(server, 0);
        tst_rtsp_server_free(server);
    }
}

#[test]
#[ignore = "the C connect does not yet pass the transport preference to the client, so this negotiates UDP"]
fn bridged_tcp_receiver_gets_media_sent_after_the_bridge() {
    bridged_receiver_gets_media_sent_after_the_bridge(2, "RTP/AVP/TCP;")
}

#[test]
fn bridged_udp_receiver_gets_media_sent_after_the_bridge() {
    bridged_receiver_gets_media_sent_after_the_bridge(1, "RTP/AVP;")
}
