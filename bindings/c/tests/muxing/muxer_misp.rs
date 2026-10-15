//! MISP timestamp push + extract entry points for the C ABI (ABI 19).
//!
//! Builds a single-stream H.264 muxer via the C ABI, pushes one IDR access
//! unit with a microsecond MISP timestamp via `tst_muxer_push_video_misp_to`,
//! and verifies the round-trip via `tst_misp_time_extract` on the demuxed AU.
//! Also covers:
//!   - `tst_muxer_push_video_misp_to_with_dts` emits PTS_DTS_flags=0b11.
//!   - `misp_kind=2` (out of range) returns `TST_E_MISP_TIME` (-45).
//!   - `tst_misp_time_extract` on an access unit without a MISP SEI returns 1.

use crate::muxing_common::{drain_mux, find_pes_start};
use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::TstError;
use tstrans::handle::TstVideoStreamHandle;
use tstrans::misp_time::tst_misp_time_extract;
use tstrans::muxer::{
    TstMuxer, tst_muxer_close, tst_muxer_open, tst_muxer_push_video_misp_to,
    tst_muxer_push_video_misp_to_with_dts,
};

/// Minimal Annex-B IDR NAL unit (H.264, nal_unit_type=5).
const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];

/// Build a minimal H.264 muxer and return `(mux_ptr, video_handle)`.
///
/// Caller is responsible for calling `tst_muxer_close(mux_ptr)` when done.
unsafe fn open_h264_muxer() -> (*mut TstMuxer, TstVideoStreamHandle) {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let handle = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null(), "tst_muxer_open returned null");
        (mux, handle)
    }
}

