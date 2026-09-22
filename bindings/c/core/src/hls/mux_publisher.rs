//! `TstMuxPublisher` — a `MuxPublisher<HlsPublisher>` projection.
//!
//! Where `tst_publisher_push_ts` takes pre-muxed TS bytes, the mux
//! publisher owns a full MPEG-TS muxer and accepts encoded elementary
//! data — Annex-B NAL units, raw MISB KLV Local Set bytes, audio frames,
//! subtitle PES — and muxes them into TS that flows to the inner HLS
//! publisher's segments. KLV stays inside the `.ts` segments (the
//! STANAG-4609 in-band carriage shape); there is no WebVTT sidecar.
//!
//! Construction takes ownership of a `TstPublisher` (which must currently
//! hold an HLS publisher) plus a `tst_mux_config_t` describing the program
//! / stream layout — the exact same config object built for the SRT/RTP
//! mux senders. `tst_mux_publisher_finish_into_publisher` flushes the
//! muxer and hands the inner `HlsPublisher` back as a fresh
//! `TstPublisher` so the caller can `tst_publisher_finish` it for a clean
//! `#EXT-X-ENDLIST` close.
//!
//! `MuxPublisher`'s `send_*` / `cut_segment` methods take `&self` (interior
//! mutability via an inner `Mutex`), so the C handle stores the publisher
//! directly behind an `Option` — `Option` only so `finish` can move the
//! inner out by value.

use tst_core::mpegts::common::Pts90khz;
use tst_hls::HlsPublisher;
use tst_pipeline::binding::BindingError;
use tst_pipeline::{MuxPublisher, MuxPublisherError};

use crate::config::TstMuxConfig;
use crate::error::{TstError, record_binding_error, record_mux_error, set_last_error};
use crate::hls::publisher::{PublisherImpl, TstPublisher};
use crate::stats::{TstMuxPublisherStats, TstPublisherStats};

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for an HLS-backed `MuxPublisher`.
///
/// Returned by [`tst_mux_publisher_with_config_hls`]. Freed with
/// [`tst_mux_publisher_free`]. The inner is `None` only after
/// [`tst_mux_publisher_finish_into_publisher`] moves the publisher out.
pub struct TstMuxPublisher {
    inner: Option<MuxPublisher<HlsPublisher>>,
}

