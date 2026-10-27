//! UDP socket helpers shared by `tst-rtp` and `tst-udp`.
//!
//! Provides:
//! - `bind_udp_socket` — bind + apply cancel-poll timeouts
//! - `bind_udp_socket_multicast` — like `bind_udp_socket` but with `SO_REUSEADDR`
//!   so multiple receivers can bind the same group:port
//! - `bind_udp_socket_multicast_group` — the multicast receiver: binds the
//!   group, joins it, and drops other groups' traffic
//! - `set_socket_buffers` — `SO_RCVBUF`/`SO_SNDBUF` via `socket2::SockRef`,
//!   shared by `tst-udp` and `tst-tcp` to avoid duplication
//! - Multicast send knobs (TTL + iface, IPv4 + IPv6)
//! - Multicast recv group join (IPv4 + IPv6)
//!
//! All multicast iface/hop helpers that require raw `setsockopt` are
//! `cfg(unix)` — the non-Unix paths return
//! `io::Error::new(ErrorKind::Unsupported, …)`.

use crate::cancel::CANCEL_POLL_INTERVAL;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// Bind a UDP socket at `local` and apply the cancel-poll read/write timeouts.
pub fn bind_udp_socket(local: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(local)?;
    socket.set_read_timeout(Some(CANCEL_POLL_INTERVAL))?;
    socket.set_write_timeout(Some(CANCEL_POLL_INTERVAL))?;
    Ok(socket)
}

/// Bind a UDP socket for multicast receive, setting `SO_REUSEADDR` (and
/// `SO_REUSEPORT` on macOS/BSD) before bind so that multiple receivers on the
/// same host can both join the same `group:port`.
///
/// Uses `socket2` for the reuse options (not available on stable
/// std at the workspace MSRV). The resulting socket has the same cancel-poll read/write timeouts as
/// [`bind_udp_socket`].
pub fn bind_udp_socket_multicast(local: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    let domain = if local.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    // SO_REUSEPORT on macOS/BSD lets multiple sockets receive the same multicast
    // datagram; on Linux it is optional (SO_REUSEADDR is sufficient). Gate to
    // the platforms where SO_REUSEPORT has multicast semantics.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    sock.set_reuse_port(true)?;
    sock.bind(&SockAddr::from(local))?;
    // Convert to std UdpSocket and apply cancel-poll timeouts.
    let socket: UdpSocket = sock.into();
    socket.set_read_timeout(Some(CANCEL_POLL_INTERVAL))?;
    socket.set_write_timeout(Some(CANCEL_POLL_INTERVAL))?;
    Ok(socket)
}

/// Bind a UDP socket that receives one multicast group: the multicast-receive
/// helper every receiver uses.
///
/// The socket gets the reuse options of [`bind_udp_socket_multicast`], so
/// several receivers on one host can share `group:port`, and then joins
/// `group` through [`apply_multicast_recv_join`], so the `iface` rules and
/// errors are the same. An `iface` the join rejects returns that error and the
/// socket is dropped. `port` 0 binds an ephemeral port; read it back with
/// `local_addr()`.
///
/// On non-Windows targets the socket binds `group:port`, so the kernel drops
/// unicast datagrams to the port. On Linux it also clears `IP_MULTICAST_ALL`
/// (`IPV6_MULTICAST_ALL` for IPv6): per ip(7) a member socket otherwise
/// receives every group any socket on the host joined on that port.
///
/// Windows cannot bind a multicast address, so the socket binds the wildcard
/// of the group's family there; a Windows receiver also sees other groups'
/// traffic and unicast sent to the port.
pub fn bind_udp_socket_multicast_group(
    group: IpAddr,
    port: u16,
    iface: Option<&str>,
) -> io::Result<UdpSocket> {
    #[cfg(not(windows))]
    let local = SocketAddr::new(group, port);
    #[cfg(windows)]
    let local = match group {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port),
    };
    let socket = bind_udp_socket_multicast(local)?;
    #[cfg(target_os = "linux")]
    {
        let sock = socket2::SockRef::from(&socket);
        match group {
            IpAddr::V4(_) => sock.set_multicast_all_v4(false)?,
            IpAddr::V6(_) => sock.set_multicast_all_v6(false)?,
        }
    }
    apply_multicast_recv_join(&socket, group, iface)?;
    Ok(socket)
}

