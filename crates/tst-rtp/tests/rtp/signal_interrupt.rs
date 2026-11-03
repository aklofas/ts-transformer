//! A handled signal that interrupts a parked RTP receive (EINTR) must not
//! end the transport. Unix only: Winsock has no EINTR.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::ports::free_rtp_port_base;
use crate::signal::{DELIVERED, install_handler};
use tst_core::transport::{RecvTransport, Transport};
use tst_rtp::{RtpRecvTransport, RtpTransport};

#[test]
fn recv_survives_a_handled_signal_and_delivers_the_next_datagram() {
    install_handler();
    let base = free_rtp_port_base();
    let url = format!("rtp://127.0.0.1:{base}");
    let mut recv = RtpRecvTransport::listen(&url).unwrap();
    let (tid_tx, tid_rx) = mpsc::channel::<usize>();
    let (out_tx, out_rx) = mpsc::channel();
    let entered = Arc::new(AtomicBool::new(false));
    let worker = {
        let entered = entered.clone();
        thread::spawn(move || {
            // SAFETY: pthread_self is always valid for the calling thread.
            tid_tx
                .send(unsafe { libc::pthread_self() } as usize)
                .unwrap();
            let mut buf = vec![0u8; recv.max_payload() + 64];
            entered.store(true, Ordering::SeqCst);
            let r = recv.recv_bytes(&mut buf);
            let alive = recv.is_alive();
            let _ = out_tx.send((r.map(|n| buf[..n].to_vec()), alive));
        })
    };
    let tid = tid_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    while !entered.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(150)); // inside the kernel wait, past the entry check
    for _ in 0..5 {
        // SAFETY: the target thread is alive (it is blocked in recv_bytes).
        assert_eq!(
            unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) },
            0
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        DELIVERED.load(Ordering::SeqCst) >= 1,
        "no SIGUSR1 reached the worker"
    );
    let mut send = RtpTransport::connect(&url).unwrap();
    let pkt = [0x47u8; 188];
    send.send_bytes(&pkt).unwrap();
    let (r, alive) = out_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker returned");
    worker.join().unwrap();
    assert_eq!(
        r.expect("the datagram after the signal is delivered"),
        pkt.to_vec()
    );
    assert!(alive, "a handled signal must not end the receiver");
}