/// Map a `MuxPublisherError<HlsError>` to a code + recorded last-error.
///
/// One path, like every other error in tst-c since Arc 2: the shared
/// `From<MuxPublisherError<E>>` classifies by SOURCE (`Mux` → the mux
/// kind, `Publisher` → the sink's own kind, `Closed` → `CLOSED`,
/// `LockPoisoned` → `INTERNAL`) and `record_binding_error` projects it.
///
/// Takes the error BY VALUE: the shared impl consumes it to keep each
/// variant's own `Display` detail.
fn record_mux_publisher_error(e: MuxPublisherError<tst_hls::HlsError>) -> i32 {
    record_binding_error(BindingError::from(e))
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Build an HLS-backed `MuxPublisher` from a `TstPublisher` (which must
/// currently hold an HLS publisher) + a `tst_mux_config_t`.
///
/// Consumes `hls` — the `TstPublisher` pointer is freed by this call (do
/// not free it again). `program_cfg` is borrowed; the caller still owns it
/// and must free it with `tst_mux_config_free`.
///
/// Returns `NULL` on error: `TST_E_HLS_CONFIG` if `hls` is null / finished
/// / not an HLS publisher, `TST_E_INVALID_CONFIG` if `program_cfg` is
/// null, or a muxer-config error code if the program config is invalid.
///
/// # Safety
///
/// `hls` must be a valid non-freed `*mut TstPublisher`. `program_cfg` must
/// be a non-null `*const TstMuxConfig` valid for this call. The returned
/// handle must eventually be freed with `tst_mux_publisher_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_with_config_hls(
    hls: *mut TstPublisher,
    program_cfg: *const TstMuxConfig,
) -> *mut TstMuxPublisher {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        if hls.is_null() {
            set_last_error(TstError::HlsConfig, "null publisher pointer");
            return std::ptr::null_mut();
        }
        let cfg_ref = match unsafe { program_cfg.as_ref() } {
            Some(c) => c,
            None => {
                set_last_error(TstError::InvalidConfig, "program_cfg is null");
                return std::ptr::null_mut();
            }
        };
        // Build the muxer config from the program config exactly as
        // rtp/mux_sender.rs does (TstMuxConfig::build_config -> MuxerConfig).
        let muxer_cfg = match cfg_ref.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return std::ptr::null_mut();
            }
        };
        // Consume the TstPublisher Box and require it currently holds HLS.
        let mut boxed = unsafe { Box::from_raw(hls) };
        let hls_pub = match boxed.inner.take() {
            Some(PublisherImpl::Hls(h)) => h,
            None => {
                set_last_error(
                    TstError::HlsConfig,
                    "publisher is finished or not an HLS publisher",
                );
                return std::ptr::null_mut();
            }
        };
        // `boxed` is dropped here (its inner is now None — no double close).
        match MuxPublisher::with_config(hls_pub, muxer_cfg) {
            Ok(mp) => Box::into_raw(Box::new(TstMuxPublisher { inner: Some(mp) })),
            Err(e) => {
                record_mux_publisher_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Free a `tst_mux_publisher_t`.
///
/// Dropping a live mux publisher drops its inner HLS publisher, which
/// shuts down the HTTP server (no `#EXT-X-ENDLIST`). For a clean close,
/// call `tst_mux_publisher_finish_into_publisher` then
/// `tst_publisher_finish` first.
///
/// Safe to call with `NULL` (no-op).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstMuxPublisher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_free(p: *mut TstMuxPublisher) {
    crate::panic::ffi_catch((), || {
        if !p.is_null() {
            drop(unsafe { Box::from_raw(p) });
        }
    });
}

// ---------------------------------------------------------------------------
// Data-path: send_*
// ---------------------------------------------------------------------------

/// Run `f` against the live inner `MuxPublisher`, mapping the common null /
/// finished guards. Used by all the `send_*` + `cut_segment` entries.
///
/// SAFETY: `p` is dereferenced as `&TstMuxPublisher`; the caller's contract
/// requires it to be a valid non-freed pointer.
unsafe fn with_mux_publisher(
    p: *mut TstMuxPublisher,
    f: impl FnOnce(&MuxPublisher<HlsPublisher>) -> libc::c_int,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null mux publisher pointer");
            return TstError::InvalidConfig as i32;
        };
        match &handle.inner {
            Some(mp) => f(mp),
            None => {
                set_last_error(TstError::HlsFinished, "mux publisher already finished");
                TstError::HlsFinished as i32
            }
        }
    })
}

/// Push one Annex-B NAL through the muxer's single video stream.
///
/// `pts` is the presentation timestamp in 90 kHz ticks. `key_frame` is
/// `true` for IDR / key frames; on a key frame the publisher auto-cuts a
/// new HLS segment so each segment is decodable from its first byte.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`. `nal` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_send_video(
    p: *mut TstMuxPublisher,
    nal: *const u8,
    len: usize,
    pts: i64,
    key_frame: bool,
) -> libc::c_int {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts);
    unsafe {
        with_mux_publisher(p, |mp| match mp.send_video(slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => record_mux_publisher_error(e),
        })
    }
}