/// Set `SO_RCVBUF` and/or `SO_SNDBUF` on a socket via `socket2::SockRef`.
///
/// Shared by `tst-udp` and `tst-tcp` to avoid copying the same three-line
/// `SockRef` pattern across every transport's socket-setup function. Only the
/// `Some(n)` fields are applied; `None` leaves the OS default unchanged.
/// Each applied value is read back, and a `tracing` warning is logged when
/// the kernel granted less than requested (Linux clamps silently to
/// `net.core.rmem_max` / `net.core.wmem_max`).
///
/// # Platform trait bounds
///
/// `SockRef::from` requires `AsFd` on Unix and `AsSocket` on Windows (the
/// concrete `UdpSocket` / `TcpStream` types implement both). This function
/// is platform-gated accordingly.
#[cfg(unix)]
pub fn set_socket_buffers<S: std::os::fd::AsFd>(
    sock: &S,
    rcv: Option<usize>,
    snd: Option<usize>,
) -> io::Result<()> {
    apply_buffers(socket2::SockRef::from(sock), rcv, snd)
}

/// Windows version of [`set_socket_buffers`] — identical body, different trait
/// bound.
#[cfg(windows)]
pub fn set_socket_buffers<S: std::os::windows::io::AsSocket>(
    sock: &S,
    rcv: Option<usize>,
    snd: Option<usize>,
) -> io::Result<()> {
    apply_buffers(socket2::SockRef::from(sock), rcv, snd)
}

fn apply_buffers(
    sr: socket2::SockRef<'_>,
    rcv: Option<usize>,
    snd: Option<usize>,
) -> io::Result<()> {
    if let Some(n) = rcv {
        sr.set_recv_buffer_size(n)?;
        warn_if_clamped("SO_RCVBUF", "net.core.rmem_max", n, sr.recv_buffer_size()?);
    }
    if let Some(n) = snd {
        sr.set_send_buffer_size(n)?;
        warn_if_clamped("SO_SNDBUF", "net.core.wmem_max", n, sr.send_buffer_size()?);
    }
    Ok(())
}

/// Log a warning when the kernel granted less buffer than requested.
///
/// Linux stores twice the request (the extra half is bookkeeping overhead)
/// and silently caps it at the `*mem_max` sysctl, so an honoured request
/// reads back as `2 × requested`; anything below that was clamped. macOS
/// stores the value as given and refuses a request above
/// `kern.ipc.maxsockbuf` with `ENOBUFS` (surfaced as an error, not a
/// clamp); Windows stores the value as given. There the check is
/// `< requested`.
fn warn_if_clamped(opt: &str, sysctl: &str, requested: usize, effective: usize) {
    let expected = if cfg!(any(target_os = "linux", target_os = "android")) {
        requested.saturating_mul(2)
    } else {
        requested
    };
    if effective < expected {
        tracing::warn!(
            requested,
            effective,
            "{opt} clamped by the kernel: requested {requested} bytes, got {effective}; \
             on Linux raise {sysctl} (e.g. `sysctl -w {sysctl}=16777216`)"
        );
    }
}

/// Apply multicast SEND-side knobs (TTL + iface).
///
/// `ttl`: optional TTL for IPv4 (`IP_MULTICAST_TTL`) or hop limit for IPv6
///        (`IPV6_MULTICAST_HOPS`). `None` keeps the OS default.
/// `iface`: optional outgoing interface (literal IPv4 address; see
///          [`apply_multicast_iface`] for limitations).
///
/// IPv4 TTL uses stable `std::net::UdpSocket::set_multicast_ttl_v4`.
/// IPv6 hop limit and IPv4 `IP_MULTICAST_IF` are not exposed on stable
/// std at the workspace MSRV — we drop to `libc::setsockopt` on Unix and
/// surface `io::ErrorKind::Unsupported` on non-Unix platforms when an
/// IPv6 mcast hop or `iface=` knob is requested.
pub fn apply_multicast_send_knobs(
    socket: &UdpSocket,
    group: IpAddr,
    ttl: Option<u8>,
    iface: Option<&str>,
) -> io::Result<()> {
    let hops = ttl.unwrap_or(MCAST_DEFAULT_TTL);
    match group {
        IpAddr::V4(_) => socket.set_multicast_ttl_v4(hops as u32)?,
        IpAddr::V6(_) => set_multicast_hops_v6(socket, hops)?,
    }
    if let Some(iface_str) = iface {
        apply_multicast_iface(socket, group, iface_str)?;
    }
    Ok(())
}

