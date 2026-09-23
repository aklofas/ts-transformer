//! `tst_mux_sender_t` (plain) and `tst_managed_mux_sender_t` (managed).
//!
//! Both wrap `tst_pipeline::MuxSender<T>`, with T parameterized on the
//! underlying transport. Plain uses `SrtTransport`; managed uses
//! `ManagedTransport<SrtTransport>` with a factory that reconnects via the
//! original URL on transport breakage.
//!
//! Push and stats bodies forward to generic impls in
//! `crate::transport_impls` (SIMP-CBIND-1). Open/close/cancel, the
//! SRT-specific data-stream entry points, and the `parse_c_srt_url*`
//! helpers stay family-local.

use crate::config::{TstMuxConfig, TstReconnectPolicy};
use crate::error::record_binding_error;
use crate::error::{
    TstError, record_mux_error, record_shell_error, set_last_error, tst_get_last_error,
};
use crate::handle::{CHandle, TstDataStreamHandle, cancel_or_latch};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::DataStreamHandle;
use tst_pipeline::binding::BindingError;
use tst_pipeline::{ManagedTransport, MuxSender};
use tst_srt::SrtTransport;

// ------------------------------------------------------------------
// tst_mux_sender_t (plain L1)
// ------------------------------------------------------------------

pub struct TstMuxSender {
    inner: CHandle<MuxSender<SrtTransport>>,
}

