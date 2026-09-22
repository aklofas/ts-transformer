//! `tst_rtsp_server_add_unicast_mount` and `tst_rtsp_server_add_multicast_mount`.
//!
//! Both entry points register a new mount on a started `TstRtspServer` and
//! return an opaque `TstRtspMountHandle*`. Push methods on the mount handle
//! live in this file, below; server-level stats/stop/free live in `stop.rs`.
//!
//! # Mount registration pattern
//!
//! ```text
//! tst_rtsp_server_add_unicast_mount(server, path, mux_cfg)
//!     │
//!     ├─ validate inputs (null check, UTF-8 decode)
//!     ├─ build MuxerConfig from TstMuxConfig  ← same as tst_rtp_mux_sender_open
//!     ├─ call RtspServer::add_mount(path, cfg)  ← inside inner Mutex
//!     └─ return Box<TstRtspMountHandle>::into_raw(…)
//!
//! tst_rtsp_server_add_multicast_mount(server, path, group, ttl, iface, mux_cfg)
//!     │
//!     ├─ validate inputs
//!     ├─ build group_url string: "rtp://<group>:<port>?ttl=N[&iface=X]"
//!     │   (only ttl and optional iface needed; port is embedded in group arg)
//!     ├─ build MuxerConfig
//!     ├─ call RtspServer::add_multicast_mount(path, cfg, group_url)
//!     └─ return Box<TstRtspMountHandle>::into_raw(…)
//! ```
//!
//! # Group URL format for multicast
//!
//! `RtspServer::add_multicast_mount` expects a `group_url` of the form
//! `rtp://<multicast-ip>:<port>[?ttl=N&iface=name_or_ip]`. The C entry point
//! accepts `group` (NUL-terminated, must be `<ip>:<port>` like
//! `"239.0.0.1:5004"`), `ttl` (u8), and `iface_name` (NUL-terminated, may
//! be NULL for default). The group URL string is assembled here and forwarded
//! to `add_multicast_mount`.
//!
//! # Error mapping
//!
//! - `RtspServerError` variants → `TST_E_RTSP_SERVER` (-24) via
//!   [`crate::error::record_with_context`] (the shared `From<RtspServerError>`
//!   row of the binding kind table).
//! - `MuxError` from config validation → mapped via
//!   [`crate::error::record_mux_error`].

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};

use crate::config::TstMuxConfig;
use crate::error::{TstError, record_mux_error, set_last_error};
use crate::handle::{
    TstAudioStreamHandle, TstDataStreamHandle, TstKlvStreamHandle, TstSubtitleStreamHandle,
    TstVideoStreamHandle,
};
use crate::panic::ffi_catch;
use crate::rtsp::server::types::{TstRtspMountHandle, TstRtspServer};

