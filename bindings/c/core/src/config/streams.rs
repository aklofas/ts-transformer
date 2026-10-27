//! Per-stream and global-mux configuration entry points.
//!
//! Houses the `tst_mux_config_add_*_stream*` constructors (video / klv /
//! audio / subtitle / data) plus the four global-mux setters (PCR pid, PCR
//! interval, PSI interval, buffer packets). The codec / stream-type enums
//! that parameterize these entries (`TstVideoCodec`, `TstAudioCodec`,
//! `TstSubtitleCodec`, `TstKlvStreamType`) live alongside them since they
//! are most-used here; `TstAudioCodec` and `TstSubtitleCodec` are also
//! used on `tst_event_t` payloads (via `from_core`) so they remain `pub`.

use super::{TstMuxConfig, TstProgramHandle};
use crate::demux_config::TstAv1CarriageMode;
use crate::error::{TstError, record_bad_enum, set_last_error};
use crate::handle::{
    TST_INVALID_STREAM_HANDLE, TstAudioStreamHandle, TstDataStreamHandle, TstKlvStreamHandle,
    TstSubtitleStreamHandle, TstVideoStreamHandle,
};
use crate::panic::ffi_catch;
use alloc::vec::Vec;
use tst_core::mpegts::mux::{
    AudioCodec, AudioStreamHandle, DataStreamHandle, KlvStreamHandle, KlvStreamType, StreamSpec,
    SubtitleCodec, SubtitleStreamHandle, VideoCodec, VideoStreamHandle,
};

/// Add a video elementary stream to the specified program and return its
/// handle.
///
/// The returned `tst_video_stream_handle_t` is stable across the config→open
/// boundary and across managed-sender reconnects. Pass it to
/// `tst_muxer_push_video_to` / `tst_mux_sender_send_video_to` /
/// `tst_managed_mux_sender_send_video_to` to fan out to this specific stream.
///
/// `codec` is one of the `TST_VIDEO_CODEC_*` constants.
///
/// Returns `TST_INVALID_STREAM_HANDLE` and sets last-error on: a `codec`
/// outside `TST_VIDEO_CODEC_*` (`TST_E_INVALID_CONFIG`), null `cfg`,
/// invalid `program` handle, or per-program stream cap exceeded (>16 streams
/// of any kind per program). Hard validation errors surface at `_open` time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_video_stream(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    codec: i32,
) -> TstVideoStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(codec) = TstVideoCodec::from_c_int(codec) else {
            record_bad_enum("codec", codec, "TST_VIDEO_CODEC_*");
            return TST_INVALID_STREAM_HANDLE;
        };
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TST_INVALID_STREAM_HANDLE;
        };
        let prog_idx = program.0 as usize;
        if prog_idx >= cfg.programs.len() {
            set_last_error(TstError::InvalidUsage, "invalid program handle");
            return TST_INVALID_STREAM_HANDLE;
        }
        let prog = &mut cfg.programs[prog_idx];
        // within_idx for video handles is the index among video streams only
        // (Muxer builds video_streams[prog] as a filtered subset of streams).
        let within_idx = prog
            .streams
            .iter()
            .filter(|s| matches!(s, StreamSpec::Video { .. }))
            .count();
        if within_idx >= 16 {
            // VideoStreamHandle::pack() debug_asserts within_index < 16; reject
            // before that fires so the C caller gets a defined error.
            set_last_error(
                TstError::InvalidUsage,
                "per-program video stream cap (16) exceeded",
            );
            return TST_INVALID_STREAM_HANDLE;
        }
        let rust_codec = match codec {
            TstVideoCodec::H264 => VideoCodec::H264,
            TstVideoCodec::H265 => VideoCodec::H265,
            TstVideoCodec::H266 => VideoCodec::H266,
            TstVideoCodec::Av1 => VideoCodec::Av1,
        };
        prog.streams.push(StreamSpec::Video {
            pid,
            codec: rust_codec,
        });
        prog.stream_descriptors.push(Vec::new());
        VideoStreamHandle::pack(prog_idx, within_idx).raw()
    })
}

