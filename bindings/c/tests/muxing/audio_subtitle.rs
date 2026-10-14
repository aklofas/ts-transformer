//! Sender-side audio + subtitle C ABI.
//!
//! Exercises the new constructor functions
//! (`tst_mux_config_add_audio_stream{,_with_language}` +
//! `tst_mux_config_add_subtitle_stream_*`) plus the muxer-side push
//! entries (`tst_muxer_push_audio[_to]`, `tst_muxer_push_subtitle[_to]`).
//!
//! SRT-wrapped `tst_mux_sender_send_*` tests are out of scope here —
//! they need listener/sender boilerplate and live in `tests/url_open.rs`.

// The offline `tst_muxer_*` surface is unconditional, so this file runs on
// every feature plane including the default (no-transport) build.

use tstrans::config::{
    TstAudioCodec, TstProgramHandle, TstVideoCodec, tst_mux_config_add_audio_stream,
    tst_mux_config_add_audio_stream_with_language, tst_mux_config_add_program,
    tst_mux_config_add_subtitle_stream_cea708, tst_mux_config_add_subtitle_stream_dvb_subtitling,
    tst_mux_config_add_subtitle_stream_dvb_teletext, tst_mux_config_add_subtitle_stream_webvtt,
    tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
};
use tstrans::handle::TST_INVALID_STREAM_HANDLE;
use tstrans::muxer::{
    tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_audio, tst_muxer_push_audio_to,
    tst_muxer_push_subtitle, tst_muxer_push_subtitle_to, tst_muxer_push_video_to,
};

const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xBB, 0xBB, 0xBB];

/// Synthetic ADTS-shaped frame: 7-byte header + 9 bytes of zero-filled payload.
/// Not a decoder-valid AAC frame; sufficient to exercise PES framing.
const SYNTHETIC_ADTS: &[u8] = &[
    0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// ----------------------------------------------------------------------------
// Audio constructor — happy path + null-cfg sentinel
// ----------------------------------------------------------------------------

#[test]
fn add_audio_stream_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        // First audio stream in a program — packed handle is 0.
        let h = tst_mux_config_add_audio_stream(cfg, prog, 0x1041, TstAudioCodec::Aac);
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        assert_eq!(h, 0);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_audio_stream_with_language_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let lang = b"eng";
        let h = tst_mux_config_add_audio_stream_with_language(
            cfg,
            prog,
            0x1041,
            TstAudioCodec::Mp2,
            lang.as_ptr(),
        );
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_audio_stream_with_language_null_lang_returns_sentinel() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h = tst_mux_config_add_audio_stream_with_language(
            cfg,
            prog,
            0x1041,
            TstAudioCodec::Aac,
            std::ptr::null(),
        );
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        // Last error should be InvalidConfig.
        let code = tstrans::error::tst_get_last_error();
        assert_eq!(code, tstrans::error::TstError::InvalidConfig as i32);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_audio_stream_null_cfg_returns_sentinel() {
    unsafe {
        let h = tst_mux_config_add_audio_stream(
            std::ptr::null_mut(),
            TstProgramHandle(0),
            0x1041,
            TstAudioCodec::Aac,
        );
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
    }
}

// ----------------------------------------------------------------------------
// Subtitle constructors — happy path for each of the 4 variants
// ----------------------------------------------------------------------------

#[test]
fn add_subtitle_stream_dvb_subtitling_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let lang = b"eng";
        let h = tst_mux_config_add_subtitle_stream_dvb_subtitling(
            cfg,
            prog,
            0x1051,
            lang.as_ptr(),
            0x10, // subtitling_type: DVB sub no AR
            0x01, // composition_page_id
            0x02, // ancillary_page_id
        );
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_subtitle_stream_dvb_subtitling_null_lang_returns_sentinel() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h = tst_mux_config_add_subtitle_stream_dvb_subtitling(
            cfg,
            prog,
            0x1051,
            std::ptr::null(),
            0x10,
            0x01,
            0x02,
        );
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        let code = tstrans::error::tst_get_last_error();
        assert_eq!(code, tstrans::error::TstError::InvalidConfig as i32);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_subtitle_stream_dvb_teletext_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let lang = b"fra";
        let h = tst_mux_config_add_subtitle_stream_dvb_teletext(
            cfg,
            prog,
            0x1052,
            lang.as_ptr(),
            0x02, // teletext_type: subtitle
            0,    // magazine_number (magazine 8 wraps to 0)
            0x88, // page_number (BCD 88)
        );
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_subtitle_stream_cea708_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h = tst_mux_config_add_subtitle_stream_cea708(cfg, prog, 0x1053);
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_subtitle_stream_webvtt_returns_valid_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h = tst_mux_config_add_subtitle_stream_webvtt(cfg, prog, 0x1054);
        assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}

