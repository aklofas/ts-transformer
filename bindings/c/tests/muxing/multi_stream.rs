//! Multi-stream `mpegts::mux` fan-out via the C ABI.

// The offline `tst_muxer_*` surface is unconditional (no feature gate);
// this test still gates on `srt` only because it links against SRT-transport
// helpers used elsewhere in the muxing test binary.
#![cfg(feature = "srt")]

use tstrans::config::{
    TstKlvStreamType, TstProgramHandle, TstVideoCodec, tst_mux_config_add_klv_stream,
    tst_mux_config_add_program, tst_mux_config_add_video_stream, tst_mux_config_free,
    tst_mux_config_new,
};
use tstrans::handle::TST_INVALID_STREAM_HANDLE;
use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_video_to};

const NAL_SPS: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02, 0x80, 0xf6, 0xc0,
];

#[test]
fn muxer_push_video_to_routes_to_correct_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_eo = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_ir = tst_mux_config_add_video_stream(cfg, prog, 0x1012, TstVideoCodec::H264);
        let h_klv =
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
        assert_ne!(h_eo, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h_ir, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h_klv, TST_INVALID_STREAM_HANDLE);

        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        let rc_eo = tst_muxer_push_video_to(mux, h_eo, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc_eo, 0);
        let rc_ir = tst_muxer_push_video_to(mux, h_ir, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc_ir, 0);

        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n > 0, "muxer produced no output");

        tst_muxer_close(mux);
    }
}

#[test]
fn muxer_push_video_to_invalid_handle_returns_invalid_usage() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Handle 99 was never added.
        let rc = tst_muxer_push_video_to(mux, 99, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, -9 /* TST_E_INVALID_USAGE */);

        tst_muxer_close(mux);
    }
}

#[test]
fn muxer_push_video_to_forged_high_bit_handle_returns_invalid_usage() {
    // A raw handle with high bits set outside
    // the canonical 4-bit program + 4-bit within layout MUST be rejected
    // — not silently masked into a valid low-byte handle. Without
    // `try_from_raw`, `valid.raw() | 0x100` aliases the genuine handle
    // because `unpack` strips bit 8 and the push-time range check only
    // sees the masked indices.
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        assert_ne!(h_video, TST_INVALID_STREAM_HANDLE);

        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // The legitimate handle pushes successfully.
        let rc_valid =
            tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc_valid, 0, "valid handle must push successfully");

        // Forge: set bit 8 (the first reserved bit above the canonical
        // 8-bit layout). The low byte still equals h_video — under the
        // pre-fix masking behavior, this would silently route the push
        // to h_video's stream. With validation, it must be rejected.
        let forged = h_video | 0x100;
        let rc_forged =
            tst_muxer_push_video_to(mux, forged, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(
            rc_forged, -9, /* TST_E_INVALID_USAGE */
            "forged handle (raw | 0x100) must be rejected, not aliased to the valid handle"
        );

        tst_muxer_close(mux);
    }
}

#[test]
fn add_program_invalid_handle_returns_sentinel() {
    unsafe {
        let cfg = tst_mux_config_new();
        // No programs added — TstProgramHandle(0) is invalid.
        let h =
            tst_mux_config_add_video_stream(cfg, TstProgramHandle(0), 0x1011, TstVideoCodec::H264);
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        tst_mux_config_free(cfg);
    }
}