/// Register a **unicast** mount on a started RTSP server.
///
/// After a successful call, connecting clients that send an RTSP SETUP
/// request to `path` will be assigned individual UDP or TCP-interleaved
/// transports. Each client's RTP stream is fed from the same broadcast
/// fanout channel that the returned [`TstRtspMountHandle`] writes into.
///
/// `path` must:
/// - Be a NUL-terminated UTF-8 string.
/// - Start with `/` (e.g. `"/live"`).
/// - Not contain URL-reserved characters like `?` or `#`.
/// - Not duplicate a path already registered on this server.
///
/// `mux_cfg` must be a valid `tst_mux_config_t` (see `tst_mux_config_new`
/// / `tst_mux_config_add_program`). It is borrowed for this call — the
/// caller still owns it and must free it. The returned handle is independent
/// of the config after this call.
///
/// Returns a non-NULL `tst_rtsp_mount_handle_t*` on success, NULL on
/// failure with last-error set. The handle must eventually be freed with
/// `tst_rtsp_mount_handle_free`.
///
/// # Safety
///
/// - `server` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_builder_start`.
/// - `path` must be a NUL-terminated C string valid for this call.
/// - `mux_cfg` must be a non-NULL pointer from `tst_mux_config_new`,
///   valid for this call (caller retains ownership).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_add_unicast_mount(
    server: *mut TstRtspServer,
    path: *const c_char,
    mux_cfg: *const TstMuxConfig,
) -> *mut TstRtspMountHandle {
    ffi_catch(std::ptr::null_mut(), || {
        // Validate server pointer.
        let Some(handle) = (unsafe { server.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "server is null");
            return std::ptr::null_mut();
        };

        // Decode the mount path.
        if path.is_null() {
            set_last_error(TstError::InvalidConfig, "path is null");
            return std::ptr::null_mut();
        }
        let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(TstError::InvalidConfig, "path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };

        // Build MuxerConfig from the TstMuxConfig builder.
        let cfg_ref = match unsafe { mux_cfg.as_ref() } {
            Some(c) => c,
            None => {
                set_last_error(TstError::InvalidConfig, "mux_cfg is null");
                return std::ptr::null_mut();
            }
        };
        let muxer_cfg = match cfg_ref.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };

        // Lock the server and call add_mount.
        let guard = match handle.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "server mutex poisoned");
                return std::ptr::null_mut();
            }
        };
        let server_ref = match guard.as_ref() {
            Some(s) => s,
            None => {
                set_last_error(TstError::Closed, "server is stopped or freed");
                return std::ptr::null_mut();
            }
        };

        match server_ref.add_mount(path_str, muxer_cfg) {
            Ok(mount) => Box::into_raw(Box::new(TstRtspMountHandle {
                inner: mount,
                cancelled: AtomicBool::new(false),
            })),
            Err(e) => {
                crate::error::record_with_context(e, "add_unicast_mount failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Register a **multicast** mount on a started RTSP server.
///
/// After a successful call, the server spawns a background task that drains
/// the mount's broadcast channel and sends RTP packets to the multicast
/// `group` address. Connecting clients that SETUP against `path` receive the
/// same group address, TTL, and optional interface in the `Transport:` header
/// so they can join the group and receive the shared stream.
///
/// `group` must be a NUL-terminated `<ip>:<port>` string (IPv4 or IPv6
/// multicast address with port), e.g. `"239.0.0.1:5004"`. The address must
/// be in a multicast range (IPv4 `224.0.0.0/4`, IPv6 `ff00::/8`). Port must
/// be included.
///
/// `ttl` is the IP multicast TTL / hop limit (1–255). Typical LAN values:
/// 1 = link-local, 8 = site-local (RTP convention).
///
/// `iface_name` is a NUL-terminated string identifying the outbound interface
/// for multicast send — either an IPv4 literal (e.g. `"192.168.1.50"`) or an
/// interface name where supported. Pass NULL to let the OS select the default
/// multicast interface.
///
/// `mux_cfg` is borrowed — the caller retains ownership and must free it.
///
/// Returns a non-NULL `tst_rtsp_mount_handle_t*` on success, NULL on failure
/// with last-error set. NULL for `iface_name` is valid (no interface
/// override).
///
/// # Safety
///
/// - `server` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_builder_start`.
/// - `path` must be a NUL-terminated C string valid for this call.
/// - `group` must be a NUL-terminated `<ip>:<port>` string.
/// - `iface_name` may be NULL or a NUL-terminated interface string.
/// - `mux_cfg` must be a non-NULL pointer from `tst_mux_config_new`,
///   valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_add_multicast_mount(
    server: *mut TstRtspServer,
    path: *const c_char,
    group: *const c_char,
    ttl: u8,
    iface_name: *const c_char,
    mux_cfg: *const TstMuxConfig,
) -> *mut TstRtspMountHandle {
    ffi_catch(std::ptr::null_mut(), || {
        // Validate server pointer.
        let Some(handle) = (unsafe { server.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "server is null");
            return std::ptr::null_mut();
        };

        // Decode mount path.
        if path.is_null() {
            set_last_error(TstError::InvalidConfig, "path is null");
            return std::ptr::null_mut();
        }
        let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(TstError::InvalidConfig, "path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };

        // Decode group address.
        if group.is_null() {
            set_last_error(TstError::InvalidConfig, "group is null");
            return std::ptr::null_mut();
        }
        let group_str = match unsafe { CStr::from_ptr(group) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(TstError::InvalidConfig, "group is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };

        // Decode optional iface_name.
        let iface_str: Option<&str> = if iface_name.is_null() {
            None
        } else {
            match unsafe { CStr::from_ptr(iface_name) }.to_str() {
                Ok(s) => Some(s),
                Err(_) => {
                    set_last_error(TstError::InvalidConfig, "iface_name is not valid UTF-8");
                    return std::ptr::null_mut();
                }
            }
        };

        // Assemble the group URL that RtspServer::add_multicast_mount expects:
        // "rtp://<group_str>[?ttl=N][&iface=X]"
        // group_str is already "<ip>:<port>", so we prepend "rtp://".
        let mut group_url = format!("rtp://{group_str}?ttl={ttl}");
        if let Some(iface) = iface_str {
            group_url.push_str("&iface=");
            group_url.push_str(iface);
        }

        // Build MuxerConfig.
        let cfg_ref = match unsafe { mux_cfg.as_ref() } {
            Some(c) => c,
            None => {
                set_last_error(TstError::InvalidConfig, "mux_cfg is null");
                return std::ptr::null_mut();
            }
        };
        let muxer_cfg = match cfg_ref.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };

        // Lock the server and call add_multicast_mount.
        let guard = match handle.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "server mutex poisoned");
                return std::ptr::null_mut();
            }
        };
        let server_ref = match guard.as_ref() {
            Some(s) => s,
            None => {
                set_last_error(TstError::Closed, "server is stopped or freed");
                return std::ptr::null_mut();
            }
        };

        match server_ref.add_multicast_mount(path_str, muxer_cfg, &group_url) {
            Ok(mount) => Box::into_raw(Box::new(TstRtspMountHandle {
                inner: mount,
                cancelled: AtomicBool::new(false),
            })),
            Err(e) => {
                crate::error::record_with_context(e, "add_multicast_mount failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Free an RTSP mount handle.
///
/// Drops the `TstRtspMountHandle` and its inner `MountHandle`. After this
/// call the pointer is invalid; any further use is undefined behavior. NULL
/// is a no-op.
///
/// Push methods on a freed handle are not safe — the caller must not call
/// any `tst_rtsp_mount_handle_*` push method after `_free`.
///
/// # Safety
///
/// `handle` must be NULL, or a pointer returned by
/// `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`
/// that has not yet been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_handle_free(handle: *mut TstRtspMountHandle) {
    ffi_catch((), || {
        if handle.is_null() {
            return;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let _ = unsafe { Box::from_raw(handle) };
        // Box drops at end of scope, running MountHandle's Drop.
    });
}

// ---------------------------------------------------------------------------
// Push — single-stream variants
// ---------------------------------------------------------------------------

/// Push one Annex-B NAL through the mount's single video stream and out the
/// RTSP broadcast fanout (single-stream shorthand).
///
/// `nal` must point to `len` bytes of Annex-B NAL data (one or more NAL
/// units with 4-byte or 3-byte start codes). `pts_90khz` is the
/// presentation timestamp in 90 kHz ticks. `key_frame` is `true` for IDR
/// / random-access frames.
///
/// Resolves only when exactly one video stream is configured; rejects with
/// `TST_E_RTSP_MOUNT` (`MuxError::AmbiguousTarget`) if more than one video
/// stream is present — use `tst_rtsp_mount_push_video_to` in that case.
///
/// Returns `0` on success, `TST_E_CLOSED` after `tst_rtsp_mount_cancel`,
/// `TST_E_RTSP_MOUNT` on muxer or mount errors, `TST_E_INVALID_CONFIG` if
/// `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `nal` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_video(
    handle: *mut TstRtspMountHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_video(slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_video failed"),
        }
    })
}

/// Push one raw KLV blob through the mount's single KLV stream (single-stream
/// shorthand).
///
/// `klv` must point to **raw MISB Local Set bytes**. For streams configured
/// as `TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA`, the muxer prepends a
/// 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 §2.12.4.2.
/// **Do not pre-wrap the AU cell on the caller side.** The current API uses
/// `metadata_service_id = 0x00` per ST 1402.2 App. B Table 2.
///
/// Returns `0` on success, `TST_E_CLOSED` after `tst_rtsp_mount_cancel`,
/// `TST_E_RTSP_MOUNT` on muxer or mount errors, `TST_E_INVALID_CONFIG` if
/// `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `klv` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_klv(
    handle: *mut TstRtspMountHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_klv(
            slice, pts,
            // C ABI exposes metadata_service_id via future entry; today
            // defaults to 0x00 per ST 1402.2 App. B Table 2.
            0x00,
        ) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_klv failed"),
        }
    })
}

