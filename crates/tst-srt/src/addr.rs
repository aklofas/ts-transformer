//! `SocketAddr` ↔ platform-native sockaddr helpers.
//!
//! libsrt's bind / connect / getsockname / getpeername take
//! `*const struct sockaddr` + a `socklen_t`. We marshal between Rust's
//! `std::net::SocketAddr` and the C representation exclusively through
//! these helpers so callers never touch raw FFI.
//!
//! Both IPv4 and IPv6 are supported — `Socket::connect_with` and
//! `Listener::bind_with` walk every address resolved by
//! `to_socket_addrs`, so AAAA records that resolve before A records on
//! dual-stack hosts will be tried first, falling through to v4 if v6
//! isn't routable.
//!
//! Uses `os_socketaddr::OsSocketAddr` rather than `libc::sockaddr_*`
//! directly because the `libc` crate does NOT expose `sockaddr_in`,
//! `sockaddr_in6`, or `sockaddr_storage` on `*-pc-windows-msvc` (only
//! the base `libc::sockaddr` is exposed there). `OsSocketAddr` is a
//! `#[repr(C)] union { sa4: sockaddr_in, sa6: sockaddr_in6 }` that
//! holds a platform-native storage and converts to/from
//! `std::net::SocketAddr` with platform-correct family + length
//! handling.
//!
//! Casts of `OsSocketAddr::as_ptr()` to `*const srt_sys::sockaddr` are
//! ABI-sound: the bindgen-generated `srt_sys::sockaddr` matches
//! `libc::sockaddr` on Unix (per srt-sys/build.rs substitution) and
//! the Win32 `SOCKADDR` on Windows (per bindgen's auto-generation
//! against `<ws2def.h>`). Both follow the BSD socket-API memory
//! layout, so the cast is byte-equivalent.

use crate::error::AddrError;
use os_socketaddr::OsSocketAddr;
use std::net::SocketAddr;

/// Convert a Rust `SocketAddr` to an `OsSocketAddr` suitable for
/// passing through to libsrt. `OsSocketAddr::as_ptr()` yields the
/// `*const sockaddr` and `OsSocketAddr::len()` the `socklen_t` the
/// FFI call needs.
pub(crate) fn to_sockaddr(addr: SocketAddr) -> OsSocketAddr {
    OsSocketAddr::from(addr)
}

/// Convert an `OsSocketAddr` (typically populated by
/// `srt_getpeername` / `srt_getsockname` / `srt_accept`) back to
/// `std::net::SocketAddr`. Returns `AddrError::Resolve` for non-IP
/// address families (libsrt should never write one of those, but the
/// error path is here for completeness).
pub(crate) fn from_sockaddr(os_addr: &OsSocketAddr) -> Result<SocketAddr, AddrError> {
    (*os_addr)
        .into_addr()
        .ok_or_else(|| AddrError::Resolve("non-IP address family returned from libsrt".to_string()))
}

/// Join `host` and `port` into the `host:port` form `ToSocketAddrs`
/// parses, bracketing a bare IPv6 literal (`::1` → `[::1]:9000`).
///
/// [`SrtUrl::parse`](crate::SrtUrl::parse) hands the host back with its
/// brackets stripped, so a plain `format!("{host}:{port}")` produced
/// `::1:9000` and every IPv6 `srt://` open failed to resolve (PR #188).
/// One helper for every open path — the C, Python and JVM bindings each
/// carried a private copy before it lived here. An already-bracketed
/// host, an IPv4 literal and a hostname pass through unchanged.
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;
    use std::net::{SocketAddrV4, SocketAddrV6};

    #[test]
    fn round_trip_v4() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let os_addr = to_sockaddr(addr);
        // sockaddr_in is 16 bytes on every platform (Unix + Win32).
        // os_socketaddr's len() returns the IPv4 portion length when
        // populated from an IPv4 SocketAddr.
        assert_eq!(os_addr.len() as usize, 16);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn round_trip_zero_port() {
        let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let os_addr = to_sockaddr(addr);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn round_trip_v6_loopback() {
        let addr: SocketAddr = "[::1]:12345".parse().unwrap();
        let os_addr = to_sockaddr(addr);
        // sockaddr_in6 is 28 bytes on every platform.
        assert_eq!(os_addr.len() as usize, 28);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn round_trip_v6_with_scope_id() {
        // SocketAddrV6 carries flowinfo + scope_id. Verify both
        // round-trip through the OsSocketAddr marshalling.
        let v6 = SocketAddrV6::new(
            std::net::Ipv6Addr::LOCALHOST,
            12345,
            /*flowinfo=*/ 0,
            /*scope_id=*/ 7,
        );
        let addr = SocketAddr::V6(v6);
        let os_addr = to_sockaddr(addr);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn round_trip_v6_full_address() {
        let addr: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let os_addr = to_sockaddr(addr);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn round_trip_v4_via_v4_constructor() {
        // SocketAddrV4 construction path (not just parse).
        let v4 = SocketAddrV4::new(std::net::Ipv4Addr::new(192, 168, 1, 1), 8080);
        let addr = SocketAddr::V4(v4);
        let os_addr = to_sockaddr(addr);
        let back = from_sockaddr(&os_addr).unwrap();
        assert_eq!(addr, back);
    }

    #[test]
    fn os_sockaddr_layout_matches_sockaddr_in_on_v4() {
        // Smoke check: the FFI cast Socket::connect / Listener::bind use
        // assumes OsSocketAddr's storage matches the platform's
        // sockaddr layout. Verify the byte length agrees with libc's
        // sockaddr_in on Unix — on Windows libc::sockaddr_in doesn't
        // exist so we skip the comparison.
        let _ = mem::size_of::<()>();
        #[cfg(unix)]
        {
            let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
            let os_addr = to_sockaddr(addr);
            assert_eq!(
                os_addr.len() as usize,
                mem::size_of::<libc::sockaddr_in>(),
                "OsSocketAddr v4 len must match libc::sockaddr_in"
            );
        }
    }

    #[test]
    fn join_host_port_brackets_bare_ipv6_only() {
        assert_eq!(join_host_port("::1", 9000), "[::1]:9000");
        assert_eq!(join_host_port("fe80::1%eth0", 7000), "[fe80::1%eth0]:7000");
        assert_eq!(join_host_port("[::1]", 9000), "[::1]:9000");
        assert_eq!(join_host_port("127.0.0.1", 9000), "127.0.0.1:9000");
        assert_eq!(join_host_port("0.0.0.0", 7000), "0.0.0.0:7000");
        assert_eq!(join_host_port("camera.local", 9000), "camera.local:9000");
    }
}
