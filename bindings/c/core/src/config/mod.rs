//! Opaque builder handles for muxer / sender / reconnect configuration.
//!
//! Each builder is a `Box<T>`. `_open` clones the inner before consuming it,
//! so the caller may free immediately after a successful open.
//!
//! This module is split across sibling files grouped by concern:
//! - `programs` — `tst_mux_config_add_program`
//! - `streams` — per-stream constructors (video / klv / audio / subtitle /
//!   data) plus global-mux setters (PCR / PSI interval, buffer packets) and the
//!   codec / stream-type enums
//! - `descriptors` — program- and per-stream PMT descriptor entries
//! - `builders` — `TstSenderConfig` / `TstRawSenderConfig` /
//!   `TstReconnectPolicy` and their setters
//!
//! Cross-module sibling visibility is via `pub mod` (required so cbindgen
//! walks them) and the re-exports below (so external callers continue to
//! see the flat `crate::config::TypeName` path the pre-split API used).

pub mod programs;
pub mod streams;
pub mod descriptors;
// builders.rs uses tst_pipeline's reconnect/sender types which are std-only.
// Gate the module (and all its re-exports) on `std`.
#[cfg(feature = "std")]
pub mod builders;

#[cfg(feature = "std")]
pub use builders::{
    TstOverflowPolicy, TstRawSenderConfig, TstReconnectMode, TstReconnectPolicy, TstSenderConfig,
    TstTsFramingMode, tst_raw_sender_config_free, tst_raw_sender_config_new,
    tst_reconnect_policy_free, tst_reconnect_policy_new,
    tst_reconnect_policy_set_backoff_constant_ms, tst_reconnect_policy_set_backoff_exponential_ms,
    tst_reconnect_policy_set_gap_buffer_capacity, tst_reconnect_policy_set_max_attempts,
    tst_reconnect_policy_set_mode, tst_reconnect_policy_set_overflow_policy,
    tst_sender_config_free, tst_sender_config_new, tst_sender_config_set_framing_mode,
    tst_sender_config_set_max_unsynced_bytes,
};
pub use descriptors::{
    tst_mux_config_add_audio_descriptor, tst_mux_config_add_data_descriptor,
    tst_mux_config_add_klv_descriptor, tst_mux_config_add_subtitle_descriptor,
    tst_mux_config_add_video_descriptor, tst_mux_config_set_program_descriptors,
    tst_mux_config_set_stream_descriptors_for_data, tst_mux_config_set_stream_descriptors_for_klv,
    tst_mux_config_set_stream_descriptors_for_video,
};
pub use programs::tst_mux_config_add_program;
pub use streams::{
    TstAudioCodec, TstKlvStreamType, TstSubtitleCodec, TstVideoCodec,
    tst_mux_config_add_audio_stream, tst_mux_config_add_audio_stream_with_language,
    tst_mux_config_add_data_stream, tst_mux_config_add_klv_stream,
    tst_mux_config_add_subtitle_stream_cea708, tst_mux_config_add_subtitle_stream_dvb_subtitling,
    tst_mux_config_add_subtitle_stream_dvb_teletext, tst_mux_config_add_subtitle_stream_webvtt,
    tst_mux_config_add_video_stream, tst_mux_config_set_av1_carriage,
    tst_mux_config_set_buffer_packets, tst_mux_config_set_pcr_interval_ms,
    tst_mux_config_set_pcr_pid, tst_mux_config_set_psi_interval_ms,
};

use crate::panic::ffi_catch;
use alloc::boxed::Box;
use alloc::vec::Vec;
use tst_core::error::MuxError;
use tst_core::mpegts::mux::{Av1CarriageMode, MuxerConfig, MuxerProgramConfig};

// ------------------------------------------------------------------
// tst_program_handle_t
// ------------------------------------------------------------------

/// Opaque handle returned by `tst_mux_config_add_program`. Used as an
/// argument to subsequent stream-add and config-set entry points to
/// disambiguate which program's streams or descriptors are being modified.
///
/// The value is a zero-based index into the program list. Programs are
/// numbered in the order they were added. Handles are stable across the
/// config→open boundary — the same index applies after `tst_muxer_open`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TstProgramHandle(pub u32);

/// Sentinel returned by `tst_mux_config_add_program` on failure (null cfg
/// or other hard error). Check `tst_get_last_error()` for the reason.
pub const TST_INVALID_PROGRAM_HANDLE: TstProgramHandle = TstProgramHandle(u32::MAX);

// ------------------------------------------------------------------
// tst_mux_config_t
// ------------------------------------------------------------------