/// Push one raw MISB KLV Local Set blob through a KLV stream.
///
/// **Pass raw KLV LS bytes** — for streams configured as synchronous
/// metadata the muxer prepends the 5-byte `Metadata_AU_cell` header per
/// ITU-T H.222.0 V9 §2.12.4.2; do not pre-wrap. `stream_index` selects the
/// KLV stream when multiple KLV PIDs are configured (`0` for single-stream
/// configs). `pts` is in 90 kHz ticks.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`. `klv` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_send_klv(
    p: *mut TstMuxPublisher,
    klv: *const u8,
    len: usize,
    pts: i64,
    stream_index: u8,
) -> libc::c_int {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts);
    unsafe {
        with_mux_publisher(p, |mp| match mp.send_klv(slice, pts, stream_index) {
            Ok(()) => 0,
            Err(e) => record_mux_publisher_error(e),
        })
    }
}

/// Push one audio frame buffer through the muxer's single audio stream.
///
/// `payload` is one or more pre-framed audio frames (ADTS / MPEG audio)
/// concatenated. `pts` is in 90 kHz ticks.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`. `payload` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_send_audio(
    p: *mut TstMuxPublisher,
    payload: *const u8,
    len: usize,
    pts: i64,
) -> libc::c_int {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts);
    unsafe {
        with_mux_publisher(p, |mp| match mp.send_audio(slice, pts) {
            Ok(()) => 0,
            Err(e) => record_mux_publisher_error(e),
        })
    }
}

/// Push one subtitle PES unit through the muxer's single subtitle stream.
///
/// `payload` is one complete logical subtitle unit. `pts` is in 90 kHz
/// ticks.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`. `payload` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_send_subtitle(
    p: *mut TstMuxPublisher,
    payload: *const u8,
    len: usize,
    pts: i64,
) -> libc::c_int {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts);
    unsafe {
        with_mux_publisher(p, |mp| match mp.send_subtitle(slice, pts) {
            Ok(()) => 0,
            Err(e) => record_mux_publisher_error(e),
        })
    }
}

/// Explicit segment-cut hint — start a new HLS segment on the next push.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_cut_segment(p: *mut TstMuxPublisher) -> libc::c_int {
    unsafe {
        with_mux_publisher(p, |mp| match mp.cut_segment() {
            Ok(()) => 0,
            Err(e) => record_mux_publisher_error(e),
        })
    }
}

// ---------------------------------------------------------------------------
// finish_into_publisher
// ---------------------------------------------------------------------------

