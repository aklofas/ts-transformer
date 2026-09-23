//! `TstTcpMuxSender` handle type and data-path entry points.
//!
//! Open a TCP-backed `MuxSender` with `tst_tcp_mux_sender_open`.
//! Push encoded video/KLV/audio/subtitle with the `push_*` family.
//! Free with `tst_tcp_mux_sender_close`.
//!
//! Data-path bodies (push_*, get_*_stats, reset_stats) are thin
//! forwarders to generic impls in `crate::transport_impls`. The
//! literal `extern "C"` signature and doc-comment are preserved here
//! so cbindgen can see and emit them to `tstrans.h`.
//!
//! **Single transport type:** TCP uses one `TcpTransport` that implements
//! both `Transport` and `RecvTransport`. `MuxSender<TcpTransport>` uses
//! the `Transport` side for sending.
//!
//! **Cancel:** `tst_tcp_mux_sender_cancel` (ABI 0.22) fires the transport's
//! `TcpCancelHandle`; a `push_*` parked on another thread returns
//! `TST_E_CLOSED`. That entry point is what unblocks a push against a peer
//! that has stopped reading: since deep review #4 WP-4b such a push blocks
//! until the peer resumes, the peer resets the connection, or the handle is
//! cancelled or closed — it no longer returns `TST_E_TRANSPORT` after
//! ~100 ms.

use std::os::raw::c_char;

use tst_pipeline::MuxSender;
use tst_tcp::{TcpTransport, TcpTransportBuilder};

use crate::config::TstMuxConfig;
use crate::error::{TstError, record_mux_error, set_last_error};
use crate::handle::{
    CHandle, TstAudioStreamHandle, TstKlvStreamHandle, TstSubtitleStreamHandle,
    TstVideoStreamHandle, cancel_or_latch,
};

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a TCP-backed mux sender.
///
/// Returned by [`tst_tcp_mux_sender_open`]. Freed with
/// [`tst_tcp_mux_sender_close`].
pub struct TstTcpMuxSender {
    pub(crate) inner: CHandle<MuxSender<TcpTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a TCP-backed `MuxSender` that muxes MPEG-TS in real time and
/// sends over TCP. `mux_cfg` must be a valid `tst_mux_config_t`
/// (constructed via `tst_mux_config_new`). Returns `NULL` on error.
///
/// The mux config is borrowed — the caller still owns it and must free
/// it. The returned handle is independent of the config after this call.
///
/// The TCP connection is established synchronously before this function
/// returns. Default connect timeout is 10 seconds.
///
/// URL grammar:
/// - `tcp://host:port` — connect to a plain TCP listener
/// - `tcps://host:port` — connect with TLS (disabled if built without `tls` feature)
/// - Query params: `?nodelay=1`, `?rcvbuf=N`, `?sndbuf=N`, `?pkt_size=N`,
///   `?connect_timeout=Ns`
///
/// # Safety
///
/// `url` is a NUL-terminated C string. `mux_cfg` must be a non-null
/// pointer to a `tst_mux_config_t` valid for this call. The returned
/// handle must eventually be freed with `tst_tcp_mux_sender_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_open(
    url: *const c_char,
    mux_cfg: *const TstMuxConfig,
) -> *mut TstTcpMuxSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::TcpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let cfg_ref = match unsafe { mux_cfg.as_ref() } {
            Some(c) => c,
            None => {
                set_last_error(TstError::InvalidConfig, "mux_cfg is null");
                return std::ptr::null_mut();
            }
        };
        let built = match cfg_ref.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        let builder = match TcpTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::TcpConfig, &format!("tcp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "tcp connect");
                return std::ptr::null_mut();
            }
        };
        let mux_sender = match MuxSender::new(transport, built) {
            Ok(s) => s,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        // TCP has a real `TcpCancelHandle`, so `cancel_or_latch` passes it
        // straight through.
        let cancel = cancel_or_latch(mux_sender.cancel_handle());
        Box::into_raw(Box::new(TstTcpMuxSender {
            inner: CHandle::new(mux_sender, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_tcp_mux_sender_t`.
///
/// Safe to call with `NULL` (no-op).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstTcpMuxSender` returned
/// by `tst_tcp_mux_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_close(p: *mut TstTcpMuxSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// Interrupt a `tst_tcp_mux_sender_push_*` parked on another thread; that call
/// returns `TST_E_CLOSED`. Callable from any thread, lock-free (never takes
/// the handle's slot), idempotent. The handle must still be freed with
/// `tst_tcp_mux_sender_close`.
///
/// Returns 0, or `TST_E_INVALID_CONFIG` if `p` is null.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut TstTcpMuxSender`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_cancel(p: *mut TstTcpMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
            return TstError::InvalidConfig as i32;
        };
        // `CHandle::cancel` → `Owned::cancel`: fires the transport's
        // `TcpCancelHandle` without taking the slot, so it answers while a
        // data-path call is parked.
        handle.inner.cancel();
        0
    })
}

// ---------------------------------------------------------------------------
// Finish
// ---------------------------------------------------------------------------

/// Drain every byte the muxer still holds to the transport, report the
/// first drain error, then close the transport (`MuxSender::finish`).
///
/// Returns 0 when everything reached the transport, or the negative
/// `TST_E_*` code of the first drain failure (the remaining bytes are
/// abandoned; the sender is closed either way). A second call returns 0.
/// Unlike `tst_tcp_mux_sender_close`, this does NOT cancel first: a `push_*`
/// parked on another thread holds the sender and `_finish` waits behind
/// it — call `tst_tcp_mux_sender_cancel` first if that is not wanted. The
/// handle must still be freed with `tst_tcp_mux_sender_close`.
///
/// Returns `TST_E_INVALID_CONFIG` on a null pointer. Like every entry
/// point, a call after `tst_tcp_mux_sender_close` has freed the pointer is a
/// use-after-free, not an error code — `_close` consumes the handle.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut tst_tcp_mux_sender_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_finish(p: *mut TstTcpMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::PanicCaught as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
            return TstError::InvalidConfig as i32;
        };
        // `CHandle::with_inner_ref(impl FnOnce(&T) -> i32) -> i32`: the
        // closure returns the C code and `CHandle` folds `HandleState`
        // (already-closed / cancelled) into `record_binding_error` itself,
        // so there is no `Err(state)` arm here.
        handle.inner.with_inner_ref(|s| match s.finish() {
            Ok(()) => 0,
            Err(e) => crate::error::record_shell_error(&e),
        })
    })
}