/// Open a `tst_mux_sender_t` connected via SRT.
///
/// `srt_url` is a `srt://host:port?key=value&...` URL. Query
/// parameters apply libsrt-vocabulary options to the connection
/// (passphrase, latency, streamid, etc.). URL values override config
/// values for the same option. See
/// `docs/guides/srt.md#url-parsing` for the recognized key table.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. The detail string from
/// `tst_get_last_error_str()` describes the specific problem.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_open(
    srt_url: *const libc::c_char,
    cfg: *mut TstMuxConfig,
) -> *mut TstMuxSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return std::ptr::null_mut();
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let built = match cfg.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        // See `tst_sender_open`: plain senders refuse `?mode=listener` here
        // (binding-level; `SrtUrl::connect` is mode-agnostic), then
        // `SrtUrl::connect` performs the whole caller-mode open.
        if require_caller_mode(&url).is_err() {
            return std::ptr::null_mut();
        }
        let transport = match url.connect() {
            Ok(t) => t,
            Err(e) => {
                record_binding_error(BindingError::from(e));
                return std::ptr::null_mut();
            }
        };
        let sender = match MuxSender::new(transport, built) {
            Ok(s) => s,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        let cancel = cancel_or_latch(sender.cancel_handle());
        Box::into_raw(Box::new(TstMuxSender {
            inner: CHandle::new(sender, cancel, ()),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_video(
    p: *mut TstMuxSender,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_video(&handle.inner, nal, len, pts_90khz, key_frame)
    }
}

/// Send one KLV blob through the muxer's single KLV stream and out the
/// transport.
///
/// `klv` must point to **raw MISB Local Set bytes**. For streams configured
/// as `TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA`, the muxer prepends a
/// 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 §2.12.4.2 before
/// emitting. **Do not pre-wrap the AU cell on the caller side** —
/// double-wrapping produces metadata that receivers cannot parse. For
/// streams configured as `TST_KLV_STREAM_TYPE_PRIVATE_DATA`, the payload
/// is emitted as-is.
///
/// `pts_90khz` is the presentation timestamp in 90 kHz ticks. The current
/// API uses `metadata_service_id = 0x00` per ST 1402.2 App. B Table 2; a
/// future entry will expose the field explicitly.
///
/// Single-stream form: the mux sender must have exactly one KLV stream
/// configured. Multi-stream callers use `tst_mux_sender_send_klv_to` with
/// an explicit `TstKlvStreamHandle`.
///
/// # Errors
///
/// Routed through `tst_get_last_error()` via the inner `MuxSender`
/// shell's `record_shell_error`. Common codes:
///
/// - `TST_E_INVALID_USAGE` — no KLV stream configured or ambiguous target.
/// - `TST_E_KLV_TOO_LARGE` — payload exceeds the per-frame KLV size limit.
/// - `TST_E_TRANSPORT` — transport-layer failure (closed, timeout, broken pipe).
/// - `TST_E_INVALID_CONFIG` — `klv` is null with non-zero `len`.
///
/// # C ABI
///
/// `tst_mux_sender_send_klv` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_klv(
    p: *mut TstMuxSender,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_klv(&handle.inner, klv, len, pts_90khz) }
}

/// Push one Annex-B NAL targeting a specific video elementary stream.
///
/// `stream_handle` is obtained from `tst_mux_config_add_video_stream` at
/// config time and is stable across the config→open boundary. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream sender, prefer `tst_mux_sender_send_video` — same
/// effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_video_to(
    p: *mut TstMuxSender,
    stream_handle: crate::handle::TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Push one pre-built KLV blob targeting a specific KLV elementary stream.
///
/// For `KlvStreamType::SynchronousMetadata` streams, the muxer auto-wraps
/// the caller's bytes in a `Metadata_AU_cell` header per ITU-T H.222.0
/// V9 § 2.12.4.2 (5 bytes prepended; PTS surfaced in the PES header).
/// For `KlvStreamType::PrivateData` streams, the caller's bytes pass
/// through unchanged.
///
/// On a single-stream sender, prefer `tst_mux_sender_send_klv` — same
/// effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_klv_to(
    p: *mut TstMuxSender,
    stream_handle: crate::handle::TstKlvStreamHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Send one data payload through the mux sender's single data stream and
/// out the transport.
///
/// **Pass-through contract:** the muxer applies no AU-cell wrap, no
/// framing, and no payload inspection — `data` lands verbatim as the
/// payload of exactly one PES packet on the configured PID, using
/// `stream_id` `0xBD` (`private_stream_1`). Record boundaries within
/// `data` (if any) are entirely the caller's convention.
///
/// `pts_90khz` is written into the PES header only when the stream was
/// configured with `carries_pts = true` in
/// `tst_mux_config_add_data_stream`; it is **always** used for PSI/PCR
/// pacing decisions regardless. For `carries_pts = false` streams the
/// PES omits the PTS field entirely; this library's demuxer surfaces
/// such samples with `pts == 0` (its no-PTS substitute).
///
/// Single-stream form: the mux sender must have exactly one data stream
/// configured. Multi-stream callers use `tst_mux_sender_send_data_to`
/// with an explicit `tst_data_stream_handle_t`.
///
/// # Errors
///
/// Routed through `tst_get_last_error()` via the inner `MuxSender`
/// shell's `record_shell_error`. Common codes:
///
/// - `TST_E_INVALID_USAGE` — no data stream configured, ambiguous target,
///   or payload exceeds the `PES_packet_length` ceiling (`DataTooLarge`:
///   65532 bytes without PTS, 65527 with).
/// - `TST_E_TRANSPORT` — transport-layer failure (closed, timeout, broken pipe).
/// - `TST_E_INVALID_CONFIG` — `data` is null with non-zero `len`.
///
/// # C ABI
///
/// `tst_mux_sender_send_data` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_data(
    p: *mut TstMuxSender,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_ref(|s| match s.send_data(slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_shell_error(&e);
                unsafe { tst_get_last_error() }
            }
        })
}

/// Send one data payload targeting a specific data elementary stream.
///
/// `stream_handle` is obtained from `tst_mux_config_add_data_stream` at
/// config time and is stable across the config→open boundary. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`). Payload, PTS, and size-ceiling
/// contracts are those of `tst_mux_sender_send_data` (the single-stream
/// form).
///
/// On a single-stream sender, prefer `tst_mux_sender_send_data` — same
/// effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_data_to(
    p: *mut TstMuxSender,
    stream_handle: TstDataStreamHandle,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(wrapper) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation — see VideoStreamHandle::try_from_raw rationale
    // in tst_mux_sender_send_video_to above.
    let stream = match DataStreamHandle::try_from_raw(stream_handle) {
        Ok(h) => h,
        Err(e) => {
            crate::error::record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    wrapper
        .inner
        .with_inner_ref(|s| match s.send_data_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_shell_error(&e);
                unsafe { tst_get_last_error() }
            }
        })
}