/// Add a KLV elementary stream to the specified program and return its handle.
///
/// `stream_type`: `TST_KLV_STREAM_TYPE_PRIVATE_DATA` (0x06, async — no AU
/// cell wrapping) or `TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA` (0x15 —
/// the muxer auto-wraps each push in a 5-byte `Metadata_AU_cell` header
/// per ITU-T H.222.0 V9 § 2.12.4.2 before TS-framing; pass raw KLV LS
/// bytes to `push_klv_to`).
///
/// `carries_pts`: set `true` for synchronous KLV (PTS carried in PES header),
/// `false` for async KLV.
///
/// Returns `TST_INVALID_STREAM_HANDLE` on error: a `stream_type` outside
/// `TST_KLV_STREAM_TYPE_*` (`TST_E_INVALID_CONFIG`), or the same conditions
/// as `tst_mux_config_add_video_stream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_klv_stream(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    stream_type: i32,
    carries_pts: bool,
) -> TstKlvStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(stream_type) = TstKlvStreamType::from_c_int(stream_type) else {
            record_bad_enum("stream_type", stream_type, "TST_KLV_STREAM_TYPE_*");
            return TST_INVALID_STREAM_HANDLE;
        };
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TST_INVALID_STREAM_HANDLE;
        };
        let prog_idx = program.0 as usize;
        if prog_idx >= cfg.programs.len() {
            set_last_error(TstError::InvalidUsage, "invalid program handle");
            return TST_INVALID_STREAM_HANDLE;
        }
        let prog = &mut cfg.programs[prog_idx];
        // within_idx for klv handles is the index among klv streams only
        // (Muxer builds klv_streams[prog] as a filtered subset of streams).
        let within_idx = prog
            .streams
            .iter()
            .filter(|s| matches!(s, StreamSpec::Klv { .. }))
            .count();
        if within_idx >= 16 {
            set_last_error(
                TstError::InvalidUsage,
                "per-program klv stream cap (16) exceeded",
            );
            return TST_INVALID_STREAM_HANDLE;
        }
        let rust_stream_type = match stream_type {
            TstKlvStreamType::PrivateData => KlvStreamType::PrivateData,
            TstKlvStreamType::SynchronousMetadata => KlvStreamType::SynchronousMetadata,
        };
        prog.streams.push(StreamSpec::Klv {
            pid,
            stream_type: rust_stream_type,
            carries_pts,
        });
        prog.stream_descriptors.push(Vec::new());
        KlvStreamHandle::pack(prog_idx, within_idx).raw()
    })
}

/// Add an arbitrary private/application data elementary stream (PES
/// pass-through, the write-side dual of demux `TST_STREAM_KIND_UNKNOWN`)
/// to the specified program and return its handle.
///
/// `stream_type` is the raw PMT `stream_type` byte (e.g. 0xF0/0xF1
/// user-private, bare 0x06) — there is no enum; the byte is emitted in the
/// PMT verbatim. The `(stream_type, descriptors)` pair must classify as
/// Unknown under the demux cascade (no typed stream_type codepoints, no
/// classifying descriptors) — that anti-masquerade rule is enforced at
/// `_open` time (config validation), not here.
///
/// `carries_pts`: when `true` the PES header carries the PTS passed to
/// each `push_data_to`; when `false` the PES omits the PTS field entirely.
/// The push-time PTS is **always** used for PSI/PCR pacing decisions
/// regardless.
///
/// Returns `TST_INVALID_STREAM_HANDLE` on error (same conditions as
/// `tst_mux_config_add_video_stream`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_data_stream(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    stream_type: u8,
    carries_pts: bool,
) -> TstDataStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TST_INVALID_STREAM_HANDLE;
        };
        let prog_idx = program.0 as usize;
        if prog_idx >= cfg.programs.len() {
            set_last_error(TstError::InvalidUsage, "invalid program handle");
            return TST_INVALID_STREAM_HANDLE;
        }
        let prog = &mut cfg.programs[prog_idx];
        // within_idx for data handles is the index among data streams only
        // (Muxer builds data_streams[prog] as a filtered subset of streams).
        let within_idx = prog
            .streams
            .iter()
            .filter(|s| matches!(s, StreamSpec::Data { .. }))
            .count();
        if within_idx >= 16 {
            // DataStreamHandle::pack() debug_asserts within_index < 16; reject
            // before that fires so the C caller gets a defined error.
            set_last_error(
                TstError::InvalidUsage,
                "per-program data stream cap (16) exceeded",
            );
            return TST_INVALID_STREAM_HANDLE;
        }
        prog.streams.push(StreamSpec::Data {
            pid,
            stream_type,
            carries_pts,
        });
        prog.stream_descriptors.push(Vec::new());
        DataStreamHandle::pack(prog_idx, within_idx).raw()
    })
}

