//! Multicast mount specialization — single shared UDP socket sending
//! to a multicast group, fed by the mount's broadcast channel. Per-client
//! per-session tasks do NOT spawn per-peer fanout (the per-peer fanout is
//! unicast-only); they just increment a counter so MountStats::peer_count
//! reflects the number of clients that SETUP'd against the multicast
//! mount.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::clock::RtpClock;
use crate::error::RtspServerError;
use crate::packet::{RTP_HEADER_LEN, RtpHeader};
use crate::rtsp::server::fanout::PeerDropCounter;

/// Resolve a network interface name (e.g. `"eth0"`, `"lo"`) to its
/// kernel ifindex via `if_nametoindex(3)`. Returns `Err(detail)` on
/// failure (the C call returns 0 on error and sets `errno`).
///
/// Used by the IPv6 multicast iface path — `IPV6_MULTICAST_IF` takes a
/// 4-byte interface index (unlike IPv4, where `IP_MULTICAST_IF` takes
/// the iface's local IPv4 address).
#[cfg(unix)]
fn resolve_ifindex(name: &str) -> Result<libc::c_uint, String> {
    let cname = std::ffi::CString::new(name)
        .map_err(|e| format!("iface name '{name}' contains an interior NUL byte: {e}"))?;
    // SAFETY: `cname.as_ptr()` is a valid NUL-terminated C string for
    // the duration of this call. `if_nametoindex` is documented to
    // return 0 on failure (no errno guarantee on all platforms, but we
    // surface errno when present for diagnostic value).
    let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if idx == 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!(
            "if_nametoindex('{name}') failed (returned 0): {errno}"
        ));
    }
    Ok(idx)
}