/// Send one audio frame buffer (single-stream shorthand).
///
/// Resolves only when exactly one audio stream is configured.
/// Otherwise rejects with `TST_E_INVALID_USAGE` (carrying
/// `MuxError::AmbiguousTarget` or `MuxError::NoAudioStreamsConfigured`).
///
/// `frames` is one or more pre-framed audio frames concatenated by the
/// caller. PTS is required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_audio(
    p: *mut TstMuxSender,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_audio(&handle.inner, frames, len, pts_90khz) }
}

/// Send one audio frame buffer targeting a specific audio elementary stream.
///
/// `stream_handle` is obtained from `tst_mux_config_add_audio_stream` /
/// `tst_mux_config_add_audio_stream_with_language`. Out-of-range handles
/// surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream sender, prefer `tst_mux_sender_send_audio` — same
/// effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_audio_to(
    p: *mut TstMuxSender,
    stream_handle: crate::handle::TstAudioStreamHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Send one subtitle PES unit (single-stream shorthand).
///
/// Resolves only when exactly one subtitle stream is configured.
/// Otherwise rejects with `TST_E_INVALID_USAGE` (carrying
/// `MuxError::AmbiguousTarget` or
/// `MuxError::NoSubtitleStreamsConfigured`).
///
/// `payload` is one complete logical subtitle unit (DVB-sub composition
/// page, teletext data field, CEA-708 service block, or WebVTT cue).
/// PTS is required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_subtitle(
    p: *mut TstMuxSender,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_subtitle(&handle.inner, payload, len, pts_90khz)
    }
}

/// Send one subtitle PES unit targeting a specific subtitle elementary
/// stream.
///
/// `stream_handle` is obtained from one of the four
/// `tst_mux_config_add_subtitle_stream_*` constructors. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream sender, prefer `tst_mux_sender_send_subtitle` —
/// same effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_send_subtitle_to(
    p: *mut TstMuxSender,
    stream_handle: crate::handle::TstSubtitleStreamHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Snapshot stats for a `tst_mux_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_get_stats(
    p: *mut TstMuxSender,
    out: *mut crate::stats::TstMuxSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_get_mux_sender_stats(&handle.inner, out) }
}

/// Read wire-level transport stats (RTT, packet loss, bandwidth, queue
/// depths) for the underlying libsrt socket. Cumulative since connect.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns:
/// * `0` on success — `*out` is populated.
/// * `TST_E_INVALID_CONFIG` if `p` or `out` is NULL.
/// * `TST_E_NOT_AVAILABLE` if the inner transport has no live socket
///   (closed or — for the managed sibling — mid-reconnect).
/// * `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstMuxSender` opened via
/// `tst_mux_sender_open` and `out` points to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_get_socket_stats(
    p: *mut TstMuxSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_socket_stats(
            &handle.inner,
            out,
            "mux sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot codec-specific stats for one PID on a `tst_mux_sender_t` into `*out`.
///
/// The returned struct is a tagged union — read `out->kind` first, then
/// the matching `out->u.<arm>` field. See `tst_stream_codec_stats_t` in
/// `tstrans.h` for the discriminator constants (`TST_CODEC_KIND_*`).
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed via `tst_mux_sender_close`
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_mux_sender_open`; `out`
/// must be a writable `tst_stream_codec_stats_t`. The pointee is fully
/// written on `TST_OK` and untouched on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_get_stream_codec_stats(
    p: *mut TstMuxSender,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this mux sender)"
            ),
        )
    }
}