/// Add an audio elementary stream (no language tag) to the specified program
/// and return its handle.
///
/// `codec`: one of the `TstAudioCodec` variants — `Mp2` (MPEG-1 Layer II
/// audio), `Aac` (AAC in ADTS framing), `AacLatm` (AAC in LATM framing),
/// or `Ac3` (Dolby AC-3). These C enum values are `0..3` per the
/// `TstAudioCodec` definition; they are NOT the MPEG-TS PMT `stream_type`
/// codepoints — the muxer derives the appropriate `stream_type` (and any
/// required registration descriptor) from the codec choice.
///
/// The returned `tst_audio_stream_handle_t` is stable across the
/// config→open boundary and across managed-sender reconnects. Pass it to
/// `tst_muxer_push_audio_to` / `tst_mux_sender_send_audio_to` /
/// `tst_managed_mux_sender_send_audio_to` to fan out to this specific
/// stream.
///
/// `codec` is one of the `TST_AUDIO_CODEC_*` constants.
///
/// Returns `TST_INVALID_STREAM_HANDLE` and sets last-error on: a `codec`
/// outside `TST_AUDIO_CODEC_*` (`TST_E_INVALID_CONFIG`), null `cfg`,
/// invalid `program` handle, or per-program stream cap exceeded (>16 audio
/// streams per program). Hard validation errors surface at `_open` time.
///
/// Use `tst_mux_config_add_audio_stream_with_language` when you want the
/// muxer to auto-emit an ISO 639 language descriptor for this stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_audio_stream(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    codec: i32,
) -> TstAudioStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(codec) = TstAudioCodec::from_c_int(codec) else {
            record_bad_enum("codec", codec, "TST_AUDIO_CODEC_*");
            return TST_INVALID_STREAM_HANDLE;
        };
        unsafe { add_audio_stream_inner(cfg, program, pid, codec, None) }
    })
}

/// Add an audio elementary stream with an ISO 639-2 language tag.
///
/// `language` MUST be a non-null pointer to a 3-byte array of lowercase
/// ASCII bytes (e.g. `"eng"`, `"fra"`, `"spa"`). The muxer auto-emits an
/// `iso_639_language_descriptor` (tag `0x0A`) in the PMT for this stream
/// with `audio_type = 0x00` (undefined / clean main).
///
/// Passing a null `language` is rejected with `TST_E_INVALID_CONFIG` —
/// use the bare `tst_mux_config_add_audio_stream` variant when no language
/// tag is desired.
///
/// `codec` is one of the `TST_AUDIO_CODEC_*` constants.
///
/// Other failure modes match `tst_mux_config_add_audio_stream` (a `codec`
/// outside `TST_AUDIO_CODEC_*`, null `cfg`, invalid `program`, per-program
/// cap exceeded).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_audio_stream_with_language(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    codec: i32,
    language: *const u8,
) -> TstAudioStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(codec) = TstAudioCodec::from_c_int(codec) else {
            record_bad_enum("codec", codec, "TST_AUDIO_CODEC_*");
            return TST_INVALID_STREAM_HANDLE;
        };
        if language.is_null() {
            set_last_error(
                TstError::InvalidConfig,
                "language pointer must be non-null for _with_language variant",
            );
            return TST_INVALID_STREAM_HANDLE;
        }
        // SAFETY: caller documented contract — pointer to 3-byte ISO 639-2 array.
        #[expect(
            clippy::disallowed_methods,
            reason = "fixed 3-byte ISO 639-2 code, not a caller-supplied length"
        )]
        let lang = unsafe { core::slice::from_raw_parts(language, 3) };
        let mut buf = [0u8; 3];
        buf.copy_from_slice(lang);
        unsafe { add_audio_stream_inner(cfg, program, pid, codec, Some(buf)) }
    })
}

