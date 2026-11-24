//! `tst_mux_config_set_transport_stream_id` and
//! `tst_mux_config_set_psi_version_base`: the PAT's transport stream id and
//! the version the PAT and every PMT start at.

use crate::muxing_common::drain_mux;
use tstrans::config::{
    TstMuxConfig, TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_mux_config_set_psi_version_base,
    tst_mux_config_set_transport_stream_id,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::handle::TstVideoStreamHandle;
use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_push_video_to};

const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];
const PMT_PID: u16 = 0x1000;

/// `(transport_stream_id or program_number, version_number)` of the first
/// section that starts on `pid` (H.222.0 §2.4.4.3 and §2.4.4.8: both read
/// the 16 bits after `section_length`, then the version byte).
fn first_section_id_and_version(ts: &[u8], pid: u16) -> (u16, u8) {
    let p = ts
        .chunks_exact(188)
        .find(|p| p[1] & 0x40 != 0 && ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == pid)
        .expect("a section start on the PID");
    let payload = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    let s = payload + 1 + p[payload] as usize;
    (
        (p[s + 3] as u16) << 8 | p[s + 4] as u16,
        (p[s + 5] >> 1) & 0x1F,
    )
}

/// One H.264 program on PMT PID `0x1000`; `set` runs on the config before
/// the open.
unsafe fn one_program_config(
    set: impl FnOnce(*mut TstMuxConfig),
) -> (*mut TstMuxConfig, TstVideoStreamHandle) {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, PMT_PID);
        let video = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32);
        set(cfg);
        (cfg, video)
    }
}

/// The first PAT's `(transport_stream_id, version)` and the first PMT's
/// version of a muxer that pushed one key frame.
fn first_tables(set: impl FnOnce(*mut TstMuxConfig)) -> ((u16, u8), u8) {
    unsafe {
        let (cfg, video) = one_program_config(set);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());
        let rc = tst_muxer_push_video_to(mux, video, NAL_IDR.as_ptr(), NAL_IDR.len(), 90_000, true);
        assert_eq!(rc, 0);
        let ts = drain_mux(mux);
        tst_muxer_close(mux);
        let pat = first_section_id_and_version(&ts, 0x0000);
        let (program_number, pmt_version) = first_section_id_and_version(&ts, PMT_PID);
        assert_eq!(program_number, 1);
        (pat, pmt_version)
    }
}

#[test]
fn the_transport_stream_id_and_version_base_reach_the_wire() {
    let (pat, pmt_version) = first_tables(|cfg| unsafe {
        assert_eq!(tst_mux_config_set_transport_stream_id(cfg, 0x1234), 0);
        assert_eq!(tst_mux_config_set_psi_version_base(cfg, 7), 0);
    });
    assert_eq!(pat, (0x1234, 7));
    assert_eq!(pmt_version, 7);
}

#[test]
fn the_defaults_are_1_and_0() {
    assert_eq!(first_tables(|_| {}), ((0x0001, 0), 0));
}

#[test]
fn every_transport_stream_id_and_the_top_version_base_open() {
    assert_eq!(
        first_tables(|cfg| unsafe {
            assert_eq!(tst_mux_config_set_transport_stream_id(cfg, 0xFFFF), 0);
            assert_eq!(tst_mux_config_set_psi_version_base(cfg, 31), 0);
        }),
        ((0xFFFF, 31), 31)
    );
    assert_eq!(
        first_tables(|cfg| unsafe {
            assert_eq!(tst_mux_config_set_transport_stream_id(cfg, 0), 0);
        }),
        ((0x0000, 0), 0)
    );
}

#[test]
fn a_version_base_above_31_is_refused_at_open() {
    for base in [32u8, 255] {
        unsafe {
            let (cfg, _) = one_program_config(|cfg| {
                assert_eq!(tst_mux_config_set_psi_version_base(cfg, base), 0);
            });
            assert!(tst_muxer_open(cfg).is_null(), "base {base}");
            assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
            tst_mux_config_free(cfg);
        }
    }
}

#[test]
fn null_config_is_invalid_config() {
    unsafe {
        assert_eq!(
            tst_mux_config_set_transport_stream_id(core::ptr::null_mut(), 1),
            TstError::InvalidConfig as i32
        );
        assert_eq!(
            tst_mux_config_set_psi_version_base(core::ptr::null_mut(), 0),
            TstError::InvalidConfig as i32
        );
    }
}