/// Reset stats counters for a `tst_mux_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_reset_stats(p: *mut TstMuxSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::mux_sender_reset_stats(&handle.inner)
}

/// Close and free a `tst_mux_sender_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_close(p: *mut TstMuxSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

/// Cancel a `tst_mux_sender_t`. Unblocks a thread parked in any `_send_*`
/// entry point within one libsrt I/O cycle (~3-10 ms) by closing the
/// underlying libsrt socket. Safe to call from any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, the first `_send_*` that observes the cancel and every
/// later one return `TST_E_CLOSED` (-7). libsrt reports the closed socket as
/// a broken connection, but `SrtTransport` reads its own cancel latch
/// afterwards and reports the cancel the caller asked for (0.7.0; through
/// 0.6.x the first call reported `TST_E_TRANSPORT`). `_cancel` itself never
/// closes the shell — the handle must still be `_close`'d to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_cancel(p: *mut TstMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Drain every byte the muxer still holds to the transport, report the
/// first drain error, then close the transport (`MuxSender::finish`).
///
/// Returns 0 when everything reached the transport, or the negative
/// `TST_E_*` code of the first drain failure (the remaining bytes are
/// abandoned; the sender is closed either way). A second call returns 0.
/// Unlike `tst_mux_sender_close`, this does NOT cancel first: a `send_*`
/// parked on another thread holds the sender and `_finish` waits behind
/// it — call `tst_mux_sender_cancel` first if that is not wanted. The
/// handle must still be freed with `tst_mux_sender_close`.
///
/// Returns `TST_E_INVALID_CONFIG` on a null pointer. Like every entry
/// point, a call after `tst_mux_sender_close` has freed the pointer is a
/// use-after-free, not an error code — `_close` consumes the handle.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut tst_mux_sender_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_sender_finish(p: *mut TstMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::PanicCaught as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Borrow `srt_url` as a Rust string and run it through `tst_srt::url`'s
/// rich URL parser. Sets last-error and returns `Err(())` on any failure
/// path; caller treats `Err(())` as "return NULL".
pub(crate) unsafe fn parse_c_srt_url(srt_url: *const libc::c_char) -> Result<tst_srt::SrtUrl, ()> {
    if srt_url.is_null() {
        set_last_error(TstError::InvalidConfig, "null srt_url");
        return Err(());
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(srt_url) };
    let s = match cstr.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error(TstError::InvalidConfig, "srt_url is not valid utf-8");
            return Err(());
        }
    };
    tst_srt::SrtUrl::parse(s).map_err(|e| {
        set_last_error(TstError::InvalidConfig, &format!("invalid srt url: {e}"));
    })
}

/// Refuse `?mode=listener` on a PLAIN sender open.
///
/// The mode check is binding-level on purpose: [`tst_srt::SrtUrl::connect`]
/// is mode-agnostic by design (the caller picks the direction by calling
/// `connect` or `accept_one`), and the only refusal inside tst-srt is in the
/// MANAGED send-side `shells::managed_*_sender_from_url` family. The plain C
/// senders have no listener path, so without this check a `?mode=listener`
/// URL would silently dial out as a caller — which is exactly what every
/// release before 0.7.0 did.
///
/// The CODE matches what the managed family surfaces (tst-srt's
/// `callers_only()` → `SrtError::Option` → `BindingErrorKind::ConfigInvalid`
/// → −1), so both C sender paths answer a listener-mode URL with
/// `TST_E_INVALID_CONFIG`. The DETAIL differs by one word on purpose:
/// tst-srt says "managed senders open as callers only" because that is the
/// only family it guards, and this one says "plain senders" for the same
/// reason.
pub(crate) fn require_caller_mode(url: &tst_srt::SrtUrl) -> Result<(), ()> {
    if url.mode == tst_srt::url::Mode::Listener {
        set_last_error(
            TstError::InvalidConfig,
            "?mode=listener: plain senders open as callers only \
             (see deferred-features.md, \"SRT URL mode=listener / mode=rendezvous dispatch\")",
        );
        return Err(());
    }
    Ok(())
}

