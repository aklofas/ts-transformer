//! The muxer's configuration rules reach C as `TST_E_INVALID_CONFIG` from
//! the open call, and a synchronous KLV push under a metadata service the
//! stream does not declare as `TST_E_INVALID_USAGE`. Each refusal has a
//! twin that differs only in the refused value and opens.

use crate::muxing_common::drain_mux;
use tst_core::mpegts::descriptors::metadata_klva;
use tstrans::config::{
    TstAudioCodec, TstKlvStreamType, TstMuxConfig, TstProgramHandle, TstVideoCodec,
    tst_mux_config_add_audio_stream_with_language, tst_mux_config_add_data_stream,
    tst_mux_config_add_klv_stream, tst_mux_config_add_program,
    tst_mux_config_add_subtitle_stream_dvb_subtitling, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_mux_config_set_psi_interval_ms,
    tst_mux_config_set_stream_descriptors_for_klv,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::handle::TST_INVALID_STREAM_HANDLE;
use tstrans::muxer::{
    tst_muxer_close, tst_muxer_open, tst_muxer_push_klv_to, tst_muxer_push_video_to,
};

const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];
const KLV: &[u8] = &[0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01];

/// A program with an H.264 stream (the PCR PID) and whatever `add` puts
/// beside it; `true` when the muxer opened, else the last error is checked
/// to be `TST_E_INVALID_CONFIG`.
fn opens(add: impl FnOnce(*mut TstMuxConfig, TstProgramHandle)) -> bool {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32);
        add(cfg, prog);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        if mux.is_null() {
            assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
            false
        } else {
            tst_muxer_close(mux);
            true
        }
    }
}

#[test]
fn a_data_stream_of_a_section_carried_type_is_refused_at_open() {
    let with_type = |stream_type: u8| {
        opens(|cfg, prog| unsafe {
            let h = tst_mux_config_add_data_stream(cfg, prog, 0x102, stream_type, true);
            assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        })
    };
    assert!(!with_type(0x05));
    assert!(with_type(0xF0));
}

#[test]
fn an_audio_language_that_is_not_three_letters_is_refused_at_open() {
    let with_language = |language: [u8; 3]| {
        opens(|cfg, prog| unsafe {
            let h = tst_mux_config_add_audio_stream_with_language(
                cfg,
                prog,
                0x101,
                TstAudioCodec::Aac as i32,
                language.as_ptr(),
            );
            assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        })
    };
    assert!(!with_language([0, 1, 2]));
    assert!(with_language(*b"ENG"));
}

#[test]
fn a_registration_other_than_klva_on_a_klv_stream_is_refused_at_open() {
    let with_registration = |format: &[u8; 4]| {
        opens(|cfg, prog| unsafe {
            let klv = tst_mux_config_add_klv_stream(
                cfg,
                prog,
                0x103,
                TstKlvStreamType::PrivateData as i32,
                false,
            );
            let tlv = [&[0x05, 0x04][..], &format[..]].concat();
            let rc =
                tst_mux_config_set_stream_descriptors_for_klv(cfg, klv, tlv.as_ptr(), tlv.len(), 1);
            assert_eq!(rc, 0);
        })
    };
    assert!(!with_registration(b"ABCD"));
    assert!(with_registration(b"KLVA"));
}

#[test]
fn a_dvb_subtitling_type_the_standard_does_not_assign_is_refused_at_open() {
    let with_type = |subtitling_type: u8| {
        opens(|cfg, prog| unsafe {
            let h = tst_mux_config_add_subtitle_stream_dvb_subtitling(
                cfg,
                prog,
                0x104,
                b"eng".as_ptr(),
                subtitling_type,
                1,
                1,
            );
            assert_ne!(h, TST_INVALID_STREAM_HANDLE);
        })
    };
    assert!(!with_type(0x03));
    assert!(with_type(0x10));
}

#[test]
fn a_psi_interval_above_200_ms_is_refused_at_open_when_a_program_carries_klv() {
    let with = |psi_interval_ms: u32, klv: bool| {
        opens(|cfg, prog| unsafe {
            if klv {
                tst_mux_config_add_klv_stream(
                    cfg,
                    prog,
                    0x103,
                    TstKlvStreamType::PrivateData as i32,
                    false,
                );
            }
            assert_eq!(tst_mux_config_set_psi_interval_ms(cfg, psi_interval_ms), 0);
        })
    };
    assert!(!with(201, true));
    assert!(with(200, true));
    assert!(with(201, false));
}

#[test]
fn a_push_under_an_undeclared_service_is_invalid_usage() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let video = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32);
        let declared_0 = tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x103,
            TstKlvStreamType::SynchronousMetadata as i32,
            true,
        );
        let declared_3 = tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x105,
            TstKlvStreamType::SynchronousMetadata as i32,
            true,
        );
        let tlv = metadata_klva(3);
        let rc = tst_mux_config_set_stream_descriptors_for_klv(
            cfg,
            declared_3,
            tlv.as_ptr(),
            tlv.len(),
            1,
        );
        assert_eq!(rc, 0);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());
        let rc = tst_muxer_push_video_to(mux, video, NAL_IDR.as_ptr(), NAL_IDR.len(), 90_000, true);
        assert_eq!(rc, 0);
        assert!(!drain_mux(mux).is_empty());

        let rc = tst_muxer_push_klv_to(mux, declared_3, KLV.as_ptr(), KLV.len(), 90_000);
        assert_eq!(rc, TstError::InvalidUsage as i32);
        assert_eq!(tst_get_last_error(), TstError::InvalidUsage as i32);
        assert!(drain_mux(mux).is_empty(), "a refused push queues nothing");

        let rc = tst_muxer_push_klv_to(mux, declared_0, KLV.as_ptr(), KLV.len(), 90_000);
        assert_eq!(rc, 0);
        assert!(!drain_mux(mux).is_empty());
        tst_muxer_close(mux);
    }
}