/// Shared body for both audio-stream constructors. `language` is `Some`
/// only for the `_with_language` variant.
unsafe fn add_audio_stream_inner(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    codec: TstAudioCodec,
    language: Option<[u8; 3]>,
) -> TstAudioStreamHandle {
    let Some(cfg) = (unsafe { cfg.as_mut() }) else {
        set_last_error(TstError::InvalidConfig, "null config pointer");
        return TST_INVALID_STREAM_HANDLE;
    };
    let prog_idx = program.0 as usize;
    if prog_idx >= cfg.programs.len() {
        set_last_error(TstError::InvalidUsage, "invalid program handle");
        return TST_INVALID_STREAM_HANDLE;
    }
    let prog = &mut cfg.programs[prog_idx];
    // within_idx for audio handles is the index among audio streams only
    // (Muxer builds audio_streams[prog] as a filtered subset of streams).
    let within_idx = prog
        .streams
        .iter()
        .filter(|s| matches!(s, StreamSpec::Audio { .. }))
        .count();
    if within_idx >= 16 {
        // AudioStreamHandle::pack() debug_asserts within_index < 16; reject
        // before that fires so the C caller gets a defined error.
        set_last_error(
            TstError::InvalidUsage,
            "per-program audio stream cap (16) exceeded",
        );
        return TST_INVALID_STREAM_HANDLE;
    }
    let rust_codec = match codec {
        TstAudioCodec::Mp2 => AudioCodec::Mp2,
        TstAudioCodec::Aac => AudioCodec::Aac,
        TstAudioCodec::AacLatm => AudioCodec::AacLatm,
        TstAudioCodec::Ac3 => AudioCodec::Ac3,
    };
    prog.streams.push(StreamSpec::Audio {
        pid,
        codec: rust_codec,
        language,
    });
    prog.stream_descriptors.push(Vec::new());
    AudioStreamHandle::pack(prog_idx, within_idx).raw()
}

/// Add a DVB-subtitling subtitle stream to the specified program and
/// return its handle. Drives PMT `stream_type = 0x06` with an auto-emitted
/// subtitling_descriptor (ETSI EN 300 468 §6.2.41 + ETSI EN 300 743).
///
/// `language` MUST be a non-null pointer to a 3-byte array of lowercase
/// ASCII bytes (ISO 639-2 language code, e.g. `"eng"`).
/// `subtitling_type` is per ETSI EN 300 468 Table 26 (common values:
/// 0x10 = DVB sub no AR signalling, 0x14 = DVB sub for 4:3 aspect-ratio).
/// `composition_page_id` and `ancillary_page_id` are 16-bit page
/// identifiers.
///
/// Returns `TST_INVALID_STREAM_HANDLE` on error (same conditions as
/// `tst_mux_config_add_video_stream`, plus null `language` →
/// `TST_E_INVALID_CONFIG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_subtitle_stream_dvb_subtitling(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    language: *const u8,
    subtitling_type: u8,
    composition_page_id: u16,
    ancillary_page_id: u16,
) -> TstSubtitleStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        if language.is_null() {
            set_last_error(TstError::InvalidConfig, "null language pointer");
            return TST_INVALID_STREAM_HANDLE;
        }
        // SAFETY: caller documented contract — pointer to 3-byte ISO 639-2 array.
        #[expect(
            clippy::disallowed_methods,
            reason = "fixed 3-byte ISO 639-2 code, not a caller-supplied length"
        )]
        let lang_slice = unsafe { core::slice::from_raw_parts(language, 3) };
        let mut lang = [0u8; 3];
        lang.copy_from_slice(lang_slice);
        unsafe {
            add_subtitle_stream_inner(
                cfg,
                program,
                pid,
                SubtitleCodec::DvbSubtitling {
                    language: lang,
                    subtitling_type,
                    composition_page_id,
                    ancillary_page_id,
                },
            )
        }
    })
}