/// Like [`parse_c_srt_url`] but more forgiving for listener-mode entry points:
/// if the URL omits both a host and `?mode=listener` (e.g. `srt://:7000`),
/// inject `?mode=listener` and retry. This lets `_open_listener` entry points
/// accept the clean `srt://:port` form directly — the `_listener` suffix in
/// the function name is the authoritative listener-mode signal, so requiring
/// the URL to also carry `?mode=listener` is redundant.
///
/// Any error other than [`tst_srt::UrlError::MissingHost`] is returned
/// unchanged (the first-pass error is already recorded in the thread-local
/// last-error by `parse_c_srt_url`).
///
/// Called by `tst_*_open_listener` entry points. Plain `tst_*_open` entry
/// points keep the strict parse via `parse_c_srt_url` (an empty host is
/// meaningless for caller mode).
pub(crate) unsafe fn parse_c_srt_url_listener(
    srt_url: *const libc::c_char,
) -> Result<tst_srt::SrtUrl, ()> {
    // First-pass: the fast common path (URL already has a host or already
    // carries ?mode=listener).
    let first = unsafe { parse_c_srt_url(srt_url) };
    if first.is_ok() {
        return first;
    }
    // First pass failed and already recorded the error. To branch on
    // MissingHost specifically, re-parse here directly. The cost is negligible
    // (one extra string parse on an error path).
    if srt_url.is_null() {
        // Null pointer: already handled by parse_c_srt_url above.
        return Err(());
    }
    let s = unsafe { std::ffi::CStr::from_ptr(srt_url) }
        .to_string_lossy()
        .into_owned();
    match tst_srt::SrtUrl::parse(&s) {
        Err(tst_srt::UrlError::MissingHost) => {}
        _ => {
            // Some other error (or unexpectedly Ok) — the first pass already
            // recorded it; return the original Err(()).
            return Err(());
        }
    }
    // MissingHost on an empty-host URL — inject mode=listener and retry.
    let sep = if s.contains('?') { '&' } else { '?' };
    let augmented = format!("{s}{sep}mode=listener");
    tst_srt::SrtUrl::parse(&augmented).map_err(|e| {
        set_last_error(TstError::InvalidConfig, &format!("invalid srt url: {e}"));
    })
}

// ------------------------------------------------------------------
// tst_managed_mux_sender_t (managed L2)
// ------------------------------------------------------------------

pub struct TstManagedMuxSender {
    /// Snapshot = the reconnect/gap telemetry observer captured at open;
    /// see `TstManagedSender`.
    inner: CHandle<MuxSender<ManagedTransport<SrtTransport>>, tst_pipeline::ManagedStatsHandle>,
}

/// Open a `tst_managed_mux_sender_t` connected via SRT.
///
/// `srt_url` is a `srt://host:port?key=value&...` URL. Query
/// parameters apply libsrt-vocabulary options to the connection
/// (passphrase, latency, streamid, etc.). URL values override config
/// values for the same option. See
/// `docs/guides/srt.md#url-parsing` for the recognized key table.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. The detail string from
/// `tst_get_last_error_str()` describes the specific problem.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_open(
    srt_url: *const libc::c_char,
    cfg: *mut TstMuxConfig,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedMuxSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return std::ptr::null_mut();
        };
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let built = match cfg.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        // See `tst_managed_sender_open`: initial connect + the same-URL
        // reconnect factory (so the overlay survives every reconnect) + the
        // decorator + the shell + the observers, all in one tst-srt call,
        // which also refuses `?mode=listener` itself. A `MuxerConfig` that
        // the shell rejects comes back as `SrtError` here, not `MuxError` —
        // `build_config()` above already validated the C-side config.
        let (sender, handles, stats) =
            match tst_srt::shells::managed_mux_sender_from_url(&url, policy, built) {
                Ok(t) => t,
                Err(e) => {
                    record_binding_error(BindingError::from(e));
                    return std::ptr::null_mut();
                }
            };
        Box::into_raw(Box::new(TstManagedMuxSender {
            inner: CHandle::new(sender, handles.cancel, stats),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_video(
    p: *mut TstManagedMuxSender,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_video(&handle.inner, nal, len, pts_90khz, key_frame)
    }
}

/// Send one KLV blob through the managed mux sender's single KLV stream
/// and out the underlying reconnecting transport.
///
/// Same payload contract as `tst_mux_sender_send_klv`: **raw MISB Local
/// Set bytes**, muxer auto-wraps the AU cell for SynchronousMetadata
/// streams. Do not pre-wrap.
///
/// `pts_90khz` is the presentation timestamp in 90 kHz ticks. The current
/// API uses `metadata_service_id = 0x00`.
///
/// Single-stream form: see `tst_managed_mux_sender_send_klv_to` for the
/// multi-stream variant.
///
/// # Errors
///
/// Routed through `tst_get_last_error()`. Same code set as
/// `tst_mux_sender_send_klv` plus reconnect-specific transient codes:
///
/// - `TST_E_NOT_AVAILABLE` — transport mid-reconnect (transient; next
///   call may succeed).
///
/// # C ABI
///
/// `tst_managed_mux_sender_send_klv` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_klv(
    p: *mut TstManagedMuxSender,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_klv(&handle.inner, klv, len, pts_90khz) }
}

