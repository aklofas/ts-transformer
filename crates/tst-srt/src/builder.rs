//! Fluent builders over `SocketConfig` / `ListenerConfig`.
//!
//! The builder is sugar; the canonical type is the config struct.
//! Bindings consume the config struct directly. Rust callers can use either.

use crate::config::{ListenerConfig, SocketConfig};
use crate::error::{BindError, ConnectError, OptionError, StreamIdError};
use crate::listener::Listener;
use crate::options::{
    Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, Role, StreamId,
};
use crate::socket::Socket;
use std::net::ToSocketAddrs;
use std::time::Duration;

/// Fluent builder for [`Socket`] with chained option setters.
///
/// `SocketBuilder::new()` starts from libsrt defaults; each setter mutates the
/// underlying [`SocketConfig`] in place and returns `&mut Self` so calls can be
/// chained. The terminal call is [`SocketBuilder::connect`], which takes
/// `&self` (clones the inner config) and opens the socket.
///
/// The `&mut self -> &mut Self` shape translates directly to Kotlin's `apply`
/// scope, Swift's `var b = …; b.x(); b.y();`, Java's chain on a fresh local,
/// and Python's step-wise assignment — see `docs/reference/binding-authors.md`.
///
/// See [`SocketBuilder::sender_defaults`] / [`SocketBuilder::receiver_defaults`]
/// for live-streaming preset bundles.
///
/// # Example — sender with non-default latency, bandwidth cap, and passphrase
/// ```no_run
/// use tst_srt::{MaxBandwidth, Passphrase, SocketBuilder};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let passphrase = Passphrase::new("hunter2hunter2")?;
///
/// let mut b = SocketBuilder::new();
/// b.sender_defaults();
/// b.latency_ms(120);
/// b.max_bandwidth(MaxBandwidth::Limited(1_250_000)); // 10 Mbps cap
/// b.passphrase(passphrase);
/// let socket = b.connect("127.0.0.1:9000")?;
/// # let _ = socket;
/// # Ok(())
/// # }
/// ```
#[must_use]
#[derive(Default)]
pub struct SocketBuilder {
    config: SocketConfig,
}