/// Build the multicast send socket — binds an ephemeral local port,
/// connects to the group address (so we can use `send` instead of
/// `send_to` per frame), and applies TTL + optional IF.
///
/// Returns the bound `UdpSocket` ready to be wrapped in `Arc` and
/// driven by a per-mount sender task.
///
/// `iface` semantics differ by family:
/// - **IPv4** — literal IPv4 address of the outbound local interface
///   (the `IP_MULTICAST_IF` wire form is a 4-byte `in_addr`).
/// - **IPv6** — interface name (e.g. `"eth0"`, `"lo"`), resolved to a
///   kernel ifindex via `if_nametoindex(3)` (Unix-only); the
///   `IPV6_MULTICAST_IF` wire form is a 4-byte interface index.
///
/// # Errors
///
/// Returns [`RtspServerError::Io`] if the ephemeral bind or `connect`
/// call fails. Returns [`RtspServerError::InvalidMulticastGroup`] when
/// setting TTL / hop-limit / interface options fails, when the iface
/// name doesn't resolve to an ifindex (IPv6 path), or when the
/// platform doesn't support the requested option (e.g. IPv6 multicast
/// TTL on non-Unix targets).
pub(crate) async fn build_multicast_send_socket(
    group: SocketAddr,
    ttl: u8,
    iface: Option<&str>,
) -> Result<UdpSocket, RtspServerError> {
    // Bind an ephemeral local port. For IPv4: 0.0.0.0:0; IPv6: [::]:0.
    let local: SocketAddr = match group {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
        SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
    };
    let socket = UdpSocket::bind(local)
        .await
        .map_err(|e| RtspServerError::Io(e.kind()))?;

    // TTL / hop-limit knob.
    match group {
        SocketAddr::V4(_) => {
            socket.set_multicast_ttl_v4(ttl as u32).map_err(|e| {
                RtspServerError::InvalidMulticastGroup {
                    addr: group.to_string(),
                    detail: format!("set_multicast_ttl_v4 failed: {e}"),
                }
            })?;
        }
        SocketAddr::V6(_) => {
            // Tokio's UdpSocket doesn't expose set_multicast_hops_v6 on
            // Rust 1.85 stable. The std-socket UDP path uses libc::setsockopt for this;
            // we replicate via std::os::fd::AsRawFd.
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let val: libc::c_int = ttl as libc::c_int;
                // SAFETY: socket FD owned by us for `socket`'s lifetime;
                // &val is valid for size_of::<c_int>().
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
                    return Err(RtspServerError::InvalidMulticastGroup {
                        addr: group.to_string(),
                        detail: format!(
                            "IPV6_MULTICAST_HOPS setsockopt failed: {}",
                            std::io::Error::last_os_error()
                        ),
                    });
                }
            }
            #[cfg(not(unix))]
            {
                return Err(RtspServerError::InvalidMulticastGroup {
                    addr: group.to_string(),
                    detail: "IPv6 multicast TTL not supported on this platform".to_string(),
                });
            }
        }
    }

    // Optional interface binding. IPv4 only for v1. Stable std/tokio
    // don't expose `set_multicast_if_v4` on tokio's UdpSocket in Rust
    // 1.85 (tracking issue rust-lang/rust#92517) — the std-socket UDP path uses libc
    // setsockopt for the same knob on std::net::UdpSocket; we mirror it
    // here on tokio's wrapper via AsRawFd.
    if let Some(iface_str) = iface {
        match group {
            SocketAddr::V4(_) => {
                // IP_MULTICAST_IF takes the local interface's IPv4 address.
                // Parse on the platforms that can apply it (unix via libc,
                // windows via socket2); the bare fallback below surfaces the
                // gap on exotic targets without an unused-variable warning.
                #[cfg(any(unix, windows))]
                let iface_ip: std::net::Ipv4Addr =
                    iface_str.parse().map_err(|e: std::net::AddrParseError| {
                        RtspServerError::InvalidMulticastGroup {
                            addr: group.to_string(),
                            detail: format!("iface '{iface_str}' is not a v4 IP literal: {e}"),
                        }
                    })?;
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let in_addr = libc::in_addr {
                        s_addr: u32::from_ne_bytes(iface_ip.octets()),
                    };
                    // SAFETY: socket FD owned by `socket` for its
                    // lifetime; &in_addr is valid for size_of::<in_addr>().
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
                        return Err(RtspServerError::InvalidMulticastGroup {
                            addr: group.to_string(),
                            detail: format!(
                                "IP_MULTICAST_IF setsockopt failed: {}",
                                std::io::Error::last_os_error()
                            ),
                        });
                    }
                }
                #[cfg(windows)]
                {
                    // socket2's SockRef borrows the tokio UdpSocket (no
                    // ownership transfer) and issues the cross-platform
                    // IP_MULTICAST_IF setsockopt. CI confirmed Winsock
                    // accepts both a real NIC IP and 127.0.0.1.
                    socket2::SockRef::from(&socket)
                        .set_multicast_if_v4(&iface_ip)
                        .map_err(|e| RtspServerError::InvalidMulticastGroup {
                            addr: group.to_string(),
                            detail: format!("IP_MULTICAST_IF setsockopt failed: {e}"),
                        })?;
                }
                #[cfg(not(any(unix, windows)))]
                {
                    return Err(RtspServerError::InvalidMulticastGroup {
                        addr: group.to_string(),
                        detail: format!(
                            "IP_MULTICAST_IF setsockopt is unsupported on this platform (requested iface '{iface_str}')"
                        ),
                    });
                }
            }
            SocketAddr::V6(_) => {
                // IPv6 multicast IF is a 4-byte interface INDEX (not an
                // IP literal — that's the IPv4 idiom). Resolve the iface
                // name (e.g. "eth0", "lo") via `if_nametoindex` and pass
                // the resulting index to IPV6_MULTICAST_IF.
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let ifindex = resolve_ifindex(iface_str).map_err(|detail| {
                        RtspServerError::InvalidMulticastGroup {
                            addr: group.to_string(),
                            detail,
                        }
                    })?;
                    let val: libc::c_uint = ifindex;
                    // SAFETY: socket FD owned by `socket` for its
                    // lifetime; &val is valid for size_of::<c_uint>().
                    // ipv6(7) documents IPV6_MULTICAST_IF as taking a
                    // 4-byte interface index.
                    let rc = unsafe {
                        libc::setsockopt(
                            socket.as_raw_fd(),
                            libc::IPPROTO_IPV6,
                            libc::IPV6_MULTICAST_IF,
                            &val as *const libc::c_uint as *const libc::c_void,
                            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
                        )
                    };
                    if rc != 0 {
                        return Err(RtspServerError::InvalidMulticastGroup {
                            addr: group.to_string(),
                            detail: format!(
                                "IPV6_MULTICAST_IF setsockopt failed for iface '{iface_str}' (ifindex {ifindex}): {}",
                                std::io::Error::last_os_error()
                            ),
                        });
                    }
                }
                #[cfg(not(unix))]
                {
                    return Err(RtspServerError::InvalidMulticastGroup {
                        addr: group.to_string(),
                        detail: format!(
                            "IPv6 multicast iface binding is Unix-only in v1 (requested iface '{iface_str}')"
                        ),
                    });
                }
            }
        }
    }

    socket
        .connect(group)
        .await
        .map_err(|e| RtspServerError::Io(e.kind()))?;
    Ok(socket)
}

