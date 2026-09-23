//! Loopback multicast tests for UdpTransport + UdpRecvTransport.
//!
//! Uses the established try_listen pattern from tst-rtp — if the host
//! lacks multicast support (CI runners on certain providers), the test
//! prints a skip message and returns rather than failing.
//!
//! The IPv4 round-trip runs on Windows since 2026-05-29 (the send-side
//! `?iface=127.0.0.1` no longer errors now that `set_multicast_if_v4` has a
//! socket2 Windows path, and the receiver-side IP_MULTICAST_LOOP is set on
//! Windows; CI `diag_win_multicast` confirmed delivery). The IPv6 case stays
//! gated off Windows: `IPV6_MULTICAST_IF` takes an interface index there
//! (not yet wired) and IPv6 multicast loopback is unverified on the runner.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{RecvTransport, Transport};
use tst_udp::{UdpRecvTransport, UdpTransport};

fn try_listen_v4(url: &str) -> Option<UdpRecvTransport> {
    match UdpRecvTransport::listen(url) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipping IPv4 multicast test: failed to listen on {url}: {e}");
            None
        }
    }
}

#[cfg(not(target_os = "windows"))] // sole caller is windows-gated below
fn try_listen_v6(url: &str) -> Option<UdpRecvTransport> {
    match UdpRecvTransport::listen(url) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipping IPv6 multicast test: failed to listen on {url}: {e}");
            None
        }
    }
}

#[test]
fn ipv4_multicast_loopback_round_trip() {
    let Some(mut recv) = try_listen_v4("udp://@239.55.55.2:55020?iface=127.0.0.1") else {
        return;
    };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _t = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload() + 64];
        // Bounded wait instead of a blocking recv_bytes: this test never
        // obtains the transport's cancel handle (it hands the receiver to
        // the worker whole), so on the skip/timeout paths below a blocking
        // recv would park this thread past the end of the test.
        // The deadline comfortably exceeds the 5 s observation window.
        if let Ok(Some(n)) = recv.recv_timeout(&mut buf, Duration::from_secs(6)) {
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    thread::sleep(Duration::from_millis(50));

    let mut send = UdpTransport::connect("udp://239.55.55.2:55020?ttl=1&iface=127.0.0.1").unwrap();
    let payload = [0x47u8; 188];
    send.send_bytes(&payload).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    assert_eq!(got.as_slice(), &payload[..]);
}

#[test]
#[cfg(not(target_os = "windows"))] // IPv6 multicast loopback flaky on Windows GHA runners
fn ipv6_multicast_loopback_round_trip() {
    let Some(mut recv) = try_listen_v6("udp://@[ff02::1]:55021?iface=lo") else {
        return;
    };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _t = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload() + 64];
        // Bounded wait instead of a blocking recv_bytes: this test never
        // obtains the transport's cancel handle (it hands the receiver to
        // the worker whole), so on the skip/timeout paths below a blocking
        // recv would park this thread past the end of the test.
        // The deadline comfortably exceeds the 5 s observation window.
        if let Ok(Some(n)) = recv.recv_timeout(&mut buf, Duration::from_secs(6)) {
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    thread::sleep(Duration::from_millis(50));

    let mut send = match UdpTransport::connect("udp://[ff02::1]:55021?ttl=1&iface=lo") {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping IPv6 multicast test: failed to connect sender: {e}");
            return;
        }
    };
    let payload = [0x47u8; 188];
    send.send_bytes(&payload).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    assert_eq!(got.as_slice(), &payload[..]);
}