/// Push one audio frame buffer through the mount's single audio stream
/// (single-stream shorthand).
///
/// `frames` must point to `len` bytes of pre-framed audio data (one or more
/// ADTS or MPEG audio frames concatenated). `pts_90khz` is the presentation
/// timestamp in 90 kHz ticks.
///
/// Returns `0` on success, `TST_E_CLOSED` after `tst_rtsp_mount_cancel`,
/// `TST_E_RTSP_MOUNT` on muxer or mount errors, `TST_E_INVALID_CONFIG` if
/// `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `frames` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_audio(
    handle: *mut TstRtspMountHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_audio(slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_audio failed"),
        }
    })
}

/// Push one subtitle payload through the mount's single subtitle stream
/// (single-stream shorthand).
///
/// `payload` is one complete logical subtitle unit (DVB-sub composition page,
/// teletext data field, CEA-708 service block, or WebVTT cue). `pts_90khz`
/// is the presentation timestamp in 90 kHz ticks.
///
/// Returns `0` on success, `TST_E_CLOSED` after `tst_rtsp_mount_cancel`,
/// `TST_E_RTSP_MOUNT` on muxer or mount errors, `TST_E_INVALID_CONFIG` if
/// `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `payload` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_subtitle(
    handle: *mut TstRtspMountHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_subtitle(slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_subtitle failed"),
        }
    })
}