// ---------------------------------------------------------------------------
// Push — single-stream variants
// ---------------------------------------------------------------------------

/// Push one Annex-B NAL through the muxer's single video stream and
/// out the TCP transport (single-stream shorthand).
///
/// `nal` must point to `len` bytes of Annex-B NAL data. `pts_90khz` is
/// the presentation timestamp in 90 kHz ticks. `key_frame` is `true`
/// for IDR / key frames (used to set the random-access indicator in the
/// MPEG-TS adaptation field).
///
/// Resolves only when exactly one video stream is configured; otherwise
/// rejects with `TST_E_INVALID_USAGE`.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `nal` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_video(
    p: *mut TstTcpMuxSender,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_video(&handle.inner, nal, len, pts_90khz, key_frame)
    }
}

/// Push one raw KLV blob through the muxer's single KLV stream and out
/// the TCP transport (single-stream shorthand).
///
/// `klv` must point to **raw MISB Local Set bytes**. For streams
/// configured as `TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA`, the muxer
/// prepends a 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9
/// §2.12.4.2. **Do not pre-wrap the AU cell on the caller side.**
/// `pts_90khz` is the presentation timestamp in 90 kHz ticks.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `klv` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_klv(
    p: *mut TstTcpMuxSender,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_klv(&handle.inner, klv, len, pts_90khz) }
}

/// Push one audio frame buffer through the muxer's single audio stream
/// and out the TCP transport (single-stream shorthand).
///
/// `frames` must point to `len` bytes of pre-framed audio data (one or
/// more ADTS frames or MPEG audio frames concatenated). `pts_90khz` is
/// the presentation timestamp in 90 kHz ticks.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `frames` must
/// be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_audio(
    p: *mut TstTcpMuxSender,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_audio(&handle.inner, frames, len, pts_90khz) }
}

/// Push one subtitle PES unit through the muxer's single subtitle stream
/// and out the TCP transport (single-stream shorthand).
///
/// `payload` is one complete logical subtitle unit. `pts_90khz` is the
/// presentation timestamp in 90 kHz ticks.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `payload` must
/// be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_subtitle(
    p: *mut TstTcpMuxSender,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_subtitle(&handle.inner, payload, len, pts_90khz)
    }
}

// ---------------------------------------------------------------------------
// Push — multi-stream (_to) variants
// ---------------------------------------------------------------------------

