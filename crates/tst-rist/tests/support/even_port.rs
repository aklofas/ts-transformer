//! An even port whose odd companion is free, for the Simple profile.

use std::net::UdpSocket;

/// An even port whose odd companion is free too (the Simple profile binds
/// both). Another process can take it before librist binds it; callers retry.
pub fn free_even_port() -> u16 {
    for _ in 0..100 {
        let probe = UdpSocket::bind("127.0.0.1:0").expect("bind probe");
        let port = probe.local_addr().expect("probe addr").port() & !1;
        drop(probe);
        if port == 0 {
            continue;
        }
        let pair = (
            UdpSocket::bind(("127.0.0.1", port)),
            UdpSocket::bind(("127.0.0.1", port + 1)),
        );
        if let (Ok(_), Ok(_)) = pair {
            return port;
        }
    }
    panic!("no free even port with a free odd neighbour in 100 tries");
}