/// Default multicast TTL for sends when `?ttl=` is absent — small but
/// non-1 so single-router LAN multicast works out of the box. Matches
/// the master spec's URL defaults table.
pub const MCAST_DEFAULT_TTL: u8 = 8;

/// Set `IPV6_MULTICAST_HOPS` via raw `setsockopt`. Stable std::net does
/// not expose this at the workspace MSRV (tracking issue rust-lang/rust#92517).
#[cfg(unix)]
pub fn set_multicast_hops_v6(socket: &UdpSocket, hops: u8) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let val: libc::c_int = hops as libc::c_int;
    // SAFETY: `socket.as_raw_fd()` returns an FD owned by `socket` for
    // its lifetime; `&val` is a valid pointer to a c_int sized to
    // `size_of::<c_int>()`. setsockopt with these args is documented in
    // ipv6(7).
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_MULTICAST_HOPS,
            &val as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-Unix fallback: report IPv6 multicast hop-limit knob as unsupported
/// rather than silently ignoring `?ttl=`.
#[cfg(not(unix))]
pub fn set_multicast_hops_v6(_socket: &UdpSocket, _hops: u8) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "IPV6_MULTICAST_HOPS via raw setsockopt is Unix-only",
    ))
}

/// Set `IP_MULTICAST_IF` for IPv4 (interface IP) or surface a limitation
/// for IPv6 (needs scope-id integer lookup, not yet wired).
///
/// IPv4 path accepts a literal IPv4 address string (e.g. `"192.168.1.50"`).
/// Name → IP resolution for interface names (e.g., `eth0`) is not done
/// — callers needing name-based binding can resolve via
/// `getifaddrs` and pass the IP. This is the same UX libsrt's
/// `?iface=` query parameter ships with.
pub fn apply_multicast_iface(socket: &UdpSocket, group: IpAddr, iface: &str) -> io::Result<()> {
    match group {
        IpAddr::V4(_) => {
            let v4: Ipv4Addr = iface.parse().map_err(|e: std::net::AddrParseError| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "IPv4 multicast iface requires literal IPv4 address, got '{iface}': {e}"
                    ),
                )
            })?;
            set_multicast_if_v4(socket, v4)?;
        }
        IpAddr::V6(_) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "IPv6 multicast iface name lookup is not implemented; \
                     pre-resolve to scope-id and use the URL form directly (iface='{iface}')"
                ),
            ));
        }
    }
    Ok(())
}

/// Set `IP_MULTICAST_IF` via raw `setsockopt`. Stable std::net does not
/// expose this at the workspace MSRV (tracking issue rust-lang/rust#92517).
#[cfg(unix)]
pub fn set_multicast_if_v4(socket: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // `IP_MULTICAST_IF` accepts a 4-byte in_addr (the IP of the local
    // interface to send out on). Pass the network-byte-order octets
    // directly — `Ipv4Addr::octets()` is already big-endian.
    let in_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(addr.octets()),
    };
    // SAFETY: `socket.as_raw_fd()` returns an FD owned by `socket` for
    // its lifetime; `&in_addr` is a valid pointer to a struct in_addr
    // sized to `size_of::<in_addr>()`. setsockopt with these args is
    // documented in ip(7).
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            &in_addr as *const libc::in_addr as *const libc::c_void,
            std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Windows `IP_MULTICAST_IF` via socket2's cross-platform setsockopt.
///
/// std::net exposes no `set_multicast_if_v4`; `socket2::SockRef` borrows the
/// underlying OS socket (no ownership transfer) and issues the same
/// `IP_MULTICAST_IF` setsockopt Winsock supports. CI confirmed Winsock accepts
/// both a real NIC IP and `127.0.0.1` here (`diag_win_multicast`).
#[cfg(windows)]
pub fn set_multicast_if_v4(socket: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
    socket2::SockRef::from(socket).set_multicast_if_v4(&addr)
}

