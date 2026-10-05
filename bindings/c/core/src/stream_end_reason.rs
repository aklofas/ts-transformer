//! `TstStreamEndReason` — shared C stream-end-reason enum, one mirror per
//! transport's own Rust-side reason type: `tst_rtp::StreamEndReason` for
//! the RTP/RTSP receive surface (`crate::rtp::end_reason`) and
//! `tst_pipeline::RecvEndReason` for the SRT managed-demux-receiver
//! surface (`crate::receiver::demux_receiver::managed`).
//!
//! Defined here, outside either feature-gated module, so cbindgen emits
//! it **unconditionally** — same reasoning as the sibling
//! `TstReconnectMode` in `config::builders`: the only feature-specific
//! pieces are the getters that read it (`tst_rtp_receiver_end_reason` /
//! `tst_rtp_demux_receiver_end_reason`, both `TST_HAS_RTP`-gated in
//! `crate::rtp`; `tst_managed_demux_receiver_end_reason`, `TST_HAS_SRT`-gated
//! in `crate::receiver`), not the enum shape itself. Keeping the enum's
//! own definition inside a feature-gated module would make cbindgen wrap
//! every variant in its own `#if defined(...)` — the doubled-guard shape
//! the project deliberately avoids (see
//! `crate::config::builders::TstReconnectMode` for the same pattern and
//! `scripts/check/c/header-conditional-sections.sh`, which enforces
//! module-level-cfg-only gating).
//!
//! Conversion from each transport's own reason type lives with that
//! transport: `crate::rtp::end_reason` for RTP, and
//! `crate::receiver::demux_receiver::managed::convert_recv_end_reason`
//! for the SRT managed receiver.

/// Why a receive session ended. One shared enum, reused by every
/// transport's own end-reason getter — see the getters' own docs
/// (`tst_rtp_receiver_end_reason` / `tst_rtp_demux_receiver_end_reason` /
/// `tst_managed_demux_receiver_end_reason`) for which reasons each
/// transport can actually produce. `None` (0) — "hasn't ended yet, or
/// ended through an uninstrumented path" — is common to all
/// of them (the case each side's own end-reason handle reports as
/// `Option::None`, e.g. a plain `rtp://` receiver that was never
/// `_cancel`'d or `_close`'d). Some variants are transport-specific: the
/// RTSP-shaped `SessionExpired` / `KeepaliveFailed` / `ProtocolError`
/// only ever surface from the RTP/RTSP side — the SRT managed receiver's
/// `RecvEndReason` has no equivalent and never produces them.
/// Discriminants 1-6 are cross-surface stable — the Python and JVM
/// bindings use the same numbering.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstStreamEndReason {
    /// The session hasn't ended yet, or ended through an uninstrumented
    /// path.
    None = 0,
    /// The peer closed the connection in an orderly way, with no
    /// protocol or transport error.
    CleanTeardown = 1,
    /// The server no longer honors the session — a keepalive ping was
    /// answered `454 Session Not Found`.
    SessionExpired = 2,
    /// The keepalive background thread failed to encode or send a ping.
    /// Detail message: see the getter doc.
    KeepaliveFailed = 3,
    /// A hard I/O error on the underlying transport. Detail message:
    /// see the getter doc.
    TransportFailed = 4,
    /// The peer violated the wire protocol. Detail message: see the
    /// getter doc.
    ProtocolError = 5,
    /// The caller explicitly cancelled or closed the transport — not a
    /// wire-level failure.
    Cancelled = 6,
}