/// Opaque mux-config builder. Constructed via `tst_mux_config_new`,
/// populated via `tst_mux_config_add_program` + stream-add / descriptor-set
/// entry points, then consumed by `tst_*_open` (which clones the inner).
/// Caller is responsible for calling `tst_mux_config_free`.
///
/// Unlike the pre-multi-program API, the config starts **empty** — no program
/// is pre-created. The caller must call `tst_mux_config_add_program` at
/// least once before opening a muxer or sender.
pub struct TstMuxConfig {
    /// Programs accumulated so far. Each `tst_mux_config_add_program` call
    /// pushes one entry; subsequent stream-add / descriptor-set calls index
    /// into this vec by the returned `TstProgramHandle` ordinal.
    pub(crate) programs: Vec<MuxerProgramConfig>,
    /// Per-call interval overrides forwarded to `MuxerConfig` at build time.
    pub(crate) pcr_interval_ms: Option<u32>,
    pub(crate) psi_interval_ms: Option<u32>,
    pub(crate) buffer_packets: Option<usize>,
    /// AV1 PES carriage mode. `None` = use `MuxerConfig` default
    /// (`Mpeg2TsBinding`). Set via `tst_mux_config_set_av1_carriage`.
    pub(crate) av1_carriage: Option<Av1CarriageMode>,
}

impl TstMuxConfig {
    /// Finish building and return a validated `MuxerConfig`.
    ///
    /// Assembles a `MuxerConfig` from the accumulated programs and any
    /// interval / buffer overrides. The `programs` vec is cloned so the
    /// config may be opened multiple times (the C API allows `_free` after
    /// `_open`, but tests call `_open` more than once in practice).
    pub(crate) fn build_config(&self) -> Result<MuxerConfig, MuxError> {
        let mut cfg = MuxerConfig::default();
        cfg.programs = self.programs.clone();
        cfg.pcr_interval_ms = self.pcr_interval_ms.unwrap_or(40);
        cfg.psi_interval_ms = self.psi_interval_ms.unwrap_or(100);
        cfg.buffer_packets = self.buffer_packets.unwrap_or(10_000);
        if let Some(mode) = self.av1_carriage {
            cfg.av1_carriage = mode;
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Create a new, empty mux config. No programs are added — call
/// `tst_mux_config_add_program` before using this config to open a muxer
/// or sender. Returns NULL only on allocation failure (OOM).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_new() -> *mut TstMuxConfig {
    ffi_catch(core::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstMuxConfig {
            programs: Vec::new(),
            pcr_interval_ms: None,
            psi_interval_ms: None,
            buffer_packets: None,
            av1_carriage: None,
        }))
    })
}