/// Fallback for the rare non-Unix, non-Windows target: surface the iface knob
/// as unsupported rather than silently ignoring it.
#[cfg(not(any(unix, windows)))]
pub fn set_multicast_if_v4(_socket: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("IP_MULTICAST_IF via raw setsockopt is unsupported on this platform (addr={addr})"),
    ))
}

/// Join a multicast group on a bound recv socket.
///
/// For IPv4 uses `join_multicast_v4(group, interface)`; for IPv6 uses
/// `join_multicast_v6(group, interface_index)`.
///
/// `iface` parsing matches the send-side rules in [`apply_multicast_iface`]:
/// IPv4 takes a literal IPv4 address; IPv6 is not supported.
pub fn apply_multicast_recv_join(
    socket: &UdpSocket,
    group: IpAddr,
    iface: Option<&str>,
) -> io::Result<()> {
    match group {
        IpAddr::V4(v4_group) => {
            let iface_v4 = match iface {
                Some(iface_str) => iface_str.parse::<Ipv4Addr>().map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "IPv4 multicast iface requires literal IPv4 address, \
                                 got '{iface_str}': {e}"
                        ),
                    )
                })?,
                None => Ipv4Addr::UNSPECIFIED, // 0.0.0.0 — OS default
            };
            socket.join_multicast_v4(&v4_group, &iface_v4)?;
            // On Windows, IP_MULTICAST_LOOP is a RECEIVE-side option (the
            // opposite of BSD/Linux, where it gates the sender). Loopback
            // multicast delivery only happens when it is enabled on the
            // receiver — set it explicitly so loopback round-trips work
            // regardless of the OS default (CI `diag_win_multicast`:
            // delivery iff receiver loop is on). No-op on Unix where the
            // option lives on the sender, so it stays Windows-gated.
            #[cfg(windows)]
            socket.set_multicast_loop_v4(true)?;
        }
        IpAddr::V6(v6_group) => {
            let scope_id = match iface {
                None => 0, // Default scope.
                Some(iface_str) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "IPv6 multicast iface name lookup is not implemented; \
                             ?iface= must be omitted for ipv6 receive (iface='{iface_str}')"
                        ),
                    ));
                }
            };
            socket.join_multicast_v6(&v6_group, scope_id)?;
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod buffer_clamp_tests {
    use super::set_socket_buffers;
    use std::net::UdpSocket;

    fn rmem_max() -> usize {
        std::fs::read_to_string("/proc/sys/net/core/rmem_max")
            .expect("read net.core.rmem_max")
            .trim()
            .parse()
            .expect("numeric rmem_max")
    }

    /// A request above `net.core.rmem_max` is clamped silently by Linux; the
    /// helper reads the value back and logs a warning naming the sysctl.
    #[test]
    #[tracing_test::traced_test]
    fn clamped_rcvbuf_logs_a_warning() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        set_socket_buffers(&s, Some(rmem_max() * 4), None).unwrap();
        assert!(logs_contain("SO_RCVBUF"));
        assert!(logs_contain("net.core.rmem_max"));
    }

    /// A request the kernel honours logs nothing.
    #[test]
    #[tracing_test::traced_test]
    fn honoured_rcvbuf_logs_nothing() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        set_socket_buffers(&s, Some(64 * 1024), None).unwrap();
        assert!(!logs_contain("SO_RCVBUF"));
    }
}

#[cfg(all(test, not(windows)))]
mod multicast_group_tests {
    use super::*;
    use std::time::Duration;

    /// The helper joins: a bound-and-joined socket receives without a
    /// separate join call, and a rejected `iface` surfaces the join's error.
    #[test]
    fn group_helper_joins_and_honours_iface() {
        let group = IpAddr::V4(Ipv4Addr::new(239, 255, 42, 4));
        let rx = bind_udp_socket_multicast_group(group, 0, Some("127.0.0.1")).unwrap();
        let port = rx.local_addr().unwrap().port();
        rx.set_read_timeout(Some(tst_test_helpers::timing::scaled(Duration::from_secs(
            5,
        ))))
        .unwrap();

        let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        set_multicast_if_v4(&tx, Ipv4Addr::LOCALHOST).unwrap();
        tx.send_to(b"G", SocketAddr::new(group, port)).unwrap();
        let mut buf = [0u8; 16];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"G");

        let err = bind_udp_socket_multicast_group(group, 0, Some("not-an-ip")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