/// Push one Annex-B NAL targeting a specific video elementary stream.
///
/// `stream_handle` is obtained from `tst_mux_config_add_video_stream` at
/// config time and is stable across the config→open boundary. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE`.
///
/// On a single-stream sender, prefer `tst_tcp_mux_sender_push_video` —
/// same effect, no handle required.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `nal` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_video_to(
    p: *mut TstTcpMuxSender,
    stream_handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_video_to(
            &handle.inner,
            stream_handle,
            nal,
            len,
            pts_90khz,
            key_frame,
        )
    }
}

/// Push one KLV blob targeting a specific KLV elementary stream.
///
/// For `KlvStreamType::SynchronousMetadata` streams the muxer auto-wraps
/// the caller's bytes in a `Metadata_AU_cell` header (do not pre-wrap).
/// On a single-stream sender, prefer `tst_tcp_mux_sender_push_klv`.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `klv` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_klv_to(
    p: *mut TstTcpMuxSender,
    stream_handle: TstKlvStreamHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_klv_to(
            &handle.inner,
            stream_handle,
            klv,
            len,
            pts_90khz,
        )
    }
}

/// Push one audio frame buffer targeting a specific audio elementary stream.
///
/// On a single-stream sender, prefer `tst_tcp_mux_sender_push_audio`.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `frames` must
/// be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_audio_to(
    p: *mut TstTcpMuxSender,
    stream_handle: TstAudioStreamHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_audio_to(
            &handle.inner,
            stream_handle,
            frames,
            len,
            pts_90khz,
        )
    }
}

/// Push one subtitle PES unit targeting a specific subtitle elementary stream.
///
/// On a single-stream sender, prefer `tst_tcp_mux_sender_push_subtitle`.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpMuxSender`. `payload` must
/// be readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_push_subtitle_to(
    p: *mut TstTcpMuxSender,
    stream_handle: TstSubtitleStreamHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_subtitle_to(
            &handle.inner,
            stream_handle,
            payload,
            len,
            pts_90khz,
        )
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Snapshot mux-sender-level stats for a `tst_tcp_mux_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpMuxSender` opened via
/// `tst_tcp_mux_sender_open`. `out` must point to a writable
/// `TstMuxSenderStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_get_mux_sender_stats(
    p: *mut TstTcpMuxSender,
    out: *mut crate::stats::TstMuxSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_get_mux_sender_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying TCP socket.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if no live stats are available, or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpMuxSender` opened via
/// `tst_tcp_mux_sender_open`. `out` must point to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_get_socket_stats(
    p: *mut TstTcpMuxSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_socket_stats(
            &handle.inner,
            out,
            "tcp mux sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot codec-specific stats for one PID on a `tst_tcp_mux_sender_t`.
///
/// The returned struct is a tagged union — read `out->kind` first, then
/// the matching `out->u.<arm>` field.
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_tcp_mux_sender_open`.
/// `out` must be a writable `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_get_stream_codec_stats(
    p: *mut TstTcpMuxSender,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this tcp mux sender)"
            ),
        )
    }
}

/// Reset stats counters for a `tst_tcp_mux_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpMuxSender` opened via
/// `tst_tcp_mux_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_mux_sender_reset_stats(p: *mut TstTcpMuxSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp mux sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::mux_sender_reset_stats(&handle.inner)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    #[test]
    fn null_close_is_safe() {
        unsafe { tst_tcp_mux_sender_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_push_video_returns_invalid_config() {
        let nal = [0u8; 4];
        let rc = unsafe {
            tst_tcp_mux_sender_push_video(std::ptr::null_mut(), nal.as_ptr(), nal.len(), 0, false)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_push_klv_returns_invalid_config() {
        let klv = [0u8; 4];
        let rc = unsafe {
            tst_tcp_mux_sender_push_klv(std::ptr::null_mut(), klv.as_ptr(), klv.len(), 0)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_mux_sender_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstMuxSenderStats::default();
        let rc =
            unsafe { tst_tcp_mux_sender_get_mux_sender_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_tcp_mux_sender_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn open_with_null_url_returns_null() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            let p = tst_tcp_mux_sender_open(std::ptr::null(), cfg as *const _);
            assert!(p.is_null());
            tst_mux_config_free(cfg);
        }
    }

    #[test]
    fn open_with_null_config_returns_null() {
        let url = std::ffi::CString::new("tcp://127.0.0.1:54422").unwrap();
        let p = unsafe { tst_tcp_mux_sender_open(url.as_ptr(), std::ptr::null()) };
        assert!(p.is_null());
    }
}
