//! Configuration structs — the canonical types for socket/listener setup.
//!
//! `SocketBuilder`/`ListenerBuilder` (in `builder.rs`) are sugar over these.
//! Bindings (UniFFI dictionaries, JNI POJOs, cbindgen C structs) consume
//! these directly.

use crate::options::{
    Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, Role, StreamId,
};
use std::time::Duration;

/// Sender-pipeline default for `SRTO_CONNTIMEO`. libsrt's default is 3s,
/// which is too short for the radio-link domain this library targets
/// (LOS-over-terrain interruptions, antenna repointing, radio warm-up).
const SENDER_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Sender-pipeline default for `SRTO_LINGER`. libsrt's live-mode default is
/// off (`l_onoff=0` — close does not wait, and data still queued in the
/// send buffer is dropped). 5s is long enough to drain a
/// small backlog under healthy conditions, short enough to never stall a
/// `ManagedTransport` reconnect cycle noticeably.
const SENDER_DEFAULT_LINGER: Duration = Duration::from_secs(5);

/// Receiver-pipeline default for `SRTO_CONNTIMEO`. Caller-mode receivers
/// face the same radio-link rendezvous reality as senders.
const RECEIVER_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Configuration for a caller-side data socket.
///
/// All `Option<T>` fields default to `None`, meaning "leave the libsrt default."
/// `KeyLength` defaults to `Aes128` (only relevant when `passphrase` is set).
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct SocketConfig {
    // Encryption
    pub passphrase: Option<Passphrase>,
    pub key_length: KeyLength,

    // Timeouts
    pub send_timeout: Option<Duration>,
    pub recv_timeout: Option<Duration>,
    /// Maximum time `connect_with` waits for handshake completion before
    /// giving up. `None` keeps libsrt's 3-second default. For radio links
    /// (LOS interruptions, antenna repointing) [`Self::sender_defaults`],
    /// [`Self::receiver_defaults`] and [`SrtUrl::connect`](crate::SrtUrl::connect)
    /// set it to 15s where it is unset.
    pub connect_timeout: Option<Duration>,
    /// SRT linger timeout. `None` preserves libsrt's default — which is
    /// `l_onoff=0, l_linger=0` (linger off; close returns immediately and
    /// data still queued in the send buffer is dropped). Note: this differs
    /// from the kernel SO_LINGER default; libsrt initializes its own
    /// `struct linger`. See `srtcore/socketconfig.h:333-336`.
    pub linger: Option<Duration>,

    // Latency / buffering
    pub latency: Option<Duration>,
    pub peer_latency: Option<Duration>,
    pub recv_latency: Option<Duration>,
    /// Receive buffer size in **bytes**, passed verbatim to `SRTO_RCVBUF`.
    ///
    /// libsrt converts the byte value to an internal buffer count by dividing
    /// by `(MSS - 28)` (header overhead) with a floor of 32 buffers.
    /// For high-latency links, size as `RCVBUF ≥ latency × bitrate`.
    /// Values above `i32::MAX` are rejected (`OptionError::OutOfRange`).
    ///
    /// **Ceiling:** libsrt silently clamps the stored size to the
    /// flow-control window ([`Self::flow_window_packets`], `SRTO_FC`,
    /// default 25600 packets — ≈ 37.7 MB at the default MSS) and still
    /// reports success. Requests above the ceiling emit a
    /// `tracing::warn!` naming both values; raise `flow_window_packets`
    /// together with this field to actually get a larger buffer.
    ///
    /// `None` leaves the libsrt default.
    pub recv_buf_bytes: Option<u32>,
    /// Send buffer size in **bytes**, passed verbatim to `SRTO_SNDBUF`.
    ///
    /// Same internal conversion as [`Self::recv_buf_bytes`]: libsrt divides
    /// by `(MSS - 28)` with a 32-buffer floor.
    /// Values above `i32::MAX` are rejected (`OptionError::OutOfRange`).
    ///
    /// `None` leaves the libsrt default.
    pub send_buf_bytes: Option<u32>,

    // Bandwidth
    pub max_bandwidth: Option<MaxBandwidth>,
    pub input_bandwidth: Option<u64>,
    pub overhead_bandwidth_pct: Option<u8>,

    // Wire / packet sizing
    pub mss: Option<u16>,
    pub payload_size: Option<u16>,

    /// Kernel UDP socket receive buffer in bytes (`SRTO_UDP_RCVBUF` —
    /// separate from SRT's own packet queue). For >25 Mbps streams,
    /// kernel UDP drops can masquerade as transmission losses; raising
    /// this helps.
    ///
    /// **Silent kernel clamp:** Linux caps the effective size at
    /// `net.core.rmem_max` without reporting an error, and the clamp is
    /// NOT visible through libsrt option readback (libsrt echoes the
    /// stored request, not the kernel's actual buffer). If a large value
    /// here doesn't cure drops, check `sysctl net.core.rmem_max`.
    pub udp_recv_buffer_bytes: Option<u32>,
    /// Kernel UDP socket send buffer in bytes (`SRTO_UDP_SNDBUF`). Same
    /// silent-clamp caveat as [`Self::udp_recv_buffer_bytes`], against
    /// `net.core.wmem_max`.
    pub udp_send_buffer_bytes: Option<u32>,

    // Identification
    pub stream_id: Option<StreamId>,
    /// Direction this socket is opened for. Drives `SRTO_SENDER` for
    /// HSv4-peer latency-negotiation compatibility. Defaults to
    /// `Role::Receiver` (libsrt default — `SRTO_SENDER=0`).
    pub role: Role,

    // Reliability / loss
    pub loss_max_ttl: Option<u32>,

    // Congestion / priority
    pub too_late_packet_drop: Option<bool>,
    /// Flow-control window in packets (`SRTO_FC`, libsrt default 25600).
    /// Also the hard ceiling for [`Self::recv_buf_bytes`] — libsrt
    /// silently clamps the receive buffer to this window, so raise both
    /// together for large-buffer configurations.
    pub flow_window_packets: Option<u32>,
    pub packet_filter: Option<PacketFilter>,
    pub congestion: Option<Congestion>,
}