/// Consume the mux publisher, flush the muxer, and return the owned HLS
/// publisher wrapped in a fresh `TstPublisher`.
///
/// The caller can then `tst_publisher_finish` the returned handle for a
/// clean `#EXT-X-ENDLIST` close, and must eventually `tst_publisher_free`
/// it. This call consumes `p` — the `TstMuxPublisher` pointer is freed
/// (do not free it again). After this the mux publisher handle is dead.
///
/// Returns `NULL` on error: `TST_E_INVALID_CONFIG` if `p` is null, or
/// `TST_E_HLS_FINISHED` if the mux publisher was already finished.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstMuxPublisher`. The returned
/// handle must eventually be freed with `tst_publisher_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_finish_into_publisher(
    p: *mut TstMuxPublisher,
) -> *mut TstPublisher {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        if p.is_null() {
            set_last_error(TstError::InvalidConfig, "null mux publisher pointer");
            return std::ptr::null_mut();
        }
        let mut boxed = unsafe { Box::from_raw(p) };
        let Some(mp) = boxed.inner.take() else {
            set_last_error(TstError::HlsFinished, "mux publisher already finished");
            return std::ptr::null_mut();
        };
        // boxed dropped here (inner now None).
        match mp.finish() {
            Ok(hls) => Box::into_raw(Box::new(TstPublisher {
                inner: Some(PublisherImpl::Hls(hls)),
            })),
            Err(e) => {
                record_mux_publisher_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Snapshot the mux-publisher cumulative stats into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// or `TST_E_HLS_FINISHED` if the mux publisher was finished.
///
/// # Safety
///
/// `p` must be a valid `*mut TstMuxPublisher`. `out` must point to a
/// writable `TstMuxPublisherStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_get_stats(
    p: *mut TstMuxPublisher,
    out: *mut TstMuxPublisherStats,
) -> libc::c_int {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe {
        with_mux_publisher(p, |mp| {
            *out = TstMuxPublisherStats::from(&mp.stats());
            0
        })
    }
}

/// Snapshot the universal publisher-side stats into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// or `TST_E_HLS_FINISHED` if the mux publisher was finished.
///
/// # Safety
///
/// `p` must be a valid `*mut TstMuxPublisher`. `out` must point to a
/// writable `TstPublisherStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_publisher_get_publisher_stats(
    p: *mut TstMuxPublisher,
    out: *mut TstPublisherStats,
) -> libc::c_int {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe {
        with_mux_publisher(p, |mp| {
            *out = TstPublisherStats::from(&mp.publisher_stats());
            0
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_free_is_safe() {
        unsafe { tst_mux_publisher_free(std::ptr::null_mut()) };
    }

    #[test]
    fn null_send_video_returns_invalid_config() {
        let nal = [0u8; 4];
        let rc = unsafe {
            tst_mux_publisher_send_video(std::ptr::null_mut(), nal.as_ptr(), 4, 0, false)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_cut_segment_returns_invalid_config() {
        let rc = unsafe { tst_mux_publisher_cut_segment(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_finish_returns_null() {
        let p = unsafe { tst_mux_publisher_finish_into_publisher(std::ptr::null_mut()) };
        assert!(p.is_null());
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstMuxPublisherStats::default();
        let rc = unsafe { tst_mux_publisher_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn with_config_null_publisher_returns_null() {
        let cfg = unsafe { crate::config::tst_mux_config_new() };
        let p = unsafe { tst_mux_publisher_with_config_hls(std::ptr::null_mut(), cfg) };
        assert!(p.is_null());
        assert_eq!(
            unsafe { crate::error::tst_get_last_error() },
            TstError::HlsConfig as i32
        );
        unsafe { crate::config::tst_mux_config_free(cfg) };
    }

    /// Arc 2 WP-B1: `record_mux_publisher_error` classifies by SOURCE through
    /// the shared `From<MuxPublisherError<E>>` instead of folding everything
    /// but `Mux` through the coarse `kind()` projection, which reported
    /// `TST_E_TRANSPORT` (-8) for BOTH `Publisher(_)` and `LockPoisoned`
    /// (`MuxPublisherError::kind()` maps both to `TransportBroken`).
    ///
    /// | variant | was | now |
    /// |---|---|---|
    /// | `Mux(e)` | the mux kind | unchanged |
    /// | `Publisher(e)` | -8 | the SINK's own kind (HLS: -34/-35/-36/-37…) |
    /// | `Closed` | -7 | unchanged |
    /// | `LockPoisoned` | -8 | **-10 `TST_E_INTERNAL`** |
    #[test]
    fn mux_publisher_error_classifies_by_source_not_the_coarse_kind() {
        use tst_pipeline::MuxPublisherError;

        // Publisher(_): the sink's own kind, not the -8 the coarse
        // `TransportBroken` projection produced.
        let rc =
            record_mux_publisher_error(MuxPublisherError::Publisher(tst_hls::HlsError::Finished));
        assert_eq!(rc, TstError::HlsFinished as i32);
        assert_eq!(unsafe { crate::error::tst_get_last_error() }, rc);

        // LockPoisoned: INTERNAL, not TRANSPORT.
        let rc = record_mux_publisher_error(MuxPublisherError::LockPoisoned);
        assert_eq!(rc, TstError::Internal as i32);

        // Closed stays -7 (it already was).
        let rc = record_mux_publisher_error(MuxPublisherError::Closed);
        assert_eq!(rc, TstError::Closed as i32);

        // Mux keeps the per-variant mux code.
        let rc = record_mux_publisher_error(MuxPublisherError::Mux(
            tst_core::error::MuxError::InvalidNal,
        ));
        assert_eq!(rc, TstError::InvalidNal as i32);
    }
}
