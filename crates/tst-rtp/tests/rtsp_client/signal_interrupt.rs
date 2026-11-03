//! A handled signal (EINTR) that interrupts the RTSP client's blocking
//! reads must not end the stream or fail the request. Unix only: Winsock
//! has no EINTR.

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::RtspClient;

use crate::fixtures::raw_rtsp_server::{
    SDP_BODY, answer_describe_and_setup, cseq_of, method_of, read_request,
};
use crate::fixtures::rtp_packets::build_rtp_packet;

static DELIVERED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_sigusr1(_: libc::c_int) {
    DELIVERED.fetch_add(1, Ordering::SeqCst);
}

/// Install a SIGUSR1 handler WITHOUT `SA_RESTART`, so an interrupted
/// `read` returns `EINTR` on every Unix.
fn install_handler() {
    // SAFETY: a zeroed sigaction is valid; the handler touches one atomic.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigusr1 as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        assert_eq!(libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()), 0);
    }
}

#[test]
fn describe_survives_a_handled_signal() {
    install_handler();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "DESCRIBE", "unexpected request: {req}");
        // Answer only after the client is parked in its response read and
        // has been signalled several times.
        std::thread::sleep(Duration::from_millis(300));
        let cseq = cseq_of(&req);
        sock.write_all(
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
                SDP_BODY.len()
            )
            .as_bytes(),
        )
        .unwrap();
        sock.write_all(SDP_BODY).unwrap();
        // Keep the connection until the client is done.
        let _ = read_request_or_eof(&mut sock);
    });

    let (tid_tx, tid_rx) = mpsc::channel::<usize>();
    let (out_tx, out_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let url = format!("rtsp://127.0.0.1:{port}/test");
        let mut client = RtspClient::connect(&url).unwrap();
        // SAFETY: pthread_self is always valid for the calling thread.
        tid_tx
            .send(unsafe { libc::pthread_self() } as usize)
            .unwrap();
        let r = client.describe().map(|_| ());
        let _ = out_tx.send(r);
    });
    let tid = tid_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(40));
        // SAFETY: the worker is alive until describe() returns.
        unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) };
    }
    let r = out_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("describe returned");
    worker.join().unwrap();
    server.join().unwrap();
    assert!(DELIVERED.load(Ordering::SeqCst) >= 1, "no signal delivered");
    r.expect("DESCRIBE survives a handled signal");
}

fn read_request_or_eof(sock: &mut std::net::TcpStream) -> usize {
    use std::io::Read;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut b = [0u8; 256];
    sock.read(&mut b).unwrap_or(0)
}

/// Thread ids of this process whose name starts with `prefix` (the kernel
/// truncates thread names to 15 bytes).
#[cfg(target_os = "linux")]
fn find_threads(prefix: &str) -> Vec<libc::pid_t> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim().starts_with(prefix)
            && let Ok(tid) = entry.file_name().to_string_lossy().parse()
        {
            out.push(tid);
        }
    }
    out
}

#[cfg(target_os = "linux")]
#[test]
fn interleaved_recv_survives_a_handled_signal() {
    install_handler();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
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
        // The packet goes out only after the pump thread was signalled.
        go_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let pkt = build_rtp_packet(1, 0, 0x1234_5678, 33, false, &[0x47u8; 188]);
        let mut frame = vec![b'$', 0];
        frame.extend_from_slice(&(pkt.len() as u16).to_be_bytes());
        frame.extend_from_slice(&pkt);
        sock.write_all(&frame).unwrap();
        let _ = read_request_or_eof(&mut sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut rx = session.into_recv_transport();
    rx.set_recv_timeout(Some(Duration::from_secs(10)));
    client.play().unwrap();

    let before = DELIVERED.load(Ordering::SeqCst);
    let pumps = find_threads("rtsp-interleave");
    assert_eq!(pumps.len(), 1, "expected one pump thread, found {pumps:?}");
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(40));
        // SAFETY: the pump thread is alive while the client is.
        let rc =
            unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), pumps[0], libc::SIGUSR1) };
        assert_eq!(rc, 0);
    }
    assert!(
        DELIVERED.load(Ordering::SeqCst) > before,
        "no signal reached the pump thread"
    );
    go_tx.send(()).unwrap();

    let mut buf = vec![0u8; rx.max_payload() + 64];
    let n = rx
        .recv_bytes(&mut buf)
        .expect("the packet after the signal is delivered");
    assert!(n > 0);
    assert!(
        rx.end_reason().is_none(),
        "a handled signal must not end the stream, got {:?}",
        rx.end_reason()
    );
    drop(client);
    server.join().unwrap();
}
