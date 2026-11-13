//! `tst_mux_config_set_pcr_lead_ms`: the muxer's PCR runs the configured
//! lead behind each access unit's decode time.

use crate::muxing_common::drain_mux;
use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_mux_config_set_pcr_lead_ms,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_push_video_to};

const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];

/// 33-bit PCR base of the first packet whose adaptation field carries a PCR
/// (H.222.0 §2.4.3.4).
fn first_pcr_base(ts: &[u8]) -> u64 {
    let p = ts
        .chunks_exact(188)
        .find(|p| p[3] & 0x20 != 0 && p[4] > 0 && p[5] & 0x10 != 0)
        .expect("a packet with a PCR");
    (p[6] as u64) << 25
        | (p[7] as u64) << 17
        | (p[8] as u64) << 9
        | (p[9] as u64) << 1
        | (p[10] as u64) >> 7
}

/// First PCR of a muxer that pushed one key frame at PTS 90 000, with the
/// lead left at its default (`None`) or set.
fn first_pcr_with_lead(lead_ms: Option<u32>) -> u64 {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let video = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32);
        if let Some(ms) = lead_ms {
            assert_eq!(tst_mux_config_set_pcr_lead_ms(cfg, ms), 0);
        }
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());
        let rc = tst_muxer_push_video_to(mux, video, NAL_IDR.as_ptr(), NAL_IDR.len(), 90_000, true);
        assert_eq!(rc, 0);
        let ts = drain_mux(mux);
        tst_muxer_close(mux);
        first_pcr_base(&ts)
    }
}

#[test]
fn pcr_lead_defaults_to_100_ms_and_is_settable() {
    assert_eq!(first_pcr_with_lead(None), 81_000);
    assert_eq!(first_pcr_with_lead(Some(0)), 90_000);
    assert_eq!(first_pcr_with_lead(Some(250)), 67_500);
}

#[test]
fn pcr_lead_over_1000_is_rejected_at_open() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32);
        assert_eq!(tst_mux_config_set_pcr_lead_ms(cfg, 1001), 0);
        assert!(tst_muxer_open(cfg).is_null());
        assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
        assert_eq!(tst_mux_config_set_pcr_lead_ms(cfg, 1000), 0);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_muxer_close(mux);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn null_config_is_invalid_config() {
    let rc = unsafe { tst_mux_config_set_pcr_lead_ms(core::ptr::null_mut(), 100) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}
