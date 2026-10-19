//! UDP port discovery for the `rtp` and `conformance` binaries, which do
//! not declare `fixtures`; each includes this file with `#[path]`.

use std::net::UdpSocket;

/// Find a free RTP port base on `127.0.0.1` where both `base` and `base + 1`
/// (the RTCP companion auto-bound on `port + 1` per RFC 3550 §11) are
/// currently bindable, then release them so the transport under test can bind
/// them itself.
///
/// This replaces hard-coded ports, which intermittently collided with the
/// Windows dynamic-exclusion / reserved UDP ranges (shift per runner boot)
/// and failed with `WSAEACCES (os error 10013)`. An OS-assigned ephemeral
/// port is never drawn from a reserved range. The transport binds by URL and
/// exposes no local-addr accessor, so this discovers then releases; the
/// serialized `network` nextest test-group means no sibling test races for
/// the freed port.
pub fn free_rtp_port_base() -> u16 {
    // Bounded so a host under unusual port pressure fails deterministically
    // with a clear message instead of spinning until nextest's timeout.
    for _ in 0..1000 {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let base = s.local_addr().unwrap().port();
        if base < u16::MAX
            && let Ok(companion) = UdpSocket::bind(("127.0.0.1", base + 1))
        {
            drop(companion);
            drop(s);
            return base;
        }
        drop(s); // base + 1 was taken (or base == u16::MAX); retry.
        std::thread::yield_now();
    }
    panic!(
        "free_rtp_port_base: no free base/base+1 UDP port pair on 127.0.0.1 \
         after 1000 attempts (host under port pressure?)"
    );
}