/// Push one Annex-B NAL targeting a specific video elementary stream on a
/// managed (auto-reconnecting) sender.
///
/// `stream_handle` is obtained from `tst_mux_config_add_video_stream` at
/// config time and is stable across reconnects. Out-of-range handles
/// surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream sender, prefer `tst_managed_mux_sender_send_video` —
/// same effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_video_to(
    p: *mut TstManagedMuxSender,
    stream_handle: crate::handle::TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Push one pre-built KLV blob targeting a specific KLV elementary stream on
/// a managed (auto-reconnecting) sender.
///
/// For `KlvStreamType::SynchronousMetadata` streams, the muxer auto-wraps
/// the caller's bytes in a `Metadata_AU_cell` header per ITU-T H.222.0
/// V9 § 2.12.4.2 (5 bytes prepended; PTS surfaced in the PES header).
/// For `KlvStreamType::PrivateData` streams, the caller's bytes pass
/// through unchanged.
///
/// On a single-stream sender, prefer `tst_managed_mux_sender_send_klv` —
/// same effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_klv_to(
    p: *mut TstManagedMuxSender,
    stream_handle: crate::handle::TstKlvStreamHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Managed sibling of [`tst_mux_sender_send_audio`]. Same semantics; routes
/// through the inner reconnecting transport.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_audio(
    p: *mut TstManagedMuxSender,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_push_audio(&handle.inner, frames, len, pts_90khz) }
}

/// Managed sibling of [`tst_mux_sender_send_audio_to`]. Same semantics;
/// `stream_handle` is stable across reconnects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_audio_to(
    p: *mut TstManagedMuxSender,
    stream_handle: crate::handle::TstAudioStreamHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Managed sibling of [`tst_mux_sender_send_subtitle`]. Same semantics; routes
/// through the inner reconnecting transport.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_subtitle(
    p: *mut TstManagedMuxSender,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_push_subtitle(&handle.inner, payload, len, pts_90khz)
    }
}

/// Managed sibling of [`tst_mux_sender_send_subtitle_to`]. Same semantics;
/// `stream_handle` is stable across reconnects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_subtitle_to(
    p: *mut TstManagedMuxSender,
    stream_handle: crate::handle::TstSubtitleStreamHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
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