/// Push one data payload through the mount's single data stream and out the
/// RTSP broadcast fanout (single-stream shorthand).
///
/// Pass-through: `data` lands verbatim as one PES packet on `stream_id`
/// 0xBD. PTS is written only for `carries_pts = true` streams.
///
/// Returns `0` on success, `TST_E_CLOSED` after `tst_rtsp_mount_cancel`,
/// `TST_E_RTSP_MOUNT` on muxer or mount errors, `TST_E_INVALID_CONFIG` if
/// `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `data` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_data(
    handle: *mut TstRtspMountHandle,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_data(slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_data failed"),
        }
    })
}

// ---------------------------------------------------------------------------
// Push — multi-stream (_to) variants
// ---------------------------------------------------------------------------

/// Push one Annex-B NAL targeting a specific video elementary stream.
///
/// `stream_handle` is obtained from `tst_mux_config_add_video_stream` at
/// config time and is stable. Out-of-range handles surface as
/// `TST_E_RTSP_MOUNT` (wrapping `MuxError::InvalidStreamHandle`).
///
/// On a single-stream mount, prefer `tst_rtsp_mount_push_video` — same
/// effect, no handle required.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `nal` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_video_to(
    handle: *mut TstRtspMountHandle,
    stream_handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        // Trust-boundary validation — a forged handle value would silently
        // route to the wrong elementary stream without this guard.
        let stream = match VideoStreamHandle::try_from_raw(stream_handle) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_video_to(stream, slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_video_to failed"),
        }
    })
}

/// Push one raw KLV blob targeting a specific KLV elementary stream.
///
/// For `KlvStreamType::SynchronousMetadata` streams the muxer auto-wraps the
/// caller's bytes in a `Metadata_AU_cell` header per ITU-T H.222.0 V9
/// §2.12.4.2. **Do not pre-wrap on the caller side.**
///
/// On a single-stream mount, prefer `tst_rtsp_mount_push_klv`.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `klv` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_klv_to(
    handle: *mut TstRtspMountHandle,
    stream_handle: TstKlvStreamHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        // Trust-boundary validation — see push_video_to rationale above.
        let stream = match KlvStreamHandle::try_from_raw(stream_handle) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_klv_to(
            stream, slice, pts,
            // C ABI exposes metadata_service_id via future entry; today
            // defaults to 0x00 per ST 1402.2 App. B Table 2.
            0x00,
        ) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_klv_to failed"),
        }
    })
}

/// Push one audio frame buffer targeting a specific audio elementary stream.
///
/// On a single-stream mount, prefer `tst_rtsp_mount_push_audio`.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `frames` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_audio_to(
    handle: *mut TstRtspMountHandle,
    stream_handle: TstAudioStreamHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        // Trust-boundary validation — see push_video_to rationale above.
        let stream = match AudioStreamHandle::try_from_raw(stream_handle) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_audio_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_audio_to failed"),
        }
    })
}