/// Add a DVB-teletext subtitle stream. Drives PMT `stream_type = 0x06`
/// with an auto-emitted teletext_descriptor (ETSI EN 300 468 §6.2.43 +
/// ETSI EN 300 706).
///
/// `language` MUST be a non-null pointer to a 3-byte ISO 639-2 array.
/// `teletext_type` is 5 bits (common: 0x01 initial page, 0x02 subtitle).
/// `magazine_number` is 0..=7 (3-bit field — values outside this range
/// surface as `TST_E_INVALID_CONFIG` at `_open` time).
/// `page_number` is BCD-encoded (0x00..=0x99).
///
/// Returns `TST_INVALID_STREAM_HANDLE` on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_subtitle_stream_dvb_teletext(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    language: *const u8,
    teletext_type: u8,
    magazine_number: u8,
    page_number: u8,
) -> TstSubtitleStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        if language.is_null() {
            set_last_error(TstError::InvalidConfig, "null language pointer");
            return TST_INVALID_STREAM_HANDLE;
        }
        // SAFETY: caller documented contract — pointer to 3-byte ISO 639-2 array.
        #[expect(
            clippy::disallowed_methods,
            reason = "fixed 3-byte ISO 639-2 code, not a caller-supplied length"
        )]
        let lang_slice = unsafe { core::slice::from_raw_parts(language, 3) };
        let mut lang = [0u8; 3];
        lang.copy_from_slice(lang_slice);
        unsafe {
            add_subtitle_stream_inner(
                cfg,
                program,
                pid,
                SubtitleCodec::DvbTeletext {
                    language: lang,
                    teletext_type,
                    magazine_number,
                    page_number,
                },
            )
        }
    })
}

/// Add a CEA-708 standalone caption stream. Drives PMT
/// `stream_type = 0x06` with an auto-emitted `registration_descriptor`
/// (`format_identifier = "GA94"`). See `SubtitleCodec::Cea708Standalone`
/// for the spec caveats — this is industry convention, not a normative
/// codepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_subtitle_stream_cea708(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
) -> TstSubtitleStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || unsafe {
        add_subtitle_stream_inner(cfg, program, pid, SubtitleCodec::Cea708Standalone)
    })
}

/// Add a WebVTT-in-MPEG-TS subtitle stream. Drives PMT
/// `stream_type = 0x06` with an auto-emitted `registration_descriptor`
/// (`format_identifier = "VTTC"` — ffmpeg `mpegtsenc.c` convention
/// recognized by hls.js + mediamtx; not a normative codepoint).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_subtitle_stream_webvtt(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
) -> TstSubtitleStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || unsafe {
        add_subtitle_stream_inner(cfg, program, pid, SubtitleCodec::WebVttInTs)
    })
}