// ----------------------------------------------------------------------------
// Push smoke tests — verify the muxer emits TS bytes after audio/subtitle push
// ----------------------------------------------------------------------------

#[test]
fn push_audio_to_emits_ts_bytes() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        // Video is needed to satisfy PCR-source resolution; audio cadence
        // alone isn't always enough.
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_audio = tst_mux_config_add_audio_stream(cfg, prog, 0x1041, TstAudioCodec::Aac);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Prime with one video IDR so PCR/PSI are established.
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);
        assert_eq!(rc, 0);
        // Now push an audio frame.
        let rc = tst_muxer_push_audio_to(
            mux,
            h_audio,
            SYNTHETIC_ADTS.as_ptr(),
            SYNTHETIC_ADTS.len(),
            900,
        );
        assert_eq!(rc, 0);

        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n >= 188, "expected at least one TS packet, got {n}");
        assert_eq!(buf[0], 0x47);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_subtitle_to_emits_ts_bytes() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_sub = tst_mux_config_add_subtitle_stream_webvtt(cfg, prog, 0x1054);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Prime with one video IDR.
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);
        assert_eq!(rc, 0);
        // Push a tiny WebVTT cue.
        let cue = b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello\n";
        let rc = tst_muxer_push_subtitle_to(mux, h_sub, cue.as_ptr(), cue.len(), 900);
        assert_eq!(rc, 0);

        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n >= 188, "expected at least one TS packet, got {n}");
        assert_eq!(buf[0], 0x47);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_audio_emits_ts_bytes() {
    // Bare single-stream shorthand (no handle arg) — resolves only when
    // exactly one audio stream is configured across all programs. Mirrors
    // push_audio_to_emits_ts_bytes minus the handle-capture + handle param.
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        // Video is needed to satisfy PCR-source resolution; audio cadence
        // alone isn't always enough.
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_audio_stream(cfg, prog, 0x1041, TstAudioCodec::Aac);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Prime with one video IDR so PCR/PSI are established.
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);
        assert_eq!(rc, 0);
        // Now push an audio frame via the bare shorthand.
        let rc = tst_muxer_push_audio(mux, SYNTHETIC_ADTS.as_ptr(), SYNTHETIC_ADTS.len(), 900);
        assert_eq!(rc, 0);

        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n >= 188, "expected at least one TS packet, got {n}");
        assert_eq!(buf[0], 0x47);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_subtitle_emits_ts_bytes() {
    // Bare single-stream shorthand (no handle arg) — resolves only when
    // exactly one subtitle stream is configured across all programs.
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_subtitle_stream_webvtt(cfg, prog, 0x1054);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Prime with one video IDR.
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);
        assert_eq!(rc, 0);
        // Push a tiny WebVTT cue via the bare shorthand.
        let cue = b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello\n";
        let rc = tst_muxer_push_subtitle(mux, cue.as_ptr(), cue.len(), 900);
        assert_eq!(rc, 0);

        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n >= 188, "expected at least one TS packet, got {n}");
        assert_eq!(buf[0], 0x47);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_audio_to_invalid_handle_returns_invalid_usage() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        // Add audio so push_audio_to with handle 99 fails at handle lookup
        // (not at "no audio streams configured").
        tst_mux_config_add_audio_stream(cfg, prog, 0x1041, TstAudioCodec::Aac);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Prime with video so the muxer is "warm".
        let _ = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);

        // Handle 99 was never added.
        let rc =
            tst_muxer_push_audio_to(mux, 99, SYNTHETIC_ADTS.as_ptr(), SYNTHETIC_ADTS.len(), 900);
        assert_eq!(rc, -9 /* TST_E_INVALID_USAGE */);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_subtitle_to_invalid_handle_returns_invalid_usage() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_subtitle_stream_webvtt(cfg, prog, 0x1054);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        let _ = tst_muxer_push_video_to(mux, h_video, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);

        let cue = b"x";
        let rc = tst_muxer_push_subtitle_to(mux, 99, cue.as_ptr(), cue.len(), 900);
        assert_eq!(rc, -9 /* TST_E_INVALID_USAGE */);
        tst_muxer_close(mux);
    }
}