/// Send one data payload through the managed mux sender's single data
/// stream and out the underlying reconnecting transport.
///
/// Pass-through contract identical to `tst_mux_sender_send_data`: `data`
/// lands verbatim as one PES packet on `stream_id` 0xBD; PTS is written
/// only for `carries_pts = true` streams.
///
/// Single-stream form: see `tst_managed_mux_sender_send_data_to` for the
/// multi-stream variant.
///
/// # Errors
///
/// Routed through `tst_get_last_error()`. Same code set as
/// `tst_mux_sender_send_data` plus `TST_E_NOT_AVAILABLE` (transport
/// mid-reconnect; transient).
///
/// # C ABI
///
/// `tst_managed_mux_sender_send_data` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_data(
    p: *mut TstManagedMuxSender,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_ref(|s| match s.send_data(slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_shell_error(&e);
                unsafe { tst_get_last_error() }
            }
        })
}

/// Send one data payload targeting a specific data elementary stream on a
/// managed (auto-reconnecting) sender. `stream_handle` is stable across
/// reconnects. Out-of-range handles surface as `TST_E_INVALID_USAGE`.
///
/// On a single-stream sender, prefer `tst_managed_mux_sender_send_data`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_send_data_to(
    p: *mut TstManagedMuxSender,
    stream_handle: TstDataStreamHandle,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> libc::c_int {
    let Some(wrapper) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match DataStreamHandle::try_from_raw(stream_handle) {
        Ok(h) => h,
        Err(e) => {
            crate::error::record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    wrapper
        .inner
        .with_inner_ref(|s| match s.send_data_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_shell_error(&e);
                unsafe { tst_get_last_error() }
            }
        })
}

/// Snapshot stats for a `tst_managed_mux_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_get_stats(
    p: *mut TstManagedMuxSender,
    out: *mut crate::stats::TstMuxSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::mux_sender_get_mux_sender_stats(&handle.inner, out) }
}

/// See [`tst_mux_sender_get_socket_stats`]. The managed variant returns
/// `TST_E_NOT_AVAILABLE` whenever the reconnect loop currently has no
/// live inner socket — callers should treat this as transient and retry.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedMuxSender` opened via
/// `tst_managed_mux_sender_open` and `out` points to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_get_socket_stats(
    p: *mut TstManagedMuxSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_socket_stats(
            &handle.inner,
            out,
            "mux sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot reconnect/gap telemetry for a `tst_managed_mux_sender_t` into
/// `*out`. Unlike [`tst_managed_mux_sender_get_socket_stats`], this never
/// returns `TST_E_NOT_AVAILABLE` — the counters live on the side-channel
/// `ManagedStatsHandle`, which stays readable across reconnect gaps.
///
/// **`Blocking` mode note:** this call still contends on the shell's own
/// lock (for the closed-check), the same lock a send stuck in
/// `Blocking` mode's inline reconnect loop holds for the whole outage —
/// so it can block for the outage's duration in that mode. Polling this
/// getter without ever blocking is a `Background`-mode property (the
/// mode these stats primarily exist to observe).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_CLOSED` if the sender has been closed, or `TST_E_INTERNAL` if the
/// gap-buffer lock is poisoned (see `ManagedTransport`'s lock poisoning
/// policy — a prior panic mid-drain).
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedMuxSender` opened via
/// `tst_managed_mux_sender_open` and `out` points to a writable
/// `tst_managed_transport_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_get_reconnect_stats(
    p: *mut TstManagedMuxSender,
    out: *mut crate::stats::TstManagedTransportStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::managed_get_reconnect_stats(&handle.inner, out) }
}

/// Managed sibling of [`tst_mux_sender_get_stream_codec_stats`]. Returns
/// the same values — codec stats live on the inner `Muxer`, so they
/// persist across reconnect. No `TST_E_NOT_AVAILABLE` routing.
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed via `tst_managed_mux_sender_close`
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_managed_mux_sender_open`;
/// `out` must be a writable `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_get_stream_codec_stats(
    p: *mut TstManagedMuxSender,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::mux_sender_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this mux sender)"
            ),
        )
    }
}

/// Reset stats counters for a `tst_managed_mux_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_reset_stats(
    p: *mut TstManagedMuxSender,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::mux_sender_reset_stats(&handle.inner)
}

/// Close and free a `tst_managed_mux_sender_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_close(p: *mut TstManagedMuxSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