/// Free a mux config previously returned by `tst_mux_config_new`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_free(p: *mut TstMuxConfig) {
    ffi_catch((), || {
        if !p.is_null() {
            unsafe { drop(Box::from_raw(p)) };
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::TST_INVALID_STREAM_HANDLE;

    #[test]
    fn mux_config_lifecycle() {
        unsafe {
            let p = tst_mux_config_new();
            assert!(!p.is_null());
            let prog = tst_mux_config_add_program(p, 1, 0x1000);
            assert_ne!(prog, TST_INVALID_PROGRAM_HANDLE);
            assert_eq!(
                tst_mux_config_add_video_stream(p, prog, 0x1011, TstVideoCodec::H264),
                // VideoStreamHandle::pack(0, 0) = 0
                0,
            );
            assert_ne!(
                tst_mux_config_add_klv_stream(
                    p,
                    prog,
                    0x1031,
                    TstKlvStreamType::PrivateData,
                    false
                ),
                TST_INVALID_STREAM_HANDLE,
            );
            assert_eq!(tst_mux_config_set_pcr_interval_ms(p, 30), 0);
            tst_mux_config_free(p);
        }
    }

    #[test]
    fn null_pointer_setters_return_invalid_config() {
        unsafe {
            assert_ne!(
                tst_mux_config_add_program(core::ptr::null_mut(), 1, 0x1000),
                TstProgramHandle(0),
            );
        }
    }

    #[test]
    fn add_video_stream_returns_packed_handles() {
        unsafe {
            let p = tst_mux_config_new();
            let prog = tst_mux_config_add_program(p, 1, 0x1000);
            // VideoStreamHandle::pack(0, 0) = (0<<4)|0 = 0
            let h0 = tst_mux_config_add_video_stream(p, prog, 0x1011, TstVideoCodec::H264);
            // VideoStreamHandle::pack(0, 1) = (0<<4)|1 = 1
            let h1 = tst_mux_config_add_video_stream(p, prog, 0x1012, TstVideoCodec::H265);
            assert_eq!(h0, 0);
            assert_eq!(h1, 1);
            tst_mux_config_free(p);
        }
    }

    #[test]
    fn add_klv_stream_returns_packed_handles() {
        unsafe {
            let p = tst_mux_config_new();
            let prog = tst_mux_config_add_program(p, 1, 0x1000);
            // KlvStreamHandle::pack(0, 0) = 0
            let h0 = tst_mux_config_add_klv_stream(
                p,
                prog,
                0x1031,
                TstKlvStreamType::PrivateData,
                false,
            );
            // KlvStreamHandle::pack(0, 1) = 1
            let h1 = tst_mux_config_add_klv_stream(
                p,
                prog,
                0x1032,
                TstKlvStreamType::SynchronousMetadata,
                true,
            );
            assert_eq!(h0, 0);
            assert_eq!(h1, 1);
            tst_mux_config_free(p);
        }
    }

    #[test]
    fn add_video_stream_null_returns_sentinel() {
        unsafe {
            // Null cfg: no program handle needed
            let h = tst_mux_config_add_video_stream(
                core::ptr::null_mut(),
                TstProgramHandle(0),
                0,
                TstVideoCodec::H264,
            );
            assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        }
    }

    #[test]
    fn add_video_stream_invalid_program_returns_sentinel() {
        unsafe {
            let p = tst_mux_config_new();
            // No programs added yet; any handle is invalid.
            let h = tst_mux_config_add_video_stream(
                p,
                TstProgramHandle(0),
                0x1011,
                TstVideoCodec::H264,
            );
            assert_eq!(h, TST_INVALID_STREAM_HANDLE);
            tst_mux_config_free(p);
        }
    }

    #[test]
    fn multi_program_handles_are_distinct() {
        unsafe {
            let p = tst_mux_config_new();
            let prog1 = tst_mux_config_add_program(p, 1, 0x1000);
            let prog2 = tst_mux_config_add_program(p, 2, 0x1100);
            assert_eq!(prog1, TstProgramHandle(0));
            assert_eq!(prog2, TstProgramHandle(1));

            // VideoStreamHandle::pack(0, 0) = 0, pack(1, 0) = 0x10
            let v1 = tst_mux_config_add_video_stream(p, prog1, 0x1011, TstVideoCodec::H264);
            let v2 = tst_mux_config_add_video_stream(p, prog2, 0x1111, TstVideoCodec::H265);
            assert_ne!(v1, v2);
            assert_ne!(v1, TST_INVALID_STREAM_HANDLE);
            assert_ne!(v2, TST_INVALID_STREAM_HANDLE);
            tst_mux_config_free(p);
        }
    }

    #[test]
    fn add_program_null_returns_sentinel() {
        unsafe {
            let h = tst_mux_config_add_program(core::ptr::null_mut(), 1, 0x1000);
            assert_eq!(h, TST_INVALID_PROGRAM_HANDLE);
        }
    }

    #[test]
    fn add_video_descriptor_smoke() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x100);
            let stream = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            assert_ne!(stream, TST_INVALID_STREAM_HANDLE);
            // Synthetic registration descriptor body: "VIDX" (4 bytes).
            let body = b"VIDX";
            let desc = crate::event::TstDescriptor {
                tag: 0x05,
                _reserved: [0; 7],
                data: body.as_ptr(),
                data_len: body.len(),
            };
            let rc = tst_mux_config_add_video_descriptor(cfg, stream, &desc);
            assert_eq!(rc, 0);
            // Verify: exactly one descriptor blob in stream_descriptors[0],
            // formatted as [tag=0x05, len=4, b'V', b'I', b'D', b'X'].
            let inner_cfg = &*cfg;
            assert_eq!(inner_cfg.programs[0].stream_descriptors[0].len(), 1);
            assert_eq!(
                inner_cfg.programs[0].stream_descriptors[0][0],
                &[0x05u8, 0x04, b'V', b'I', b'D', b'X']
            );
            tst_mux_config_free(cfg);
        }
    }

    #[test]
    fn add_video_descriptor_null_cfg_returns_error() {
        unsafe {
            let body = b"TEST";
            let desc = crate::event::TstDescriptor {
                tag: 0x09,
                _reserved: [0; 7],
                data: body.as_ptr(),
                data_len: body.len(),
            };
            let rc = tst_mux_config_add_video_descriptor(core::ptr::null_mut(), 0, &desc);
            assert!(rc < 0);
        }
    }

    #[test]
    fn add_klv_descriptor_smoke() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x100);
            let stream = tst_mux_config_add_klv_stream(
                cfg,
                prog,
                0x1031,
                TstKlvStreamType::PrivateData,
                false,
            );
            assert_ne!(stream, TST_INVALID_STREAM_HANDLE);
            // Empty-body descriptor (data_len = 0, data pointer need not be valid).
            let desc = crate::event::TstDescriptor {
                tag: 0xDE,
                _reserved: [0; 7],
                data: core::ptr::null(),
                data_len: 0,
            };
            let rc = tst_mux_config_add_klv_descriptor(cfg, stream, &desc);
            assert_eq!(rc, 0);
            // KLV stream is at streams[0] in this single-stream program.
            let inner_cfg = &*cfg;
            assert_eq!(inner_cfg.programs[0].stream_descriptors[0].len(), 1);
            assert_eq!(
                inner_cfg.programs[0].stream_descriptors[0][0],
                &[0xDEu8, 0x00]
            );
            tst_mux_config_free(cfg);
        }
    }
}
