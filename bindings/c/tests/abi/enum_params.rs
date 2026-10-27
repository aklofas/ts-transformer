//! Enum-valued parameters arrive as `int32_t` and are validated.
//!
//! A Rust enum parameter holding an out-of-range value is undefined
//! behaviour before the function body runs, so the eight entry points that
//! take a `TST_*` constant receive a plain `int32_t` and reject a value
//! outside the constant family with `TST_E_INVALID_CONFIG`.

use tstrans::config::{
    TstAudioCodec, TstKlvStreamType, TstOverflowPolicy, TstReconnectMode, TstTsFramingMode,
    TstVideoCodec, tst_mux_config_add_audio_stream, tst_mux_config_add_audio_stream_with_language,
    tst_mux_config_add_klv_stream, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_reconnect_policy_free, tst_reconnect_policy_new,
    tst_reconnect_policy_set_mode, tst_reconnect_policy_set_overflow_policy,
    tst_sender_config_free, tst_sender_config_new, tst_sender_config_set_framing_mode,
};
use tstrans::error::{TstError, test_clear_last_error, test_last_error_code, test_last_error_msg};
use tstrans::handle::TST_INVALID_STREAM_HANDLE;
use tstrans::misp_time::tst_misp_time_extract;

fn assert_bad_enum(msg: &str) {
    assert_eq!(test_last_error_code(), TstError::InvalidConfig as i32);
    assert_eq!(test_last_error_msg(), msg);
    test_clear_last_error();
}

/// An out-of-range enum value from C is a configuration error, never an
/// invalid Rust discriminant.
#[test]
fn out_of_range_enum_values_are_invalid_config() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);

        assert_eq!(
            tst_mux_config_add_video_stream(cfg, prog, 0x100, 99),
            TST_INVALID_STREAM_HANDLE
        );
        assert_bad_enum("codec 99 is not a TST_VIDEO_CODEC_* value");

        assert_eq!(
            tst_mux_config_add_klv_stream(cfg, prog, 0x101, 2, true),
            TST_INVALID_STREAM_HANDLE
        );
        assert_bad_enum("stream_type 2 is not a TST_KLV_STREAM_TYPE_* value");

        assert_eq!(
            tst_mux_config_add_audio_stream(cfg, prog, 0x102, -1),
            TST_INVALID_STREAM_HANDLE
        );
        assert_bad_enum("codec -1 is not a TST_AUDIO_CODEC_* value");

        assert_eq!(
            tst_mux_config_add_audio_stream_with_language(cfg, prog, 0x103, 4, b"eng".as_ptr()),
            TST_INVALID_STREAM_HANDLE
        );
        assert_bad_enum("codec 4 is not a TST_AUDIO_CODEC_* value");
        tst_mux_config_free(cfg);

        let sc = tst_sender_config_new();
        assert_eq!(
            tst_sender_config_set_framing_mode(sc, 2),
            TstError::InvalidConfig as i32
        );
        assert_bad_enum("mode 2 is not a TST_TS_FRAMING_MODE_* value");
        tst_sender_config_free(sc);

        let rp = tst_reconnect_policy_new();
        assert_eq!(
            tst_reconnect_policy_set_overflow_policy(rp, 2),
            TstError::InvalidConfig as i32
        );
        assert_bad_enum("policy 2 is not a TST_OVERFLOW_POLICY_* value");
        assert_eq!(
            tst_reconnect_policy_set_mode(rp, 2),
            TstError::InvalidConfig as i32
        );
        assert_bad_enum("mode 2 is not a TST_RECONNECT_MODE_* value");
        tst_reconnect_policy_free(rp);

        let au = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0x88];
        let (mut kind, mut status, mut value) = (0u8, 0u8, 0u64);
        assert_eq!(
            tst_misp_time_extract(au.as_ptr(), au.len(), 7, &mut kind, &mut status, &mut value),
            TstError::InvalidConfig as i32
        );
        assert_bad_enum("codec 7 is not a TST_VIDEO_CODEC_* value");
    }
}

/// Every named constant is still accepted after the retype to `int32_t`.
#[test]
fn every_named_enum_value_is_still_accepted() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let mut pid = 0x100;
        for codec in [
            TstVideoCodec::H264,
            TstVideoCodec::H265,
            TstVideoCodec::H266,
            TstVideoCodec::Av1,
        ] {
            let h = tst_mux_config_add_video_stream(cfg, prog, pid, codec as i32);
            assert_ne!(h, TST_INVALID_STREAM_HANDLE, "video codec {}", codec as i32);
            pid += 1;
        }
        for codec in [
            TstAudioCodec::Mp2,
            TstAudioCodec::Aac,
            TstAudioCodec::AacLatm,
            TstAudioCodec::Ac3,
        ] {
            let h = tst_mux_config_add_audio_stream(cfg, prog, pid, codec as i32);
            assert_ne!(h, TST_INVALID_STREAM_HANDLE, "audio codec {}", codec as i32);
            pid += 1;
            let h = tst_mux_config_add_audio_stream_with_language(
                cfg,
                prog,
                pid,
                codec as i32,
                b"eng".as_ptr(),
            );
            assert_ne!(h, TST_INVALID_STREAM_HANDLE, "audio codec {}", codec as i32);
            pid += 1;
        }
        for st in [
            TstKlvStreamType::PrivateData,
            TstKlvStreamType::SynchronousMetadata,
        ] {
            let h = tst_mux_config_add_klv_stream(cfg, prog, pid, st as i32, true);
            assert_ne!(
                h, TST_INVALID_STREAM_HANDLE,
                "klv stream_type {}",
                st as i32
            );
            pid += 1;
        }
        tst_mux_config_free(cfg);

        let sc = tst_sender_config_new();
        for mode in [TstTsFramingMode::Recover, TstTsFramingMode::Strict] {
            assert_eq!(tst_sender_config_set_framing_mode(sc, mode as i32), 0);
        }
        tst_sender_config_free(sc);

        let rp = tst_reconnect_policy_new();
        for policy in [TstOverflowPolicy::DropOldest, TstOverflowPolicy::Reject] {
            assert_eq!(
                tst_reconnect_policy_set_overflow_policy(rp, policy as i32),
                0
            );
        }
        for mode in [TstReconnectMode::Blocking, TstReconnectMode::Background] {
            assert_eq!(tst_reconnect_policy_set_mode(rp, mode as i32), 0);
        }
        tst_reconnect_policy_free(rp);

        // An AU without a MISP SEI returns 1 (absent) for every codec.
        let au = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0x88];
        let (mut kind, mut status, mut value) = (0u8, 0u8, 0u64);
        for codec in [
            TstVideoCodec::H264,
            TstVideoCodec::H265,
            TstVideoCodec::H266,
            TstVideoCodec::Av1,
        ] {
            let rc = tst_misp_time_extract(
                au.as_ptr(),
                au.len(),
                codec as i32,
                &mut kind,
                &mut status,
                &mut value,
            );
            assert_eq!(rc, 1, "misp codec {}", codec as i32);
        }
    }
}
