//! The four PMT descriptor setters clear a list only when `tlv_count` and
//! `tlv_total_len` are both zero; exactly one of them zero is refused and
//! leaves the stored list untouched. Observed through the PMT bytes.

use std::ffi::CStr;

use tstrans::config::{
    TstKlvStreamType, TstMuxConfig, TstProgramHandle, TstVideoCodec,
    tst_mux_config_add_data_stream, tst_mux_config_add_klv_stream, tst_mux_config_add_program,
    tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
    tst_mux_config_set_program_descriptors, tst_mux_config_set_stream_descriptors_for_data,
    tst_mux_config_set_stream_descriptors_for_klv, tst_mux_config_set_stream_descriptors_for_video,
};
use tstrans::error::{TstError, tst_get_last_error, tst_get_last_error_str};
use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_video_to};

const NAL_SPS: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02, 0x80, 0xf6, 0xc0,
];

// User-private tags, so no stream-kind classification rule objects to them.
const PROGRAM_TLV: &[u8] = &[0xA1, 0x03, 0x11, 0x22, 0x33];
// One distinct TLV per stream so a survive-assertion can only be met by
// that stream's own PMT entry.
const VIDEO_TLV: &[u8] = &[0xA0, 0x04, 0xDE, 0xAD, 0xBE, 0xEF];
const KLV_TLV: &[u8] = &[0xA2, 0x04, 0xC0, 0xFF, 0xEE, 0x01];
const DATA_TLV: &[u8] = &[0xA3, 0x04, 0xFA, 0xCE, 0xB0, 0x0C];

type Setter = unsafe fn(*mut TstMuxConfig, u32, *const u8, usize, usize) -> i32;

struct Fixture {
    cfg: *mut TstMuxConfig,
    video: u32,
    handles: [(&'static str, u32, Setter, &'static [u8]); 4],
}

unsafe fn set_program(cfg: *mut TstMuxConfig, h: u32, p: *const u8, len: usize, n: usize) -> i32 {
    unsafe { tst_mux_config_set_program_descriptors(cfg, TstProgramHandle(h as _), p, len, n) }
}
unsafe fn set_video(cfg: *mut TstMuxConfig, h: u32, p: *const u8, len: usize, n: usize) -> i32 {
    unsafe { tst_mux_config_set_stream_descriptors_for_video(cfg, h, p, len, n) }
}
unsafe fn set_klv(cfg: *mut TstMuxConfig, h: u32, p: *const u8, len: usize, n: usize) -> i32 {
    unsafe { tst_mux_config_set_stream_descriptors_for_klv(cfg, h, p, len, n) }
}
unsafe fn set_data(cfg: *mut TstMuxConfig, h: u32, p: *const u8, len: usize, n: usize) -> i32 {
    unsafe { tst_mux_config_set_stream_descriptors_for_data(cfg, h, p, len, n) }
}

unsafe fn fixture() -> Fixture {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        let klv = tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x1021,
            TstKlvStreamType::PrivateData as i32,
            false,
        );
        let data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        Fixture {
            cfg,
            video,
            handles: [
                ("program", prog.0 as u32, set_program, PROGRAM_TLV),
                ("video", video, set_video, VIDEO_TLV),
                ("klv", klv, set_klv, KLV_TLV),
                ("data", data, set_data, DATA_TLV),
            ],
        }
    }
}

unsafe fn pmt_bytes(f: &Fixture) -> Vec<u8> {
    unsafe {
        let mux = tst_muxer_open(f.cfg);
        assert!(!mux.is_null(), "tst_muxer_open failed");
        let rc = tst_muxer_push_video_to(mux, f.video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0);
        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        buf.truncate(n);
        tst_muxer_close(mux);
        buf
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

unsafe fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

#[test]
fn tlv_setters_refuse_a_one_zero_mismatch_without_mutating() {
    unsafe {
        let f = fixture();
        for (name, h, set, tlv) in f.handles {
            assert_eq!(set(f.cfg, h, tlv.as_ptr(), tlv.len(), 1), 0, "{name}");
        }
        let before = pmt_bytes(&f);
        for (name, _, _, tlv) in f.handles {
            assert!(contains(&before, tlv), "{name}: descriptor must be set");
        }

        for (name, h, set, tlv) in f.handles {
            // count 1, no bytes.
            let rc = set(f.cfg, h, core::ptr::null(), 0, 1);
            assert_eq!(rc, TstError::InvalidConfig as i32, "{name} (1, 0)");
            assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
            let msg = last_error_msg();
            assert!(
                msg.contains("tlv_count") && msg.contains("tlv_total_len"),
                "{name}: {msg}"
            );
            // zero count over real bytes.
            let rc = set(f.cfg, h, tlv.as_ptr(), tlv.len(), 0);
            assert_eq!(rc, TstError::InvalidConfig as i32, "{name} (0, len)");
            let msg = last_error_msg();
            assert!(
                msg.contains("tlv_count") && msg.contains("tlv_total_len"),
                "{name}: {msg}"
            );
        }
        let after = pmt_bytes(&f);
        for (name, _, _, tlv) in f.handles {
            assert!(contains(&after, tlv), "{name}: descriptor must survive");
        }
        tst_mux_config_free(f.cfg);
    }
}

#[test]
fn tlv_setters_clear_on_zero_count_and_zero_length() {
    unsafe {
        let f = fixture();
        for (name, h, set, tlv) in f.handles {
            assert_eq!(set(f.cfg, h, tlv.as_ptr(), tlv.len(), 1), 0, "{name}");
        }
        for (name, h, set, _) in f.handles {
            assert_eq!(set(f.cfg, h, core::ptr::null(), 0, 0), 0, "{name}");
        }
        let ts = pmt_bytes(&f);
        for (name, _, _, tlv) in f.handles {
            assert!(!contains(&ts, tlv), "{name}: descriptor must be cleared");
        }
        tst_mux_config_free(f.cfg);
    }
}
