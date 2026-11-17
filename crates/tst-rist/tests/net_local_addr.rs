//! `RistRecvTransport::local_addr`: the address librist bound, read from its
//! socket, so a receiver bound on port 0 can be reached; and the Simple
//! profile's refusal of port 0, where librist reads the kernel's port in
//! network byte order and derives the even-port check and the RTCP port
//! (port + 1) from that wrong value.

#[path = "support/even_port.rs"]
mod even_port;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_rist::{RistError, RistProfile, RistRecvTransport, RistRecvTransportBuilder};

/// Some CI environments disable IPv6 loopback; same probe as `net_loopback.rs`.
fn ipv6_loopback_available() -> bool {
    UdpSocket::bind("[::1]:0").is_ok()
}

/// Seven TS packets in one datagram, the usual RIST payload.
fn seven_ts_packets() -> Vec<u8> {
    let mut out = vec![0xffu8; 188 * 7];
    for (i, pkt) in out.chunks_mut(188).enumerate() {
        pkt[0] = 0x47;
        pkt[1] = 0x1f;
        pkt[3] = i as u8;
    }
    out
}

/// Send to `rx`'s reported port until one read returns the datagram, or
/// `deadline` passes.
fn assert_reachable(rx: &mut RistRecvTransport, query: &str, deadline: Instant) {
    let local = rx.local_addr();
    let host = if local.is_ipv6() {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    let mut tx =
        tst_rist::RistTransport::connect(&format!("rist://{host}:{}{query}", local.port()))
            .expect("connect");
    let payload = seven_ts_packets();
    let mut buf = vec![0u8; rx.max_payload()];
    while Instant::now() < deadline {
        tx.send_bytes(&payload).expect("send");
        match rx.recv_bytes(&mut buf) {
            Ok(n) => {
                assert_eq!(&buf[..n], &payload[..], "{local}");
                return;
            }
            Err(TransportError::Backpressure { .. }) => continue,
            Err(e) => panic!("{local}: recv: {e:?}"),
        }
    }
    panic!("{local}: nothing arrived before the test's deadline");
}

#[test]
fn main_profile_port_zero_reports_the_port_a_sender_reaches() {
    // One deadline for every bind, inside nextest's 20 s kill for network tests.
    let deadline = Instant::now() + Duration::from_secs(15);
    for bind in [
        "rist://@127.0.0.1:0",
        "rist://@0.0.0.0:0",
        "rist://@[::1]:0",
        "rist://@127.0.0.1:0?secret=0123456789abcdef",
    ] {
        if bind.contains("[::1]") && (cfg!(windows) || !ipv6_loopback_available()) {
            eprintln!("skipping {bind}: no IPv6 receiver bind on this host");
            continue;
        }
        if bind.contains("secret=") && !cfg!(feature = "mbedtls") {
            eprintln!("skipping {bind}: built without encryption");
            continue;
        }
        let mut rx = RistRecvTransport::listen(bind).unwrap_or_else(|e| panic!("{bind}: {e:?}"));
        assert_ne!(rx.local_addr().port(), 0, "{bind}");
        let query = bind.find('?').map_or("", |i| &bind[i..]);
        assert_reachable(&mut rx, query, deadline);
    }
}

#[test]
fn a_fixed_port_reads_back() {
    for profile in [RistProfile::Main, RistProfile::Simple] {
        let mut attempts = 0;
        let (rx, port) = loop {
            let port = even_port::free_even_port();
            match RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{port}"))
                .unwrap()
                .profile(profile)
                .listen()
            {
                Ok(rx) => break (rx, port),
                Err(RistError::PeerCreateFailed) if attempts < 10 => attempts += 1,
                Err(e) => panic!("{profile:?} on {port}: {e:?}"),
            }
        };
        assert_eq!(
            rx.local_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            "{profile:?}"
        );
    }
}

#[test]
fn simple_profile_port_zero_is_refused_before_librist() {
    let assert_refused = |what: &str, r: Result<RistRecvTransport, RistError>| match r {
        Err(RistError::InvalidConfig(m)) => {
            assert!(
                m.contains("Simple") && m.contains("even port") && m.contains("Main"),
                "{what}: {m}"
            );
        }
        Err(e) => panic!("{what}: {e:?}"),
        Ok(_) => panic!("{what}: opened, expected a refusal"),
    };
    // librist fails the peer about half the time and opens it otherwise,
    // so one open proves nothing: every one of 20 must be refused.
    for _ in 0..20 {
        for bind in [
            "rist://@127.0.0.1:0?profile=simple",
            "rist://@0.0.0.0:0?profile=simple",
        ] {
            assert_refused(bind, RistRecvTransport::listen(bind));
        }
        assert_refused(
            "builder",
            RistRecvTransportBuilder::new("rist://@127.0.0.1:0")
                .unwrap()
                .profile(RistProfile::Simple)
                .listen(),
        );
    }
}

#[test]
fn opener_reports_the_port_it_bound() {
    let rx = tst_rist::opener::open_recv("rist://@127.0.0.1:0").expect("open_recv");
    let local = rx.local_addr.expect("a bound receiver reports its address");
    assert_ne!(local.port(), 0);
    assert!(local.ip().is_loopback());
}