/// Cancel a `tst_managed_mux_sender_t`. Same semantics as
/// `tst_mux_sender_cancel`; reaches the currently-active inner
/// transport's cancel handle through `ManagedTransport`'s atomic
/// snapshot.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel the `_send_*` entry points report `TST_E_CLOSED` (-7) — the
/// managed decorator latches the close before the send reaches libsrt.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_cancel(p: *mut TstManagedMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Drain every byte the muxer still holds to the transport, report the
/// first drain error, then close the transport (`MuxSender::finish`).
///
/// Returns 0 when everything reached the transport, or the negative
/// `TST_E_*` code of the first drain failure (the remaining bytes are
/// abandoned; the sender is closed either way). A second call returns 0.
/// Unlike `tst_managed_mux_sender_close`, this does NOT cancel first: a
/// `send_*` parked on another thread — including one waiting out a
/// reconnect backoff — holds the sender and `_finish` waits behind it;
/// call `tst_managed_mux_sender_cancel` first if that is not wanted. The
/// handle must still be freed with `tst_managed_mux_sender_close`.
///
/// Returns `TST_E_INVALID_CONFIG` on a null pointer. Like every entry
/// point, a call after `tst_managed_mux_sender_close` has freed the
/// pointer is a use-after-free, not an error code — `_close` consumes the
/// handle.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed
/// `*mut tst_managed_mux_sender_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_mux_sender_finish(p: *mut TstManagedMuxSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::PanicCaught as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null managed sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.with_inner_ref(|s| match s.finish() {
            Ok(()) => 0,
            Err(e) => crate::error::record_shell_error(&e),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::ffi::CString;

    #[test]
    fn open_with_invalid_url_returns_null_and_sets_error() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
            let bad = CString::new("not-an-srt-url").unwrap();
            let p = tst_mux_sender_open(bad.as_ptr(), cfg);
            assert!(p.is_null());
            assert_eq!(
                crate::error::tst_get_last_error() as i32,
                TstError::InvalidConfig as i32,
            );
            tst_mux_config_free(cfg);
        }
    }

    #[test]
    fn open_with_unreachable_host_returns_null_with_transport_error() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
            // Reserved-for-documentation address that should reject quickly.
            let url = CString::new("srt://192.0.2.1:9").unwrap();
            let p = tst_mux_sender_open(url.as_ptr(), cfg);
            assert!(p.is_null());
            // Either Transport (broken) or InvalidConfig depending on libsrt
            // resolver behavior — both are valid failures here.
            let code = crate::error::tst_get_last_error() as i32;
            assert!(
                code == TstError::Transport as i32 || code == TstError::InvalidConfig as i32,
                "expected Transport or InvalidConfig, got {code}",
            );
            tst_mux_config_free(cfg);
        }
    }

    #[test]
    fn null_close_is_safe() {
        unsafe {
            tst_mux_sender_close(std::ptr::null_mut());
            tst_managed_mux_sender_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_mux_sender_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_managed_mux_sender_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_handle_get_reconnect_stats_returns_invalid_config() {
        let mut out = crate::stats::TstManagedTransportStats::default();
        let rc =
            unsafe { tst_managed_mux_sender_get_reconnect_stats(std::ptr::null_mut(), &mut out) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    /// Twin of `ts_sender.rs`'s pin: `require_caller_mode` refuses
    /// `?mode=listener` before any socket (Arc 2 WP-B1 behaviour change — it
    /// used to dial out as a caller). Nothing is dialled, so port 1 is never
    /// touched and the test needs no peer.
    #[test]
    fn open_with_listener_mode_url_is_refused_before_any_socket() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            let url = std::ffi::CString::new("srt://127.0.0.1:1?mode=listener").unwrap();
            let p = tst_mux_sender_open(url.as_ptr(), cfg);
            assert!(p.is_null());
            assert_eq!(
                crate::error::tst_get_last_error(),
                TstError::InvalidConfig as i32
            );
            tst_mux_config_free(cfg);
        }
    }
}