/// Shared body for the 4 subtitle-stream constructors.
unsafe fn add_subtitle_stream_inner(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
    codec: SubtitleCodec,
) -> TstSubtitleStreamHandle {
    let Some(cfg) = (unsafe { cfg.as_mut() }) else {
        set_last_error(TstError::InvalidConfig, "null config pointer");
        return TST_INVALID_STREAM_HANDLE;
    };
    let prog_idx = program.0 as usize;
    if prog_idx >= cfg.programs.len() {
        set_last_error(TstError::InvalidUsage, "invalid program handle");
        return TST_INVALID_STREAM_HANDLE;
    }
    let prog = &mut cfg.programs[prog_idx];
    // within_idx for subtitle handles is the index among subtitle streams only
    // (Muxer builds subtitle_streams[prog] as a filtered subset of streams).
    let within_idx = prog
        .streams
        .iter()
        .filter(|s| matches!(s, StreamSpec::Subtitle { .. }))
        .count();
    if within_idx >= 16 {
        // SubtitleStreamHandle::pack() debug_asserts within_index < 16; reject
        // before that fires so the C caller gets a defined error.
        set_last_error(
            TstError::InvalidUsage,
            "per-program subtitle stream cap (16) exceeded",
        );
        return TST_INVALID_STREAM_HANDLE;
    }
    prog.streams.push(StreamSpec::Subtitle { pid, codec });
    prog.stream_descriptors.push(Vec::new());
    SubtitleStreamHandle::pack(prog_idx, within_idx).raw()
}

/// Pin the PCR PID for the specified program. By default the muxer uses the
/// first video stream's PID (or first audio stream's PID if there is no video).
///
/// Returns 0 on success, or a negative `TST_E_*` code on null pointer or
/// invalid program handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_pcr_pid(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    pid: u16,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        let prog_idx = program.0 as usize;
        if prog_idx >= cfg.programs.len() {
            set_last_error(TstError::InvalidUsage, "invalid program handle");
            return TstError::InvalidUsage as i32;
        }
        cfg.programs[prog_idx].pcr_pid = Some(pid);
        0
    })
}

/// Set the PCR re-emission interval for this mux config (applies to all
/// programs). Default is 40 ms. Must be in range 1..=100.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_pcr_interval_ms(
    p: *mut TstMuxConfig,
    ms: u32,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.pcr_interval_ms = Some(ms);
        0
    })
}

/// Set the PAT/PMT re-emission interval for this mux config. Default 100 ms.
/// Must be >= 10.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_psi_interval_ms(
    p: *mut TstMuxConfig,
    ms: u32,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.psi_interval_ms = Some(ms);
        0
    })
}

/// Set the TS-packet output buffer capacity. Default 10000 (~1.88 MB).
/// Must be >= 10.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_buffer_packets(
    p: *mut TstMuxConfig,
    n: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.buffer_packets = Some(n);
        0
    })
}

/// Set the AV1 PES carriage mode for this mux config. `mode` is one of
/// `TST_AV1_CARRIAGE_MODE_MPEG2_TS_BINDING` (0, default — spec-conformant
/// per the AV1-in-MPEG-2-TS binding, OBUs wrapped in
/// `ts_open_bitstream_unit()` framing on PES `stream_id=0xBD`) or
/// `TST_AV1_CARRIAGE_MODE_INTEROP_RAW_OBU` (1 — raw OBU payload on PES
/// `stream_id=0xE0`, matching ffmpeg / libaom / hls.js / mediamtx senders).
///
/// Must match the source carriage when remuxing AV1 via
/// `tst_muxer_push_video_wire`; read the carriage from
/// `ev.u.sample.av1_carriage` on the demuxed event.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg` or
/// unrecognized `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_av1_carriage(
    cfg: *mut TstMuxConfig,
    mode: crate::c_types::c_int,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        let Some(parsed) = TstAv1CarriageMode::from_c_int(mode) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "unrecognized av1 carriage mode (valid: 0..=1)",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.av1_carriage = Some(parsed.to_rust());
        0
    })
}

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstVideoCodec {
    H264 = 0,
    H265 = 1,
    H266 = 2,
    Av1 = 3,
}

impl TstVideoCodec {
    pub(crate) fn from_core(c: tst_core::mpegts::demux::VideoCodec) -> Self {
        use tst_core::mpegts::demux::VideoCodec;
        match c {
            VideoCodec::H264 => Self::H264,
            VideoCodec::H265 => Self::H265,
            VideoCodec::H266 => Self::H266,
            VideoCodec::Av1 => Self::Av1,
        }
    }