impl SocketBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn passphrase(&mut self, p: Passphrase) -> &mut Self {
        self.config.passphrase = Some(p);
        self
    }
    pub fn key_length(&mut self, kl: KeyLength) -> &mut Self {
        self.config.key_length = kl;
        self
    }
    pub fn send_timeout(&mut self, t: Duration) -> &mut Self {
        self.config.send_timeout = Some(t);
        self
    }
    pub fn recv_timeout(&mut self, t: Duration) -> &mut Self {
        self.config.recv_timeout = Some(t);
        self
    }
    /// Set `SRTO_CONNTIMEO` — maximum handshake-wait duration on `connect`.
    /// libsrt default 3s; recommend 10–15s for radio links.
    pub fn connect_timeout(&mut self, t: Duration) -> &mut Self {
        self.config.connect_timeout = Some(t);
        self
    }
    /// Set `SRTO_LINGER` — drop/close grace period for unsent data.
    /// `None` preserves libsrt's default — which is `l_onoff=0, l_linger=0`
    /// (linger off; close returns immediately and libsrt's internal queue
    /// drains in the background). Note: this differs from the kernel
    /// SO_LINGER default; libsrt initializes its own `struct linger`. See
    /// `srtcore/socketconfig.h:333-336`.
    pub fn linger(&mut self, d: Duration) -> &mut Self {
        self.config.linger = Some(d);
        self
    }
    pub fn latency(&mut self, d: Duration) -> &mut Self {
        self.config.latency = Some(d);
        self
    }
    pub fn latency_ms(&mut self, ms: u64) -> &mut Self {
        self.latency(Duration::from_millis(ms))
    }
    pub fn peer_latency(&mut self, d: Duration) -> &mut Self {
        self.config.peer_latency = Some(d);
        self
    }
    pub fn recv_latency(&mut self, d: Duration) -> &mut Self {
        self.config.recv_latency = Some(d);
        self
    }
    pub fn mss(&mut self, mss: u16) -> &mut Self {
        self.config.mss = Some(mss);
        self
    }
    pub fn payload_size(&mut self, n: u16) -> &mut Self {
        self.config.payload_size = Some(n);
        self
    }
    /// Set `SRTO_UDP_RCVBUF` (kernel UDP socket recv buffer in bytes).
    /// For >25 Mbps streams; default is OS-dependent (~208 KB on Linux).
    /// Linux clamps to `net.core.rmem_max` **silently** — and the clamp
    /// is not visible through libsrt option readback (libsrt echoes the
    /// stored request). If a large value doesn't cure kernel-level
    /// drops, check `sysctl net.core.rmem_max`.
    pub fn udp_recv_buffer_bytes(&mut self, n: u32) -> &mut Self {
        self.config.udp_recv_buffer_bytes = Some(n);
        self
    }
    /// Set `SRTO_UDP_SNDBUF` (kernel UDP socket send buffer in bytes).
    /// For >25 Mbps streams. Linux clamps to `net.core.wmem_max`
    /// silently (same readback caveat as [`Self::udp_recv_buffer_bytes`]).
    pub fn udp_send_buffer_bytes(&mut self, n: u32) -> &mut Self {
        self.config.udp_send_buffer_bytes = Some(n);
        self
    }
    pub fn max_bandwidth(&mut self, bw: MaxBandwidth) -> &mut Self {
        self.config.max_bandwidth = Some(bw);
        self
    }
    pub fn input_bandwidth(&mut self, bw: u64) -> &mut Self {
        self.config.input_bandwidth = Some(bw);
        self
    }
    pub fn overhead_bandwidth_pct(&mut self, pct: u8) -> &mut Self {
        self.config.overhead_bandwidth_pct = Some(pct);
        self
    }
    /// Set the SRT receive buffer size in **bytes** (`SRTO_RCVBUF`).
    ///
    /// Passed verbatim to libsrt, which converts to internal buffers via
    /// `(MSS - 28)` division with a 32-buffer floor.
    /// For high-latency links, size as `RCVBUF ≥ latency × bitrate`.
    ///
    /// libsrt silently clamps this to the flow-control window
    /// (`SRTO_FC`, default 25600 packets ≈ 37.7 MB at default MSS); a
    /// clamped request emits a `tracing::warn!`. Raise
    /// [`SocketConfig::flow_window_packets`](crate::SocketConfig::flow_window_packets)
    /// together with this for larger buffers.
    pub fn recv_buf_bytes(&mut self, n: u32) -> &mut Self {
        self.config.recv_buf_bytes = Some(n);
        self
    }
    /// Set the SRT send buffer size in **bytes** (`SRTO_SNDBUF`).
    ///
    /// Same internal conversion as [`Self::recv_buf_bytes`].
    pub fn send_buf_bytes(&mut self, n: u32) -> &mut Self {
        self.config.send_buf_bytes = Some(n);
        self
    }
    pub fn stream_id(&mut self, id: StreamId) -> &mut Self {
        self.config.stream_id = Some(id);
        self
    }
    /// Convenience: validate-and-set from `&str` / `String`. Returns error if invalid.
    ///
    /// # Errors
    /// Returns [`OptionError`] if the input fails [`StreamId`] validation.
    pub fn try_stream_id(
        &mut self,
        id: impl TryInto<StreamId, Error = StreamIdError>,
    ) -> Result<&mut Self, OptionError> {
        self.config.stream_id = Some(id.try_into()?);
        Ok(self)
    }
    pub fn loss_max_ttl(&mut self, n: u32) -> &mut Self {
        self.config.loss_max_ttl = Some(n);
        self
    }
    pub fn too_late_packet_drop(&mut self, on: bool) -> &mut Self {
        self.config.too_late_packet_drop = Some(on);
        self
    }
    pub fn flow_window_packets(&mut self, n: u32) -> &mut Self {
        self.config.flow_window_packets = Some(n);
        self
    }
    pub fn packet_filter(&mut self, pf: PacketFilter) -> &mut Self {
        self.config.packet_filter = Some(pf);
        self
    }
    pub fn congestion(&mut self, c: Congestion) -> &mut Self {
        self.config.congestion = Some(c);
        self
    }
    /// Set the role (drives `SRTO_SENDER` for HSv4-peer compatibility).
    /// Defaults to `Role::Receiver` (libsrt default — `SRTO_SENDER=0`).
    pub fn role(&mut self, role: Role) -> &mut Self {
        self.config.role = role;
        self
    }

    /// Apply live-streaming sender defaults — `connect_timeout=15s`,
    /// `linger=5s`, `role=Sender`. Preserves any fields the caller has
    /// already set explicitly (merge-if-default semantics — order-independent).
    ///
    /// ```
    /// # use tst_srt::{Socket, SocketBuilder};
    /// # use tst_srt::options::Passphrase;
    /// # let passphrase = Passphrase::new("secretsecretsecret").unwrap();
    /// let mut b = SocketBuilder::new();
    /// b.sender_defaults();
    /// b.passphrase(passphrase);
    /// // b.connect("host:port")?
    /// ```
    pub fn sender_defaults(&mut self) -> &mut Self {
        self.config.merge_sender_defaults();
        self
    }

    /// Apply live-streaming receiver defaults — `connect_timeout=15s`.
    /// Preserves any fields the caller has already set explicitly
    /// (merge-if-default semantics — order-independent). `role` defaults to
    /// `Role::Receiver` and is not altered.
    pub fn receiver_defaults(&mut self) -> &mut Self {
        self.config.merge_receiver_defaults();
        self
    }

    /// Reach the underlying config (for inspection, copying, FFI marshaling).
    /// Clones the inner config so the builder can be reused. Cloning is
    /// cheap — at most three short heap allocations (optional
    /// `Passphrase`, `StreamId`, and `PacketFilter`, all `String`-backed).
    pub fn config(&self) -> SocketConfig {
        self.config.clone()
    }

    /// Terminal call: open and connect the socket.
    ///
    /// Takes `&self` (not `self`) so the builder can be reused; clones the
    /// inner config into [`Socket::connect_with`]. Cloning is cheap — the
    /// config is a flat `SocketConfig` with at most three short heap
    /// allocations (the optional `Passphrase`, `StreamId`, and
    /// `PacketFilter`, all `String`-backed).
    ///
    /// # C ABI
    ///
    /// No direct counterpart — the C surface composes builder + transport +
    /// shell in a single call: `tst_mux_sender_open`, `tst_sender_open`,
    /// `tst_raw_sender_open`, `tst_managed_mux_sender_open`,
    /// `tst_managed_sender_open`, `tst_managed_raw_sender_open`. URL query
    /// parameters take the place of builder setters; see
    /// `bindings/c/include/tstrans.h` and `docs/reference/binding-authors.md`.
    ///
    /// # Errors
    /// Returns [`ConnectError`] on hostname-resolution failure, libsrt
    /// option-set failure, or SRT-handshake failure (handshake timeout,
    /// auth mismatch, peer reject, etc.).
    pub fn connect(&self, addr: impl ToSocketAddrs) -> Result<Socket, ConnectError> {
        Socket::connect_with(&self.config, addr)
    }
}

