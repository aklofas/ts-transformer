//! Per-transport stats.
//!
//! UDP itself exposes very little — there's no acknowledgment plane and no
//! kernel-level loss accounting in std::net. We track what we can: datagrams
//! sent/received + bytes. Future enhancement: poll `SO_RCVBUF`/`SO_SNDBUF`
//! drop counters via the cmsg path (libc-only).

use tst_core::transport::SocketStats;

tst_core::binding_record! {
    /// Cumulative stats snapshot for a UDP transport handle. Send-side counters
    /// are zero on a receive-only handle and vice-versa.
    #[derive(Debug, Clone, Copy, Default)]
    #[non_exhaustive]
    pub struct UdpStats {
        /// Datagrams successfully sent (sender only).
        pub datagrams_sent: u64,
        /// Bytes successfully sent (sender only): the sum of datagram payload sizes.
        pub bytes_sent: u64,
        /// Datagrams successfully received (receiver only).
        pub datagrams_received: u64,
        /// Bytes successfully received (receiver only).
        pub bytes_received: u64,
        /// Send-side I/O errors (sender only), such as an oversize message or an
        /// unreachable network.
        pub send_errors: u64,
        /// Receive-side I/O errors (receiver only); a would-block or a timeout is
        /// not an error.
        pub recv_errors: u64,
    }
}

impl UdpStats {
    /// Project to the workspace-uniform [`SocketStats`].
    ///
    /// Fields not meaningful for raw UDP (RTT, bandwidth estimates,
    /// retransmits, kernel-side loss counters) are zeroed.
    pub fn to_socket_stats(&self) -> SocketStats {
        // SocketStats is `#[non_exhaustive]` so we can't construct via a
        // struct expression from this crate. Build via Default + field
        // assignment instead.
        let mut s = SocketStats::default();
        s.bytes_sent = self.bytes_sent;
        s.packets_sent = self.datagrams_sent;
        s.bytes_received = self.bytes_received;
        s.packets_received = self.datagrams_received;
        s.packets_dropped_send = self.send_errors;
        s.packets_dropped_recv = self.recv_errors;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_preserves_counters() {
        let u = UdpStats {
            datagrams_sent: 10,
            bytes_sent: 1316 * 10,
            datagrams_received: 5,
            bytes_received: 1316 * 5,
            send_errors: 1,
            recv_errors: 2,
        };
        let s = u.to_socket_stats();
        assert_eq!(s.packets_sent, 10);
        assert_eq!(s.bytes_sent, 13160);
        assert_eq!(s.packets_received, 5);
        assert_eq!(s.bytes_received, 6580);
        assert_eq!(s.packets_dropped_send, 1);
        assert_eq!(s.packets_dropped_recv, 2);
        assert_eq!(s.rtt_us, 0);
    }
}