    /// Inverse of [`Self::from_core`].
    pub(crate) fn to_core(self) -> tst_core::mpegts::demux::VideoCodec {
        use tst_core::mpegts::demux::VideoCodec;
        match self {
            Self::H264 => VideoCodec::H264,
            Self::H265 => VideoCodec::H265,
            Self::H266 => VideoCodec::H266,
            Self::Av1 => VideoCodec::Av1,
        }
    }

    /// Validate a raw C `int` against the four known discriminants.
    ///
    /// Needed because [`Self`] is received directly (by value) as a
    /// parameter on some entry points (e.g.
    /// `tst_mux_config_add_video_stream`) but that shape cannot reject an
    /// out-of-range caller value — by the time Rust code could inspect
    /// it, materializing the invalid discriminant is already undefined
    /// behavior. Entry points that need to report a bad codec value
    /// (rather than accept that risk) take a raw `c_int` and parse it
    /// through this method first — same pattern as
    /// [`crate::demux_config::TstAv1CarriageMode::from_c_int`].
    pub(crate) fn from_c_int(v: crate::c_types::c_int) -> Option<Self> {
        match v {
            0 => Some(Self::H264),
            1 => Some(Self::H265),
            2 => Some(Self::H266),
            3 => Some(Self::Av1),
            _ => None,
        }
    }
}

/// `repr(i32)` mirror of `tst_core::mpegts::demux::AudioCodec`.
/// On `tst_event_t.u.sample.codec` when `stream_kind == TST_STREAM_KIND_AUDIO`,
/// and on `tst_stream_info_t.codec`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstAudioCodec {
    Mp2 = 0,
    Aac = 1,
    AacLatm = 2,
    Ac3 = 3,
}

impl TstAudioCodec {
    pub(crate) fn from_core(c: tst_core::mpegts::demux::AudioCodec) -> Self {
        use tst_core::mpegts::demux::AudioCodec;
        match c {
            AudioCodec::Mp2 => Self::Mp2,
            AudioCodec::Aac => Self::Aac,
            AudioCodec::AacLatm => Self::AacLatm,
            AudioCodec::Ac3 => Self::Ac3,
        }
    }

    /// Validate a raw C `int32_t` against the four known discriminants
    /// (see [`TstVideoCodec::from_c_int`]).
    pub(crate) fn from_c_int(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Mp2),
            1 => Some(Self::Aac),
            2 => Some(Self::AacLatm),
            3 => Some(Self::Ac3),
            _ => None,
        }
    }
}

/// `repr(i32)` mirror of `tst_core::mpegts::demux::SubtitleCodec`.
/// On `tst_event_t.u.sample.codec` when `stream_kind == TST_STREAM_KIND_SUBTITLE`,
/// and on `tst_stream_info_t.codec`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstSubtitleCodec {
    DvbSubtitling = 0,
    DvbTeletext = 1,
    Cea708Standalone = 2,
    WebVttInTs = 3,
}

impl TstSubtitleCodec {
    pub(crate) fn from_core(c: tst_core::mpegts::demux::SubtitleCodec) -> Self {
        use tst_core::mpegts::demux::SubtitleCodec;
        match c {
            SubtitleCodec::DvbSubtitling => Self::DvbSubtitling,
            SubtitleCodec::DvbTeletext => Self::DvbTeletext,
            SubtitleCodec::Cea708Standalone => Self::Cea708Standalone,
            SubtitleCodec::WebVttInTs => Self::WebVttInTs,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum TstKlvStreamType {
    PrivateData = 0,
    SynchronousMetadata = 1,
}

impl TstKlvStreamType {
    /// Validate a raw C `int32_t` against the two known discriminants
    /// (see [`TstVideoCodec::from_c_int`]).
    pub(crate) fn from_c_int(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::PrivateData),
            1 => Some(Self::SynchronousMetadata),
            _ => None,
        }
    }
}