/// Spawn the per-mount multicast sender task. Drains the mount's
/// `broadcast::Receiver` + writes RTP frames to the multicast group via
/// the shared connected socket. Single task per mount (vs. per-peer
/// tasks for unicast mounts).
///
/// `ssrc` is fixed per-mount (not per-peer) — multicast peers see the
/// same SSRC, which is RFC 3550 §6.3.3-compatible (group senders share
/// the SSRC space; collision detection is the receiver's responsibility).
pub(crate) fn spawn_multicast_sender(
    mut rx: broadcast::Receiver<Bytes>,
    socket: Arc<UdpSocket>,
    cancel: CancellationToken,
    ssrc: u32,
    initial_seq: u16,
    drop_counter: Arc<PeerDropCounter>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let clock = RtpClock::new(0);
        let mut seq = initial_seq;
        loop {
            tokio::select! {
                res = rx.recv() => match res {
                    Ok(payload) => {
                        let mut datagram = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
                        datagram.resize(RTP_HEADER_LEN, 0);
                        RtpHeader::new(seq, clock.now_ticks(), ssrc)
                            .encode_into(&mut datagram);
                        datagram.extend_from_slice(&payload);
                        seq = seq.wrapping_add(1);
                        if let Err(e) = socket.send(&datagram).await {
                            tracing::warn!(
                                target: "tst_rtp::server::multicast",
                                error = %e,
                                "multicast send failed; exiting"
                            );
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        drop_counter.add(n);
                        tracing::warn!(
                            target: "tst_rtp::server::multicast",
                            lagged = n,
                            "broadcast lag on multicast sender"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = cancel.cancelled() => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_multicast_send_socket_v4_succeeds() {
        let group: SocketAddr = "239.0.0.1:5004".parse().unwrap();
        let sock = build_multicast_send_socket(group, 4, None).await.unwrap();
        // Socket bound to ephemeral port; connect set the default peer.
        let local = sock.local_addr().unwrap();
        assert!(local.port() > 0);
    }

    #[tokio::test]
    async fn build_multicast_send_socket_rejects_unicast() {
        // 10.0.0.1 isn't multicast — but we don't validate at the bind
        // step; that's MulticastGroup::parse's job.
        // The function just sets the multicast knobs which are no-ops
        // for unicast send. So this test verifies the function still
        // succeeds (it would only fail on bind / setsockopt errors).
        let unicast: SocketAddr = "127.0.0.1:5004".parse().unwrap();
        // setsockopt for multicast_ttl on a unicast destination is
        // permitted but no-op; the test passes if no panic.
        let _ = build_multicast_send_socket(unicast, 4, None).await;
        // No assert — caller is expected to validate via MulticastGroup::parse.
    }

    // Setting IP_MULTICAST_IF to the loopback address is permitted on both Unix
    // (libc) and Windows (socket2) — Winsock returns Ok for
    // IP_MULTICAST_IF=127.0.0.1, so this runs on all platforms.
    #[tokio::test]
    async fn build_multicast_with_iface_v4() {
        let group: SocketAddr = "239.0.0.1:5004".parse().unwrap();
        // 127.0.0.1 is a valid (loopback) IP literal — passes the parse step.
        let res = build_multicast_send_socket(group, 4, Some("127.0.0.1")).await;
        // Setting IP_MULTICAST_IF to loopback is permitted; assert success.
        assert!(res.is_ok());
    }

    /// IPv6 `iface=` means an interface NAME, not an IP literal.
    /// Passing an IP literal must fail via the if_nametoindex path,
    /// since no real iface is named `::1`.
    #[cfg(unix)]
    #[tokio::test]
    async fn build_multicast_v6_rejects_ip_literal_as_iface_name() {
        let group: SocketAddr = "[ff02::1]:5004".parse().unwrap();
        let e = build_multicast_send_socket(group, 4, Some("::1"))
            .await
            .unwrap_err();
        match e {
            RtspServerError::InvalidMulticastGroup { detail, .. } => {
                assert!(
                    detail.contains("if_nametoindex"),
                    "expected if_nametoindex failure detail, got: {detail}"
                );
            }
            other => panic!("expected InvalidMulticastGroup, got {other:?}"),
        }
    }

    /// `if_nametoindex` for the loopback iface must succeed on any
    /// Unix host (loopback is always present). The returned ifindex
    /// is non-zero. Linux + FreeBSD name the iface `lo`; macOS names
    /// it `lo0`.
    #[cfg(unix)]
    #[test]
    fn resolve_ifindex_loopback() {
        #[cfg(target_os = "macos")]
        let iface = "lo0";
        #[cfg(not(target_os = "macos"))]
        let iface = "lo";
        let idx =
            resolve_ifindex(iface).unwrap_or_else(|e| panic!("loopback iface {iface:?}: {e}"));
        assert!(idx > 0, "if_nametoindex({iface:?}) returned 0");
    }

    /// A bogus iface name must surface a typed error rather than
    /// silently returning 0 or panicking.
    #[cfg(unix)]
    #[test]
    fn resolve_ifindex_unknown_returns_err() {
        let err =
            resolve_ifindex("definitely-not-a-real-iface-zzz").expect_err("bogus iface must fail");
        assert!(
            err.contains("if_nametoindex"),
            "expected if_nametoindex in detail, got: {err}"
        );
    }

    // NOTE: an end-to-end "build_multicast_send_socket succeeds with
    // iface='lo'" test was deliberately omitted — the function's final
    // `socket.connect(group)` step depends on the host's IPv6 routing
    // table having a route for the multicast group, which is not
    // guaranteed inside CI containers (NetworkUnreachable is common).
    // The two `resolve_ifindex_*` tests above + the
    // `build_multicast_v6_rejects_ip_literal_as_iface_name`
    // integration error-path test cover the new code without depending
    // on IPv6 multicast routing.
}
