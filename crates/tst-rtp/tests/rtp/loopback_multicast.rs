//! Loopback multicast on `239.x.x.x` over the loopback interface.
//!
//! Some CI environments don't have multicast routing enabled by default
//! on `lo` (macOS GHA runner is the historical offender). The test
//! gracefully degrades: if `join_multicast_v4` returns an error
//! (typically `EADDRINUSE` or `ENODEV`), it records a MULTICAST capability
//! skip and returns, unless the host sets `TST_REQUIRE_MULTICAST=1`.
//!
//! Runs on Windows too: `tst_core::net::udp_socket` sets IP_MULTICAST_IF
//! through socket2 (so the send-side `?iface=127.0.0.1` works there) and
//! sets IP_MULTICAST_LOOP on the receiver, where Windows applies it;
//! loopback multicast delivers on the GHA runner. The `try_listen` skip-on-error path still
//! degrades gracefully where multicast routing is genuinely absent.

use std::io;
use std::thread;
use std::time::Duration;

use tst_core::transport::{RecvTransport, Transport};
use tst_rtp::{RtpConnectError, RtpRecvTransport, RtpTransport, RtpUrl};
use tst_test_helpers::capability::skip_unless_required;

fn try_listen_multicast_v4(addr: &str) -> Option<RtpRecvTransport> {
    match RtpRecvTransport::listen(addr) {
        Ok(t) => Some(t),
        Err(e) => {
            skip_unless_required(
                "MULTICAST",
                format_args!(
                    "bind/join failed: {e:?}. Likely missing multicast routing \
                     on lo (common on macOS GHA runners)."
                ),
            );
            None
        }
    }
}

#[test]
fn ipv4_multicast_loopback_round_trip() {
    let Some(mut recv) = try_listen_multicast_v4("rtp://239.55.55.1:55010?iface=127.0.0.1") else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    // Capture the cancel handle before the transport moves into the thread —
    // recv_bytes blocks until data or cancellation, so the timeout-skip arm
    // below must cancel or the thread outlives the test (same pattern as
    // two_rtp_multicast_receivers_deliver_same_datagram).
    let cancel = recv.cancel_handle();
    let _t = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload() + 64];
        if let Ok(n) = recv.recv_bytes(&mut buf) {
            let _ = tx.send(buf[..n].to_vec());
        }
    });

    thread::sleep(Duration::from_millis(50));
    let mut send = RtpTransport::connect("rtp://239.55.55.1:55010?ttl=1&iface=127.0.0.1").unwrap();
    let payload = [0x47u8; 188];
    send.send_bytes(&payload).unwrap();

    let got = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(v) => v,
        Err(_) => {
            if let Some(c) = &cancel {
                c.cancel();
            }
            skip_unless_required("MULTICAST", "recv timed out — kernel filter on lo");
            return;
        }
    };
    assert_eq!(got.as_slice(), &payload[..]);
}

/// Two receivers joining the same RTP multicast
/// group on loopback must both bind AND both receive the same datagram.
///
/// Uses a fixed group `239.55.55.4:55012` (distinct from the round-trip test
/// at `239.55.55.1:55010`).
///
/// r1 failure: graceful skip (no loopback multicast routing on this platform).
/// r2 failure with EADDRINUSE: hard panic — means SO_REUSEADDR is not applied.
/// r2 failure with anything else: graceful skip.
#[test]
fn two_rtp_multicast_receivers_deliver_same_datagram() {
    const GROUP: &str = "rtp://239.55.55.4:55012?iface=127.0.0.1";
    let parsed = RtpUrl::parse(GROUP).expect("parse test URL");

    let mut recv1 = match RtpRecvTransport::listen_with(&parsed) {
        Ok(r) => r,
        Err(e) => {
            skip_unless_required(
                "MULTICAST",
                format_args!("r1 bind/join failed: {e:?}. Likely missing multicast routing on lo."),
            );
            return;
        }
    };

    let mut recv2 = match RtpRecvTransport::listen_with(&parsed) {
        Ok(r) => r,
        Err(RtpConnectError::Io(ref io_err)) if io_err.kind() == io::ErrorKind::AddrInUse => {
            panic!(
                "r2 got EADDRINUSE on the same group:port — SO_REUSEADDR fix \
                 is not applied to the RTP recv bind path: {io_err:?}"
            );
        }
        Err(e) => {
            skip_unless_required(
                "MULTICAST",
                format_args!(
                    "r2 bind/join failed: {e:?}. Platform may not honour \
                     SO_REUSEADDR for multicast."
                ),
            );
            return;
        }
    };

    let payload = [0x47u8; 188];
    let (tx1, rx1) = std::sync::mpsc::channel::<Vec<u8>>();
    let (tx2, rx2) = std::sync::mpsc::channel::<Vec<u8>>();

    // Capture cancel handles before the transports move into the threads —
    // recv_bytes loops until data or cancellation, so the timeout/skip paths
    // below must cancel or the threads outlive the test.
    let cancel1 = recv1.cancel_handle();
    let cancel2 = recv2.cancel_handle();
    let cancel_receivers = || {
        if let Some(c) = &cancel1 {
            c.cancel();
        }
        if let Some(c) = &cancel2 {
            c.cancel();
        }
    };

    let _t1 = thread::spawn(move || {
        let mut buf = vec![0u8; recv1.max_payload() + 64];
        if let Ok(n) = recv1.recv_bytes(&mut buf) {
            let _ = tx1.send(buf[..n].to_vec());
        }
    });
    let _t2 = thread::spawn(move || {
        let mut buf = vec![0u8; recv2.max_payload() + 64];
        if let Ok(n) = recv2.recv_bytes(&mut buf) {
            let _ = tx2.send(buf[..n].to_vec());
        }
    });

    // Brief pause so both threads reach recv_bytes before the datagram is sent.
    thread::sleep(Duration::from_millis(50));

    let mut send = RtpTransport::connect("rtp://239.55.55.4:55012?ttl=1&iface=127.0.0.1").unwrap();
    send.send_bytes(&payload).unwrap();

    let got1 = match rx1.recv_timeout(Duration::from_secs(5)) {
        Ok(v) => v,
        Err(_) => {
            cancel_receivers();
            skip_unless_required(
                "MULTICAST",
                "recv1 timed out — kernel filtered loopback multicast",
            );
            return;
        }
    };
    let got2 = match rx2.recv_timeout(Duration::from_secs(5)) {
        Ok(v) => v,
        Err(_) => {
            cancel_receivers();
            skip_unless_required(
                "MULTICAST",
                "recv2 timed out — kernel filtered loopback multicast",
            );
            return;
        }
    };

    assert_eq!(
        got1.len(),
        payload.len(),
        "recv1: expected {} bytes, got {}",
        payload.len(),
        got1.len()
    );
    assert_eq!(
        got2.len(),
        payload.len(),
        "recv2: expected {} bytes, got {}",
        payload.len(),
        got2.len()
    );
    assert_eq!(&got1[..], &payload[..], "recv1 payload mismatch");
    assert_eq!(&got2[..], &payload[..], "recv2 payload mismatch");
}