/// Push one subtitle payload targeting a specific subtitle elementary stream.
///
/// On a single-stream mount, prefer `tst_rtsp_mount_push_subtitle`.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `payload` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_subtitle_to(
    handle: *mut TstRtspMountHandle,
    stream_handle: TstSubtitleStreamHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        // Trust-boundary validation — see push_video_to rationale above.
        let stream = match SubtitleStreamHandle::try_from_raw(stream_handle) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_subtitle_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_subtitle_to failed"),
        }
    })
}

/// Push one data payload targeting a specific data elementary stream.
///
/// On a single-stream mount, prefer `tst_rtsp_mount_push_data`.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
/// `data` must be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_push_data_to(
    handle: *mut TstRtspMountHandle,
    stream_handle: TstDataStreamHandle,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        if h.cancelled.load(Ordering::Acquire) {
            set_last_error(TstError::Closed, "mount handle has been cancelled");
            return TstError::Closed as i32;
        }
        let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        // Trust-boundary validation — see push_video_to rationale above.
        let stream = match DataStreamHandle::try_from_raw(stream_handle) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        let pts = Pts90khz::new(pts_90khz);
        match h.inner.push_data_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => crate::error::record_with_context(e, "push_data_to failed"),
        }
    })
}

// ---------------------------------------------------------------------------
// Lifecycle helpers — flush, cancel, reset_stats
// ---------------------------------------------------------------------------

/// Drain any TS packets buffered in the mount's inner muxer and broadcast
/// them through the mount's fanout channel.
///
/// Call after finishing a batch of `push_*` calls to ensure all queued TS
/// output is flushed to active subscribers. No-op when the muxer has no
/// pending output. Safe to call concurrently with other `push_*` calls on
/// separate threads; each call acquires the inner muxer mutex independently.
///
/// Returns `0` on success, `TST_E_INVALID_CONFIG` if `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_flush(handle: *mut TstRtspMountHandle) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        h.inner.flush();
        0
    })
}

/// Cancel a mount handle. All subsequent `push_*` calls on this handle will
/// return `TST_E_CLOSED` immediately without entering the muxer.
///
/// Cancellation is handle-local — other `tst_rtsp_mount_handle_t` pointers
/// to the same mount (e.g. obtained from separate `add_*_mount` calls on the
/// same mount path) are unaffected. The underlying broadcast fanout and inner
/// muxer continue operating for other holders.
///
/// Safe to call from any thread. Idempotent — calling twice is harmless.
/// The handle must still be freed via `tst_rtsp_mount_handle_free` to
/// release memory.
///
/// Returns `0` on success, `TST_E_INVALID_CONFIG` if `handle` is null.
///
/// # Safety
///
/// `handle` must be NULL or a valid non-freed `*mut tst_rtsp_mount_handle_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_cancel(handle: *mut TstRtspMountHandle) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        // Side-channel: do NOT acquire the muxer lock (a concurrent push holds
        // it). The cancelled flag is accessible without locking; it is checked
        // by all push methods before they acquire the muxer mutex.
        h.cancelled.store(true, Ordering::Release);
        0
    })
}

