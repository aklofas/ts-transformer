//! Loopback multicast tests for UdpTransport + UdpRecvTransport.
//!
//! Uses the established try_listen pattern from tst-rtp — if the host
//! lacks multicast support (CI runners on certain providers), the test
//! records a MULTICAST capability skip and returns, unless the host sets
//! `TST_REQUIRE_MULTICAST=1`, which makes the skip a failure. The IPv6 case
//! records MULTICAST_V6 instead: the receiver rejects `?iface=` for IPv6
//! today, so it skips on every host and no CI leg requires it.
//!
//! The IPv4 round-trip runs on Windows too: the send-side
//! `?iface=127.0.0.1` goes through `set_multicast_if_v4`'s socket2 Windows
//! path, and the receiver-side IP_MULTICAST_LOOP is set on Windows. The IPv6 case stays
//! gated off Windows: `IPV6_MULTICAST_IF` takes an interface index there
//! (not yet wired) and IPv6 multicast loopback is unverified on the runner.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{RecvTransport, Transport};
use tst_test_helpers::capability::skip_unless_required;
use tst_udp::{UdpRecvTransport, UdpTransport};

fn try_listen_v4(url: &str) -> Option<UdpRecvTransport> {
    match UdpRecvTransport::listen(url) {
        Ok(t) => Some(t),
        Err(e) => {
            skip_unless_required(
                "MULTICAST",
                format_args!("IPv4 listen on {url} failed: {e}"),
            );
            None
        }
    }
}

#[cfg(not(target_os = "windows"))] // sole caller is windows-gated below
fn try_listen_v6(url: &str) -> Option<UdpRecvTransport> {
    match UdpRecvTransport::listen(url) {
        Ok(t) => Some(t),
        Err(e) => {
            skip_unless_required(
                "MULTICAST_V6",
                format_args!("IPv6 listen on {url} failed: {e}"),
            );
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
            skip_unless_required(
                "MULTICAST_V6",
                format_args!("IPv6 sender connect failed: {e}"),
            );
            return;
        }
    };
    let payload = [0x47u8; 188];
    send.send_bytes(&payload).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    assert_eq!(got.as_slice(), &payload[..]);
}

/// Reads `rx` until a datagram equal to `want` arrives or the deadline passes,
/// returning every datagram read in order.
#[cfg(not(target_os = "windows"))]
fn read_until(rx: &mut UdpRecvTransport, want: &[u8], deadline: Duration) -> Vec<Vec<u8>> {
    let end = std::time::Instant::now() + deadline;
    let mut buf = vec![0u8; rx.max_payload() + 64];
    let mut got = Vec::new();
    while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(&mut buf, left) {
            Ok(Some(n)) => {
                got.push(buf[..n].to_vec());
                if &buf[..n] == want {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => panic!("recv failed: {e}"),
        }
    }
    got
}

/// ip(7) IP_MULTICAST_ALL: a wildcard-bound member also receives other groups'
/// traffic and unicast to the port. A receiver must deliver only its group.
#[test]
#[cfg(not(target_os = "windows"))] // Windows binds the wildcard and cannot filter
fn multicast_receiver_ignores_other_groups_and_unicast_on_its_port() {
    let Some(mut rx_a) = try_listen_v4("udp://@239.255.42.1:0?iface=127.0.0.1") else {
        return;
    };
    let port = rx_a.local_addr().port();
    let Some(_rx_b) = try_listen_v4(&format!("udp://@239.255.42.2:{port}?iface=127.0.0.1")) else {
        return;
    };

    // Every datagram is queued before rx_a is read, so the receive side needs
    // no worker thread; loopback keeps the send order.
    UdpTransport::connect(&format!("udp://239.255.42.2:{port}?iface=127.0.0.1&ttl=0"))
        .unwrap()
        .send_bytes(b"B")
        .unwrap();
    UdpTransport::connect(&format!("udp://127.0.0.1:{port}"))
        .unwrap()
        .send_bytes(b"U")
        .unwrap();
    UdpTransport::connect(&format!("udp://239.255.42.1:{port}?iface=127.0.0.1&ttl=0"))
        .unwrap()
        .send_bytes(b"A")
        .unwrap();

    let got = read_until(
        &mut rx_a,
        b"A",
        tst_test_helpers::timing::scaled(Duration::from_secs(5)),
    );
    assert!(
        !got.is_empty() && got.iter().all(|d| d.as_slice() == b"A"),
        "rx_a must read only its group's datagram, got {got:?}"
    );
}

/// Two receivers on one group and port both get every datagram.
#[test]
#[cfg(not(target_os = "windows"))]
fn two_receivers_on_one_group_both_receive() {
    let Some(mut rx1) = try_listen_v4("udp://@239.255.42.3:0?iface=127.0.0.1") else {
        return;
    };
    let port = rx1.local_addr().port();
    let Some(mut rx2) = try_listen_v4(&format!("udp://@239.255.42.3:{port}?iface=127.0.0.1"))
    else {
        return;
    };

    UdpTransport::connect(&format!("udp://239.255.42.3:{port}?iface=127.0.0.1&ttl=0"))
        .unwrap()
        .send_bytes(b"X")
        .unwrap();

    let deadline = tst_test_helpers::timing::scaled(Duration::from_secs(5));
    assert_eq!(read_until(&mut rx1, b"X", deadline), vec![b"X".to_vec()]);
    assert_eq!(read_until(&mut rx2, b"X", deadline), vec![b"X".to_vec()]);
}