impl SocketConfig {
    /// Returns a `SocketConfig` pre-populated with the live-streaming sender
    /// preset suitable for radio-link MPEG-TS+SRT delivery from gimbaled
    /// platforms:
    ///
    /// - `connect_timeout = 15s` — accommodates LOS interruptions, antenna
    ///   repointing, and radio warm-up (libsrt default 3s is too short).
    /// - `linger = 5s` — drains a small backlog on graceful close without
    ///   stalling a reconnect cycle (libsrt default is off — drops queued
    ///   data on close).
    /// - `role = Role::Sender` — sets `SRTO_SENDER=1` for HSv4-peer
    ///   compatibility (older Teradek/Makito gear, cable-industry hardware).
    ///
    /// All other fields take their `Default` value. Use struct-update syntax
    /// to layer caller-specific fields:
    ///
    /// ```
    /// # use tst_srt::SocketConfig;
    /// # use tst_srt::options::Passphrase;
    /// # let passphrase = Passphrase::new("secretsecretsecret").unwrap();
    /// let mut cfg = SocketConfig::sender_defaults();
    /// cfg.passphrase = Some(passphrase);
    /// ```
    pub fn sender_defaults() -> Self {
        let mut cfg = Self::default();
        cfg.merge_sender_defaults();
        cfg
    }

    /// Apply sender defaults to fields the caller has not explicitly set.
    /// Preserves caller intent: only fills in `connect_timeout` if `None`,
    /// `linger` if `None`, and `role` if `Role::Receiver` (the default).
    /// Idempotent when called repeatedly on a sender-configured `SocketConfig`.
    ///
    /// Useful when a caller has parsed configuration from another source
    /// (e.g. URL query parameters via `tst_srt::url::parse`) and wants to
    /// fill in the sender-pipeline defaults for any fields the source
    /// did not specify.
    ///
    /// # Note on Role ambiguity
    ///
    /// Because [`Role::Receiver`] is simultaneously the default value and
    /// the sentinel this method checks ("role unset → fill with Sender"),
    /// there is no way to distinguish "I want this socket to be a Receiver"
    /// from "I haven't set role yet." This method always promotes a
    /// Receiver-roled config to Sender. Callers who need an explicit
    /// Receiver role should not call `merge_sender_defaults` (use
    /// [`merge_receiver_defaults`](Self::merge_receiver_defaults) or set
    /// the role explicitly via other means after this method instead).
    pub fn merge_sender_defaults(&mut self) {
        if self.connect_timeout.is_none() {
            self.connect_timeout = Some(SENDER_DEFAULT_CONNECT_TIMEOUT);
        }
        if self.linger.is_none() {
            self.linger = Some(SENDER_DEFAULT_LINGER);
        }
        if self.role == Role::Receiver {
            self.role = Role::Sender;
        }
    }