/// Demux the TS stream to extract the video AU bytes for a given PID.
/// Returns the raw AU bytes from the first video PES packet found.
fn extract_au_from_ts(ts_bytes: &[u8], pid: u16) -> Vec<u8> {
    // Collect payload bytes from TS packets on `pid` that have PUSI set.
    // For a simple IDR-only mux the AU fits in the first PES packet.
    let mut au = Vec::new();
    let mut found_pes = false;
    for pkt in ts_bytes.chunks(188) {
        if pkt[0] != 0x47 {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        let pkt_pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        if pkt_pid != pid {
            continue;
        }
        let afc = (pkt[3] >> 4) & 0x03;
        let payload_offset = match afc {
            1 => 4,
            3 => 4 + 1 + pkt[4] as usize,
            _ => continue,
        };
        if payload_offset >= 188 {
            continue;
        }
        let payload = &pkt[payload_offset..];
        if pusi {
            // Skip PES header to reach the elementary stream bytes.
            // PES header: 3-byte start code + 1 stream_id + 2 PES_packet_length
            // + 1 flags1 + 1 flags2 + 1 header_data_length + header_data.
            if payload.len() < 9 {
                continue;
            }
            let header_data_len = payload[8] as usize;
            let es_offset = 9 + header_data_len;
            if es_offset > payload.len() {
                continue;
            }
            au.extend_from_slice(&payload[es_offset..]);
            found_pes = true;
        } else if found_pes {
            au.extend_from_slice(payload);
        }
    }
    // Strip TS padding (0xFF fill at the end of the last packet).
    while au.last() == Some(&0xFF) {
        au.pop();
    }
    au
}

#[test]
fn push_video_misp_to_round_trip_extract() {
    let (mux, video_handle) = unsafe { open_h264_muxer() };

    // Push with a microsecond MISP timestamp: kind=0, status=0xA0, value=1234567890.
    let rc = unsafe {
        tst_muxer_push_video_misp_to(
            mux,
            video_handle,
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            /*pts_90khz=*/ 9000,
            /*key_frame=*/ true,
            /*misp_kind=*/ 0,
            /*time_status=*/ 0xA0,
            /*value=*/ 1234567890,
        )
    };
    assert_eq!(rc, 0, "tst_muxer_push_video_misp_to returned {rc}");

    let ts = drain_mux(mux);
    assert!(!ts.is_empty(), "muxer produced no TS output");

    // Recover the AU bytes and call extract.
    let au = extract_au_from_ts(&ts, 0x100);
    assert!(!au.is_empty(), "no AU bytes recovered from TS");

    let mut out_kind: u8 = 0xFF;
    let mut out_time_status: u8 = 0;
    let mut out_value: u64 = 0;
    let rc = unsafe {
        tst_misp_time_extract(
            au.as_ptr(),
            au.len(),
            TstVideoCodec::H264,
            &mut out_kind,
            &mut out_time_status,
            &mut out_value,
        )
    };
    assert_eq!(
        rc, 0,
        "tst_misp_time_extract returned {rc} (expected 0 = found)"
    );
    assert_eq!(out_kind, 0, "expected microsecond kind (0)");
    assert_eq!(out_time_status, 0xA0, "time_status round-trip failed");
    assert_eq!(out_value, 1234567890, "value round-trip failed");

    unsafe { tst_muxer_close(mux) };
}

#[test]
fn push_video_misp_to_with_dts_emits_pts_dts_flags_11() {
    // Verify the DTS variant also succeeds and the AU contains the MISP SEI.
    let (mux, video_handle) = unsafe { open_h264_muxer() };

    let rc = unsafe {
        tst_muxer_push_video_misp_to_with_dts(
            mux,
            video_handle,
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            /*pts_90khz=*/ 9000,
            /*dts_90khz=*/ 6000,
            /*key_frame=*/ true,
            /*misp_kind=*/ 0,
            /*time_status=*/ 0x00,
            /*value=*/ 999,
        )
    };
    assert_eq!(rc, 0, "tst_muxer_push_video_misp_to_with_dts returned {rc}");

    let ts = drain_mux(mux);
    assert!(!ts.is_empty(), "muxer produced no TS output");

    // Locate the PES and check PTS_DTS_flags = 0b11.
    let pes_start = find_pes_start(&ts, 0x100).expect("no PES start on PID 0x100");
    let pes = &ts[pes_start..];
    assert!(pes.len() >= 19, "PES too short");
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    assert_eq!(
        pts_dts_flags, 0b11,
        "expected PTS_DTS_flags=0b11, got 0b{:02b}",
        pts_dts_flags
    );

    // Also verify MISP SEI round-trips through the _with_dts path.
    let au = extract_au_from_ts(&ts, 0x100);
    let mut ok = 0u8;
    let mut ts_field = 0u8;
    let mut val = 0u64;
    let rc2 = unsafe {
        tst_misp_time_extract(
            au.as_ptr(),
            au.len(),
            TstVideoCodec::H264,
            &mut ok,
            &mut ts_field,
            &mut val,
        )
    };
    assert_eq!(rc2, 0, "extract returned {rc2} on DTS-push output");
    assert_eq!(val, 999, "MISP value round-trip failed for DTS push");

    unsafe { tst_muxer_close(mux) };
}

#[test]
fn push_video_misp_invalid_kind_returns_misp_time_error() {
    let (mux, video_handle) = unsafe { open_h264_muxer() };

    let rc = unsafe {
        tst_muxer_push_video_misp_to(
            mux,
            video_handle,
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            /*pts_90khz=*/ 9000,
            /*key_frame=*/ true,
            /*misp_kind=*/ 2, // out of range
            /*time_status=*/ 0x00,
            /*value=*/ 0,
        )
    };
    assert_eq!(
        rc,
        TstError::MispTime as i32,
        "expected TST_E_MISP_TIME (-45), got {rc}"
    );

    unsafe { tst_muxer_close(mux) };
}

#[test]
fn extract_absent_returns_one_on_plain_au() {
    // A plain IDR NAL (no SEI inserted) must yield 1 (absent).
    let mut out_kind: u8 = 0xFF;
    let mut out_time_status: u8 = 0;
    let mut out_value: u64 = 0;
    let rc = unsafe {
        tst_misp_time_extract(
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            TstVideoCodec::H264,
            &mut out_kind,
            &mut out_time_status,
            &mut out_value,
        )
    };
    assert_eq!(rc, 1, "expected 1 (absent) for plain IDR NAL, got {rc}");
    // Out-params must be untouched on absent.
    assert_eq!(out_kind, 0xFF, "out_kind should be untouched on absent");
}