/// Fluent builder for [`Listener`] with chained option setters.
///
/// `ListenerBuilder::new()` starts from libsrt defaults; each setter mutates
/// the underlying [`ListenerConfig`] in place and returns `&mut Self` so calls
/// can be chained. The terminal call is [`ListenerBuilder::bind`], which takes
/// `&self` (clones the inner config) and binds the listener.
///
/// The `&mut self -> &mut Self` shape mirrors [`SocketBuilder`] and translates
/// directly to Kotlin's `apply` scope, Swift's `var b = …; b.x(); b.y();`,
/// Java's chain on a fresh local, and Python's step-wise assignment — see
/// `docs/reference/binding-authors.md`.
#[must_use]
#[derive(Default)]
pub struct ListenerBuilder {
    config: ListenerConfig,
}

impl ListenerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn passphrase(&mut self, p: Passphrase) -> &mut Self {
        self.config.passphrase = Some(p);
        self
    }
    pub fn key_length(&mut self, kl: KeyLength) -> &mut Self {
        self.config.key_length = kl;
        self
    }
    pub fn latency(&mut self, d: Duration) -> &mut Self {
        self.config.latency = Some(d);
        self
    }
    pub fn latency_ms(&mut self, ms: u64) -> &mut Self {
        self.latency(Duration::from_millis(ms))
    }
    pub fn recv_latency(&mut self, d: Duration) -> &mut Self {
        self.config.recv_latency = Some(d);
        self
    }
    pub fn mss(&mut self, mss: u16) -> &mut Self {
        self.config.mss = Some(mss);
        self
    }
    pub fn payload_size(&mut self, n: u16) -> &mut Self {
        self.config.payload_size = Some(n);
        self
    }
    /// See `SocketBuilder::udp_recv_buffer_bytes`.
    pub fn udp_recv_buffer_bytes(&mut self, n: u32) -> &mut Self {
        self.config.udp_recv_buffer_bytes = Some(n);
        self
    }
    pub fn max_bandwidth(&mut self, bw: MaxBandwidth) -> &mut Self {
        self.config.max_bandwidth = Some(bw);
        self
    }
    pub fn overhead_bandwidth_pct(&mut self, pct: u8) -> &mut Self {
        self.config.overhead_bandwidth_pct = Some(pct);
        self
    }
    /// Set the SRT receive buffer size in **bytes** (`SRTO_RCVBUF`).
    ///
    /// Passed verbatim to libsrt. See [`SocketBuilder::recv_buf_bytes`] for
    /// sizing guidance.
    pub fn recv_buf_bytes(&mut self, n: u32) -> &mut Self {
        self.config.recv_buf_bytes = Some(n);
        self
    }
    pub fn loss_max_ttl(&mut self, n: u32) -> &mut Self {
        self.config.loss_max_ttl = Some(n);
        self
    }
    pub fn too_late_packet_drop(&mut self, on: bool) -> &mut Self {
        self.config.too_late_packet_drop = Some(on);
        self
    }
    pub fn flow_window_packets(&mut self, n: u32) -> &mut Self {
        self.config.flow_window_packets = Some(n);
        self
    }
    pub fn packet_filter(&mut self, pf: PacketFilter) -> &mut Self {
        self.config.packet_filter = Some(pf);
        self
    }
    pub fn congestion(&mut self, c: Congestion) -> &mut Self {
        self.config.congestion = Some(c);
        self
    }
    pub fn backlog(&mut self, n: u32) -> &mut Self {
        self.config.backlog = n;
        self
    }
    pub fn reuse_addr(&mut self, on: bool) -> &mut Self {
        self.config.reuse_addr = on;
        self
    }
    pub fn recv_timeout(&mut self, t: Duration) -> &mut Self {
        self.config.recv_timeout = Some(t);
        self
    }
    pub fn send_timeout(&mut self, t: Duration) -> &mut Self {
        self.config.send_timeout = Some(t);
        self
    }
    /// Set `SRTO_LINGER` — drop/close grace period for unsent data.
    /// `Duration::ZERO` closes immediately; libsrt's default is linger off
    /// (`l_onoff=0, l_linger=0`). Inherited by accepted sockets.
    pub fn linger(&mut self, d: Duration) -> &mut Self {
        self.config.linger = Some(d);
        self
    }

    /// Reach the underlying config (for inspection, copying, FFI marshaling).
    /// Clones the inner config so the builder can be reused. Cloning is
    /// cheap — at most two short heap allocations (optional `Passphrase` and
    /// `PacketFilter`, both `String`-backed).
    pub fn config(&self) -> ListenerConfig {
        self.config.clone()
    }

    /// Terminal call: bind, listen, return the Listener.
    ///
    /// Takes `&self` (not `self`) so the builder can be reused; clones the
    /// inner config into [`Listener::bind_with`]. Cloning is cheap — the
    /// config is a flat `ListenerConfig` with at most two short heap
    /// allocations (the optional `Passphrase` and `PacketFilter`, both
    /// `String`-backed).
    ///
    /// # C ABI
    ///
    /// No direct counterpart — `Listener` itself is not exposed at the C
    /// ABI. The C receivers fold the bind/listen/accept sequence into
    /// their `tst_*_receiver_open_listener` calls.
    ///
    /// # Errors
    /// Returns [`BindError`] on hostname-resolution failure, libsrt
    /// option-set failure, or bind/listen failure.
    pub fn bind(&self, addr: impl ToSocketAddrs) -> Result<Listener, BindError> {
        Listener::bind_with(&self.config, addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_builder_chains() {
        let cfg = SocketBuilder::new()
            .latency_ms(120)
            .mss(1316)
            .payload_size(1316)
            .config();

        assert_eq!(cfg.latency, Some(Duration::from_millis(120)));
        assert_eq!(cfg.mss, Some(1316));
        assert_eq!(cfg.payload_size, Some(1316));
    }

    #[test]
    fn listener_builder_chains() {
        let cfg = ListenerBuilder::new()
            .backlog(10)
            .reuse_addr(false)
            .config();

        assert_eq!(cfg.backlog, 10);
        assert!(!cfg.reuse_addr);
    }

    #[test]
    fn try_stream_id_validates() {
        let mut b = SocketBuilder::new();
        assert!(b.try_stream_id("publish:cam1").is_ok());

        let too_long = "a".repeat(513);
        let mut b = SocketBuilder::new();
        assert!(b.try_stream_id(too_long.as_str()).is_err());
    }

    #[test]
    fn builder_sender_defaults_matches_config_sender_defaults() {
        let from_builder = SocketBuilder::new().sender_defaults().config();
        let from_struct = SocketConfig::sender_defaults();
        assert_eq!(from_builder.connect_timeout, from_struct.connect_timeout);
        assert_eq!(from_builder.linger, from_struct.linger);
        assert_eq!(from_builder.role, from_struct.role);
    }

    #[test]
    fn builder_sender_defaults_then_override() {
        // Preset applied first, explicit setter wins (last-write).
        let cfg = SocketBuilder::new()
            .sender_defaults()
            .connect_timeout(Duration::from_secs(7))
            .config();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(cfg.linger, Some(Duration::from_secs(5)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn builder_explicit_then_sender_defaults_preserves_explicit() {
        // Explicit setter first, preset doesn't clobber (merge-if-default).
        let cfg = SocketBuilder::new()
            .connect_timeout(Duration::from_secs(7))
            .sender_defaults()
            .config();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(cfg.linger, Some(Duration::from_secs(5)));
        assert_eq!(cfg.role, Role::Sender);
    }

    #[test]
    fn builder_receiver_defaults_matches_config_receiver_defaults() {
        let from_builder = SocketBuilder::new().receiver_defaults().config();
        let from_struct = SocketConfig::receiver_defaults();
        assert_eq!(from_builder.connect_timeout, from_struct.connect_timeout);
        assert_eq!(from_builder.linger, from_struct.linger);
        assert_eq!(from_builder.role, from_struct.role);
    }

    #[test]
    fn builder_receiver_defaults_then_override() {
        let cfg = SocketBuilder::new()
            .receiver_defaults()
            .connect_timeout(Duration::from_secs(7))
            .config();
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(cfg.role, Role::Receiver);
    }

    #[test]
    fn builder_explicit_sender_then_receiver_defaults_preserves_explicit() {
        // Explicit Role::Sender is not overridden by receiver_defaults.
        let cfg = SocketBuilder::new()
            .role(Role::Sender)
            .receiver_defaults()
            .config();
        assert_eq!(cfg.role, Role::Sender);
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(15)));
    }
}
