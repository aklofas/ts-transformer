//! Per-transport stats.

use tst_core::transport::SocketStats;

tst_core::binding_record! {
    /// Cumulative stats snapshot for a TCP transport handle. Both send and
    /// receive counters are populated on the same handle (TCP is full-duplex).
    #[derive(Debug, Clone, Copy, Default)]
    #[non_exhaustive]
    pub struct TcpStats {
        /// Bytes successfully sent.
        pub bytes_sent: u64,
        /// Bytes successfully received.
        pub bytes_received: u64,
        /// Number of successful send calls.
        pub send_calls: u64,
        /// Number of successful recv calls.
        pub recv_calls: u64,
        /// Send-side I/O errors.
        pub send_errors: u64,
        /// Receive-side I/O errors.
        pub recv_errors: u64,
    }
}

impl TcpStats {
    /// Project to the workspace-uniform [`SocketStats`].
    ///
    /// `SocketStats` is `#[non_exhaustive]` so we construct via Default
    /// and assign fields (struct expression construction from outside the
    /// defining crate is blocked by Rust RFC 2008).
    pub fn to_socket_stats(&self) -> SocketStats {
        let mut s = SocketStats::default();
        s.bytes_sent = self.bytes_sent;
        s.packets_sent = self.send_calls;
        s.bytes_received = self.bytes_received;
        s.packets_received = self.recv_calls;
        s.packets_dropped_send = self.send_errors;
        s.packets_dropped_recv = self.recv_errors;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection() {
        let t = TcpStats {
            bytes_sent: 1000,
            bytes_received: 500,
            send_calls: 5,
            recv_calls: 3,
            send_errors: 1,
            recv_errors: 0,
        };
        let s = t.to_socket_stats();
        assert_eq!(s.bytes_sent, 1000);
        assert_eq!(s.packets_sent, 5);
        assert_eq!(s.bytes_received, 500);
    }
}