    /// Returns a `SocketConfig` pre-populated with the live-streaming
    /// receiver preset:
    ///
    /// - `connect_timeout = 15s` — caller-mode receivers face the same
    ///   radio-link rendezvous reality as senders.
    /// - `role = Role::Receiver` — does not set `SRTO_SENDER` (libsrt default).
    ///
    /// `linger` is left at `None` (libsrt default — off) because receivers
    /// have no outbound queue to drain.
    pub fn receiver_defaults() -> Self {
        let mut cfg = Self::default();
        cfg.merge_receiver_defaults();
        cfg
    }

    /// Apply receiver defaults to fields the caller has not explicitly set.
    /// Mirrors `merge_sender_defaults` for the receiver pipeline. Idempotent.
    /// The role field is left untouched: `Role::Receiver` is already the
    /// default, so no fill-in is needed.
    pub fn merge_receiver_defaults(&mut self) {
        if self.connect_timeout.is_none() {
            self.connect_timeout = Some(RECEIVER_DEFAULT_CONNECT_TIMEOUT);
        }
    }
}

/// Configuration for a listener (passive) socket.
///
/// Most fields parallel `SocketConfig`; the listener applies them to itself
/// and inherits the timeout fields onto sockets returned by `accept()`.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    // Encryption (applied to listener; libsrt enforces during handshake)
    pub passphrase: Option<Passphrase>,
    pub key_length: KeyLength,

    // Latency / buffering (listener-side)
    pub latency: Option<Duration>,
    pub recv_latency: Option<Duration>,
    /// Receive buffer size in **bytes**, passed verbatim to `SRTO_RCVBUF`.
    ///
    /// See [`SocketConfig::recv_buf_bytes`] for sizing guidance.
    pub recv_buf_bytes: Option<u32>,

    // Bandwidth
    pub max_bandwidth: Option<MaxBandwidth>,
    pub overhead_bandwidth_pct: Option<u8>,

    // Wire / packet sizing
    pub mss: Option<u16>,
    pub payload_size: Option<u16>,

    /// Underlying UDP socket recv buffer (separate from SRT's packet queue).
    /// See `SocketConfig::udp_recv_buffer_bytes` for context.
    pub udp_recv_buffer_bytes: Option<u32>,

    // Reliability / loss
    pub loss_max_ttl: Option<u32>,

    // Congestion / priority
    pub too_late_packet_drop: Option<bool>,
    /// Flow-control window in packets (`SRTO_FC`, libsrt default 25600).
    /// Also the hard ceiling for [`Self::recv_buf_bytes`] — libsrt
    /// silently clamps the receive buffer to this window, so raise both
    /// together for large-buffer configurations.
    pub flow_window_packets: Option<u32>,
    pub packet_filter: Option<PacketFilter>,
    pub congestion: Option<Congestion>,

    // Listener-specific
    pub backlog: u32,
    pub reuse_addr: bool,

    // Inherited by accepted sockets
    pub recv_timeout: Option<Duration>,
    pub send_timeout: Option<Duration>,

    /// SRT linger timeout. `None` preserves libsrt's default — which is
    /// `l_onoff=0, l_linger=0` (linger off; close returns immediately and
    /// data still queued in the send buffer is dropped). Note: this differs
    /// from the kernel SO_LINGER default; libsrt initializes its own
    /// `struct linger`. See `srtcore/socketconfig.h:333-336`. Inherited by
    /// accepted sockets via libsrt's option-inheritance (PRE options).
    pub linger: Option<Duration>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            passphrase: None,
            key_length: KeyLength::default(),
            latency: None,
            recv_latency: None,
            recv_buf_bytes: None,
            max_bandwidth: None,
            overhead_bandwidth_pct: None,
            mss: None,
            payload_size: None,
            udp_recv_buffer_bytes: None,
            loss_max_ttl: None,
            too_late_packet_drop: None,
            flow_window_packets: None,
            packet_filter: None,
            congestion: None,
            backlog: 5,
            reuse_addr: true,
            recv_timeout: None,
            send_timeout: None,
            linger: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_default_is_unencrypted() {
        let cfg = SocketConfig::default();
        assert!(cfg.passphrase.is_none());
        assert_eq!(cfg.key_length, KeyLength::Aes128);
    }

    #[test]
    fn listener_default_has_backlog_5() {
        let cfg = ListenerConfig::default();
        assert_eq!(cfg.backlog, 5);
        assert!(cfg.reuse_addr);
    }

    #[test]
    fn sender_defaults_sets_three_fields() {
        let cfg = SocketConfig::sender_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
        assert_eq!(cfg.linger, Some(Duration::from_secs(5)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn sender_defaults_leaves_other_fields_at_default() {
        let cfg = SocketConfig::sender_defaults();
        assert!(cfg.passphrase.is_none());
        assert_eq!(cfg.key_length, KeyLength::Aes128);
        assert!(cfg.send_timeout.is_none());
        assert!(cfg.recv_timeout.is_none());
        assert!(cfg.latency.is_none());
        assert!(cfg.peer_latency.is_none());
        assert!(cfg.recv_latency.is_none());
        assert!(cfg.recv_buf_bytes.is_none());
        assert!(cfg.send_buf_bytes.is_none());
        assert!(cfg.max_bandwidth.is_none());
        assert!(cfg.input_bandwidth.is_none());
        assert!(cfg.overhead_bandwidth_pct.is_none());
        assert!(cfg.mss.is_none());
        assert!(cfg.payload_size.is_none());
        assert!(cfg.udp_recv_buffer_bytes.is_none());
        assert!(cfg.udp_send_buffer_bytes.is_none());
        assert!(cfg.stream_id.is_none());
        assert!(cfg.loss_max_ttl.is_none());
        assert!(cfg.too_late_packet_drop.is_none());
        assert!(cfg.flow_window_packets.is_none());
        assert!(cfg.packet_filter.is_none());
        assert!(cfg.congestion.is_none());
    }

    #[test]
    fn merge_sender_defaults_fills_unset_fields() {
        let mut cfg = SocketConfig::default();
        cfg.merge_sender_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
        assert_eq!(cfg.linger, Some(Duration::from_secs(5)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn merge_sender_defaults_is_idempotent() {
        let mut cfg = SocketConfig::default();
        cfg.merge_sender_defaults();
        let after_first = cfg.clone();
        cfg.merge_sender_defaults();
        assert_eq!(cfg.connect_timeout, after_first.connect_timeout);
        assert_eq!(cfg.linger, after_first.linger);
        assert_eq!(cfg.role, after_first.role);
    }

    #[test]
    fn merge_sender_defaults_preserves_explicit_connect_timeout() {
        let mut cfg = SocketConfig {
            connect_timeout: Some(Duration::from_secs(7)),
            ..Default::default()
        };
        cfg.merge_sender_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(cfg.linger, Some(Duration::from_secs(5)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn merge_sender_defaults_preserves_explicit_linger() {
        let mut cfg = SocketConfig {
            linger: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        cfg.merge_sender_defaults();
        assert_eq!(cfg.linger, Some(Duration::from_secs(30)));
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn receiver_defaults_sets_one_field() {
        let cfg = SocketConfig::receiver_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
        // Role::Receiver is the default; receiver_defaults leaves it at default.
        assert_eq!(cfg.role, Role::Receiver);
        // Linger stays at libsrt default (None) — receivers have no outbound
        // queue to drain.
        assert!(cfg.linger.is_none());
    }

    #[test]
    fn merge_receiver_defaults_fills_unset_fields() {
        let mut cfg = SocketConfig::default();
        cfg.merge_receiver_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
        assert_eq!(cfg.role, Role::Receiver);
        assert!(cfg.linger.is_none());
    }

    #[test]
    fn merge_receiver_defaults_preserves_explicit_connect_timeout() {
        let mut cfg = SocketConfig {
            connect_timeout: Some(Duration::from_secs(7)),
            ..Default::default()
        };
        cfg.merge_receiver_defaults();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(cfg.role, Role::Receiver);
    }

    #[test]
    fn merge_receiver_defaults_preserves_explicit_sender_role() {
        // Explicit Role::Sender is not overridden by merge_receiver_defaults.
        let mut cfg = SocketConfig {
            role: Role::Sender,
            ..Default::default()
        };
        cfg.merge_receiver_defaults();
        assert_eq!(cfg.role, Role::Sender);
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
    }
}
