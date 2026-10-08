//! Opaque handle types for the RTSP server C ABI.
//!
//! `TstRtspServer` — owns the live `RtspServer` Rust value behind a `Mutex`.
//! `TstRtspMountHandle` — owns a `MountHandle` returned from
//! `RtspServer::add_mount` / `add_multicast_mount`; push methods live in
//! `mount.rs`.
//! `TstRtspPublishMount` — owns a `PublishMountHandle` returned from
//! `RtspServer::add_publish_mount` / `next_publisher`, plus the `repr(C)`
//! snapshot types its getters fill; entry points live in `publish.rs`.
//!
//! Both types are opaque from the C caller's perspective. The naming follows
//! the `tst_rtsp_server_t` / `tst_rtsp_mount_handle_t` C type names emitted
//! by cbindgen.
//!
//! # Lifecycle
//!
//! ```text
//! tst_rtsp_server_builder_new()  →  TstRtspServerBuilder
//!      ↓  (setter calls)
//! tst_rtsp_server_builder_start()  →  TstRtspServer
//!      ↓
//! tst_rtsp_server_add_unicast_mount()    →  TstRtspMountHandle
//! tst_rtsp_server_add_multicast_mount()  →  TstRtspMountHandle
//!      ↓
//! push_video / push_klv / … on TstRtspMountHandle
//!      ↓
//! tst_rtsp_server_stop() / tst_rtsp_server_free()
//! ```

use std::ffi::CString;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Opaque handle for a started RTSP server.
///
/// Obtained from [`super::start::tst_rtsp_server_builder_start`]. Freed (with
/// graceful shutdown) via `tst_rtsp_server_stop` + `tst_rtsp_server_free`,
/// or implicitly via Drop (hard cancel).
///
/// The inner `Mutex<Option<…>>` gives close-idempotence: after `_stop` or
/// `_free` the `Option` is `None` and subsequent calls return `TST_E_CLOSED`.
/// This mirrors the `TstRtspSession` shape used in the client surface.
///
/// The server is held in an `Arc` so the calls that block on it
/// (`tst_rtsp_server_next_publisher`, `tst_rtsp_server_remove_mount`) clone
/// it and release the `Mutex` before they wait: a call parked in
/// `next_publisher` must not hold the lock `tst_rtsp_server_stop` needs to
/// wake it.
pub struct TstRtspServer {
    /// Live Rust `RtspServer`. `None` after a call to `tst_rtsp_server_stop`
    /// or `tst_rtsp_server_free` consumes the value.
    pub(crate) inner: Mutex<Option<Arc<tst_rtp::RtspServer>>>,
    /// Hard-cancel handle. Cloned from the server before inserting into
    /// `inner` so that `tst_rtsp_server_cancel_handle` can fire it
    /// without acquiring the `inner` Mutex.
    pub(crate) cancel: tst_rtp::RtspServerCancelHandle,
}

impl TstRtspServer {
    /// Wrap a `RtspServer` in an opaque handle, extracting the cancel handle.
    pub(crate) fn new(server: tst_rtp::RtspServer) -> Self {
        let cancel = server.cancel_handle();
        Self {
            inner: Mutex::new(Some(Arc::new(server))),
            cancel,
        }
    }
}