/// Reset all flow counters on the mount to zero.
///
/// Clears both the mount-level accumulators (`bytes_pushed`,
/// `packets_pushed`, `frames_dropped_total`) and the inner muxer's
/// per-stream counters. Per-stream entries are preserved; only the flow
/// counters inside them are zeroed. Same semantics as
/// `tst_mux_sender_reset_stats`.
///
/// Returns `0` on success, `TST_E_INVALID_CONFIG` if `handle` is null.
///
/// # Safety
///
/// `handle` must be a valid non-freed `*mut tst_rtsp_mount_handle_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_reset_stats(
    handle: *mut TstRtspMountHandle,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mount handle pointer");
            return TstError::InvalidConfig as i32;
        };
        h.inner.reset_stats();
        0
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::TstRtspServerBuilder;

    /// Helper: start a server on 127.0.0.1:0 and return the raw handle.
    fn start_test_server() -> *mut TstRtspServer {
        let b = TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0").expect("test url parses");
        let raw_b = TstRtspServerBuilder::into_raw(Box::new(b));
        unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(raw_b) }
    }

    /// Helper: build a minimal TstMuxConfig with one H.264 video stream.
    fn make_mux_cfg() -> *mut TstMuxConfig {
        use crate::config::streams::TstVideoCodec;
        unsafe {
            let p = crate::config::tst_mux_config_new();
            let prog = crate::config::tst_mux_config_add_program(p, 1, 0x1000);
            let _v = crate::config::streams::tst_mux_config_add_video_stream(
                p,
                prog,
                0x1011,
                TstVideoCodec::H264,
            );
            p
        }
    }

    #[test]
    fn null_server_add_unicast_returns_null() {
        let cfg = make_mux_cfg();
        let path = std::ffi::CString::new("/live").unwrap();
        let h = unsafe {
            tst_rtsp_server_add_unicast_mount(std::ptr::null_mut(), path.as_ptr(), cfg as *const _)
        };
        assert!(h.is_null());
        unsafe { crate::config::tst_mux_config_free(cfg) };
    }

    #[test]
    fn null_path_add_unicast_returns_null() {
        let server = start_test_server();
        assert!(!server.is_null());
        let cfg = make_mux_cfg();
        let h =
            unsafe { tst_rtsp_server_add_unicast_mount(server, std::ptr::null(), cfg as *const _) };
        assert!(h.is_null());
        unsafe { crate::config::tst_mux_config_free(cfg) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn null_mux_cfg_add_unicast_returns_null() {
        let server = start_test_server();
        assert!(!server.is_null());
        let path = std::ffi::CString::new("/live").unwrap();
        let h =
            unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), std::ptr::null()) };
        assert!(h.is_null());
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn valid_unicast_mount_returns_non_null_handle() {
        let server = start_test_server();
        assert!(!server.is_null());
        let cfg = make_mux_cfg();
        let path = std::ffi::CString::new("/live").unwrap();
        let mount =
            unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg as *const _) };
        assert!(
            !mount.is_null(),
            "unicast mount returned null; error: {}",
            {
                let s = unsafe { std::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) };
                s.to_str().unwrap_or("<invalid utf8>")
            }
        );
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::config::tst_mux_config_free(cfg) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn null_server_add_multicast_returns_null() {
        let cfg = make_mux_cfg();
        let path = std::ffi::CString::new("/mc").unwrap();
        let group = std::ffi::CString::new("239.0.0.1:5004").unwrap();
        let h = unsafe {
            tst_rtsp_server_add_multicast_mount(
                std::ptr::null_mut(),
                path.as_ptr(),
                group.as_ptr(),
                8,
                std::ptr::null(),
                cfg as *const _,
            )
        };
        assert!(h.is_null());
        unsafe { crate::config::tst_mux_config_free(cfg) };
    }

    #[test]
    fn valid_multicast_mount_returns_non_null_handle() {
        let server = start_test_server();
        assert!(!server.is_null());
        let cfg = make_mux_cfg();
        let path = std::ffi::CString::new("/mc").unwrap();
        let group = std::ffi::CString::new("239.0.0.1:5004").unwrap();
        let mount = unsafe {
            tst_rtsp_server_add_multicast_mount(
                server,
                path.as_ptr(),
                group.as_ptr(),
                8,
                std::ptr::null(),
                cfg as *const _,
            )
        };
        assert!(
            !mount.is_null(),
            "multicast mount returned null; error: {}",
            {
                let s = unsafe { std::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) };
                s.to_str().unwrap_or("<invalid utf8>")
            }
        );
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::config::tst_mux_config_free(cfg) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn unicast_null_is_noop() {
        unsafe { tst_rtsp_mount_handle_free(std::ptr::null_mut()) };
    }

    // ── Push methods — null-pointer guard tests ──────────────────────────

    #[test]
    fn push_video_null_handle_returns_invalid_config() {
        let rc = unsafe {
            tst_rtsp_mount_push_video(
                std::ptr::null_mut(),
                [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB].as_ptr(),
                6,
                0,
                true,
            )
        };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn push_klv_null_handle_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_mount_push_klv(std::ptr::null_mut(), [0u8; 4].as_ptr(), 4, 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn push_audio_null_handle_returns_invalid_config() {
        let rc =
            unsafe { tst_rtsp_mount_push_audio(std::ptr::null_mut(), [0u8; 4].as_ptr(), 4, 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn push_subtitle_null_handle_returns_invalid_config() {
        let rc =
            unsafe { tst_rtsp_mount_push_subtitle(std::ptr::null_mut(), [0u8; 4].as_ptr(), 4, 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn push_data_null_handle_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_mount_push_data(std::ptr::null_mut(), [0u8; 4].as_ptr(), 4, 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn push_data_to_null_handle_returns_invalid_config() {
        let rc = unsafe {
            tst_rtsp_mount_push_data_to(std::ptr::null_mut(), 0, [0u8; 4].as_ptr(), 4, 0)
        };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn flush_null_handle_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_mount_flush(std::ptr::null_mut()) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn cancel_null_handle_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_mount_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn reset_stats_null_handle_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_mount_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    // ── Push methods — end-to-end via a live unicast mount ───────────────

    /// Helper: open a unicast mount and return the handle (raw pointer).
    fn open_unicast_mount() -> (*mut TstRtspServer, *mut TstRtspMountHandle) {
        let server = start_test_server();
        assert!(!server.is_null());
        let cfg = make_mux_cfg();
        let path = std::ffi::CString::new("/live").unwrap();
        let mount =
            unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg as *const _) };
        assert!(!mount.is_null(), "unicast mount should not be null");
        unsafe { crate::config::tst_mux_config_free(cfg) };
        (server, mount)
    }

    #[test]
    fn push_video_on_valid_mount_succeeds() {
        let (server, mount) = open_unicast_mount();
        // Minimal Annex-B IDR: 4-byte start + NAL type 5.
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        let rc = unsafe { tst_rtsp_mount_push_video(mount, nal.as_ptr(), nal.len(), 0, true) };
        assert_eq!(rc, 0, "push_video should succeed on a fresh unicast mount");
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn cancel_then_push_video_returns_closed() {
        let (server, mount) = open_unicast_mount();
        let rc_cancel = unsafe { tst_rtsp_mount_cancel(mount) };
        assert_eq!(rc_cancel, 0, "cancel should return 0");
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        let rc_push = unsafe { tst_rtsp_mount_push_video(mount, nal.as_ptr(), nal.len(), 0, true) };
        assert_eq!(
            rc_push,
            crate::error::TstError::Closed as i32,
            "push after cancel should return TST_E_CLOSED"
        );
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn flush_on_valid_mount_returns_zero() {
        let (server, mount) = open_unicast_mount();
        let rc = unsafe { tst_rtsp_mount_flush(mount) };
        assert_eq!(rc, 0);
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn reset_stats_on_valid_mount_returns_zero() {
        let (server, mount) = open_unicast_mount();
        let rc = unsafe { tst_rtsp_mount_reset_stats(mount) };
        assert_eq!(rc, 0);
        unsafe { tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn duplicate_mount_path_returns_null() {
        let server = start_test_server();
        assert!(!server.is_null());
        let cfg1 = make_mux_cfg();
        let cfg2 = make_mux_cfg();
        let path = std::ffi::CString::new("/live").unwrap();
        let m1 =
            unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg1 as *const _) };
        assert!(!m1.is_null());
        let m2 =
            unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg2 as *const _) };
        assert!(m2.is_null(), "duplicate path should return null");
        // Arc 2 WP-A2 (K5): the retired `rtsp_server_error_to_code` collapsed
        // EVERY `RtspServerError` variant to `TST_E_RTSP_SERVER` (-24); the
        // shared kind table splits them the way Python and the JVM already
        // did. `DuplicateMount` is in the MOUNT bucket (-25), alongside
        // `InvalidMountPath` / `InvalidMulticastGroup` / `InvalidConfig`.
        // (`Io`/`BindAddrInUse` → -22, `Tls` → -21 — pinned by
        // `start::tests::tls_cert_pem_on_plaintext_bind_fails_start` —,
        // `UrlParse` → -16; only `AlreadyStarted`/`NotStarted`/`Shutdown`
        // stay -24.)
        assert_eq!(
            unsafe { crate::error::tst_get_last_error() },
            TstError::RtspMount as i32,
            "DuplicateMount must project to TST_E_RTSP_MOUNT (-25), not the \
             retired catch-all TST_E_RTSP_SERVER (-24)"
        );
        unsafe { tst_rtsp_mount_handle_free(m1) };
        unsafe { crate::config::tst_mux_config_free(cfg1) };
        unsafe { crate::config::tst_mux_config_free(cfg2) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }
}