/// Opaque handle for an RTSP mount.
///
/// Obtained from [`super::mount::tst_rtsp_server_add_unicast_mount`] or
/// [`super::mount::tst_rtsp_server_add_multicast_mount`]. Push methods
/// (`push_video`, `push_klv`, etc.) live in `mount.rs`. Freed with
/// `tst_rtsp_mount_handle_free`.
///
/// The `MountHandle` returned by the Rust API is `Clone + Send`, so multiple
/// C handles pointing at the same mount are safe — each clone pushes to the
/// same broadcast fanout channel.
///
/// The `cancelled` flag is C-layer-only: `tst_rtsp_mount_cancel` sets it and
/// subsequent push calls return `TST_E_CLOSED` immediately without entering
/// the Rust muxer. This avoids the need for a cancel-token in the Rust
/// `MountHandle` API. Unlike transport-based handles, "cancelling" a mount
/// handle only stops this particular C-side caller; the underlying
/// `tst_rtp::MountHandle` (and any other C clones sharing the same broadcast
/// Arc) continues operating.
pub struct TstRtspMountHandle {
    /// The inner `MountHandle`.
    pub(crate) inner: tst_rtp::MountHandle,
    /// Set by `tst_rtsp_mount_cancel`. Guards all push calls — returns
    /// `TST_E_CLOSED` when true. Stored here (not in `MountState`) so that
    /// multiple independent C-side mount handles can have independent cancel
    /// states. Safe to read without the muxer lock because it is checked
    /// before the push path acquires any lock.
    pub(crate) cancelled: AtomicBool,
}

/// Opaque handle for an RTSP publish mount (the `tst_rtsp_publish_mount_t`
/// C type).
///
/// Obtained from `tst_rtsp_server_add_publish_mount` or
/// `tst_rtsp_server_next_publisher`; freed with
/// `tst_rtsp_publish_mount_free`. The mount itself lives in the server:
/// freeing this handle never removes or closes it
/// (`tst_rtsp_server_remove_mount` does).
pub struct TstRtspPublishMount {
    /// The inner `PublishMountHandle`. Cloning it is cheap; its application
    /// transport is take-once across every clone.
    pub(crate) inner: tst_rtp::PublishMountHandle,
    /// The mount path as a C string, borrowed out by
    /// `tst_rtsp_publish_mount_path` for the life of this handle.
    pub(crate) path_c: CString,
}

/// Wire shape a publisher announced (`tst_rtsp_publish_shape`). Mirrors
/// `tst_rtp::PublishShape`; the elementary shape's KLV flag travels in
/// `tst_rtsp_publisher_info_t.klv`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TstRtspPublishShape {
    /// One MPEG-TS-over-RTP track (RFC 2250, `MP2T/90000`).
    Mp2t = 0,
    /// Elementary tracks: one H.264 (RFC 6184) video track, optionally one
    /// KLV (RFC 6597) track.
    Elementary = 1,
}

/// How a publish mount aligns its announced tracks to one clock
/// (`tst_rtsp_clock_alignment`). Mirrors `tst_rtp::ClockAlignment`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TstRtspClockAlignment {
    /// Nothing to align: an MP2T or video-only publisher, or no publisher
    /// has announced yet.
    NotApplicable = 0,
    /// A KLV track was announced but its clock is not yet related to the
    /// video clock; KLV units are held until it is.
    Pending = 1,
    /// Tracks aligned by first-packet coincidence (RTCP sender reports did
    /// not arrive in time).
    Provisional = 2,
    /// Tracks aligned through RTCP sender reports (RFC 3550 section 6.4.1).
    SenderReport = 3,
}

/// Per-publish-mount stats snapshot (`tst_rtsp_publish_mount_stats_t`),
/// filled by `tst_rtsp_publish_mount_get_stats`. Every field of
/// `tst_rtp::PublishMountStats`, in its order; counters are cumulative over
/// the mount's life, across publishers. Size 136 B.
///
/// - `rtp_packets_received` / `bytes_received`: RTP packets from publishers
///   (headers included in the bytes), counted before validation.
/// - `malformed_packets`: dropped as unusable (not RTP, wrong payload type,
///   invalid MP2T payload, unknown interleaved channel).
/// - `source_rejected`: UDP datagrams from an IP other than the publisher's
///   control connection.
/// - `frames_emitted`: frames emitted to PLAY readers and the application
///   transport.
/// - `frames_dropped_app`: frames dropped because the application
///   transport's queue was full (the application stopped reading).
/// - `frames_dropped_readers`: frames dropped across lagging PLAY readers.
/// - `aus_emitted` / `aus_dropped` / `aus_reordered`: access units of an
///   elementary publisher muxed, dropped, and muxed with a PTS below an
///   earlier one (a publisher sending B-frames).
/// - `klv_units_emitted` / `klv_units_dropped`: KLV units of an elementary
///   publisher.
/// - `alignment`: how the current publisher's tracks are aligned.
/// - `alignment_steps`: times a new clock mapping replaced the previous one.
/// - `ssrc_changes`: source restarts on an elementary publisher's tracks.
/// - `generation`: publishers that have ended on this mount.
/// - `peer_count`: live PLAY readers on the mount.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TstRtspPublishMountStats {
    pub rtp_packets_received: u64,
    pub bytes_received: u64,
    pub malformed_packets: u64,
    pub source_rejected: u64,
    pub frames_emitted: u64,
    pub frames_dropped_app: u64,
    pub frames_dropped_readers: u64,
    pub aus_emitted: u64,
    pub aus_dropped: u64,
    pub aus_reordered: u64,
    pub klv_units_emitted: u64,
    pub klv_units_dropped: u64,
    pub alignment: TstRtspClockAlignment,
    pub alignment_steps: u64,
    pub ssrc_changes: u64,
    pub generation: u64,
    pub peer_count: u64,
}

impl Default for TstRtspPublishMountStats {
    fn default() -> Self {
        Self {
            rtp_packets_received: 0,
            bytes_received: 0,
            malformed_packets: 0,
            source_rejected: 0,
            frames_emitted: 0,
            frames_dropped_app: 0,
            frames_dropped_readers: 0,
            aus_emitted: 0,
            aus_dropped: 0,
            aus_reordered: 0,
            klv_units_emitted: 0,
            klv_units_dropped: 0,
            alignment: TstRtspClockAlignment::NotApplicable,
            alignment_steps: 0,
            ssrc_changes: 0,
            generation: 0,
            peer_count: 0,
        }
    }
}

const _TST_RTSP_PUBLISH_MOUNT_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstRtspPublishMountStats>() == 136,
    "TstRtspPublishMountStats must be 136 bytes (16 x u64 + a 4-byte enum padded to 8)"
);

/// Capacity of `tst_rtsp_publisher_info_t.peer`, NUL included.
pub const TST_RTSP_PEER_ADDR_LEN: usize = 64;

/// The publisher holding a publish mount (`tst_rtsp_publisher_info_t`),
/// filled by `tst_rtsp_publish_mount_publisher_info`.
///
/// - `present`: false when no publisher holds the mount; every other field
///   is then zero.
/// - `shape` / `klv`: the wire shape the publisher announced; `klv` is true
///   for an elementary publisher that announced a KLV track.
/// - `generation`: the mount's publisher generation while this publisher
///   holds it.
/// - `since_unix_ms`: when the publisher's ANNOUNCE claimed the mount, in
///   milliseconds since the Unix epoch.
/// - `peer`: the publisher's control-connection address
///   (`ip:port`, `[v6]:port`), NUL-terminated, truncated to fit.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TstRtspPublisherInfo {
    pub present: bool,
    pub shape: TstRtspPublishShape,
    pub klv: bool,
    pub generation: u64,
    pub since_unix_ms: u64,
    pub peer: [core::ffi::c_char; TST_RTSP_PEER_ADDR_LEN],
}

impl Default for TstRtspPublisherInfo {
    fn default() -> Self {
        Self {
            present: false,
            shape: TstRtspPublishShape::Mp2t,
            klv: false,
            generation: 0,
            since_unix_ms: 0,
            peer: [0; TST_RTSP_PEER_ADDR_LEN],
        }
    }
}

const _TST_RTSP_PUBLISHER_INFO_SIZE: () = assert!(
    core::mem::size_of::<TstRtspPublisherInfo>() == 96,
    "TstRtspPublisherInfo must be 96 bytes"
);
