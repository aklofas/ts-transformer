//! DTS-aware video push entry points for the C ABI.
//!
//! Builds a single-stream H.264 muxer via the C ABI, pushes one IDR access
//! unit with distinct PTS (9000) and DTS (6000) via the
//! `tst_muxer_push_video_to_with_dts` function, and verifies that the
//! emitted PES carries `PTS_DTS_flags = '11'` (bits 7–6 of PES optional
//! header byte 3 both set) with the exact PTS/DTS values surviving
//! the round-trip.

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::muxer::{
    TstMuxer, tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_video_to_with_dts,
    tst_muxer_push_video_wire_to_with_dts,
};

/// Minimal Annex-B IDR NAL unit accepted by the muxer's Annex-B validator.
/// start code (4 bytes) + NAL type 0x65 (IDR slice) + filler payload.
const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];

/// Drain all buffered TS bytes from the C muxer into a Vec.
///
/// The backing `Vec` keeps its allocation alive for the caller — no pointer
/// into it may outlive the returned Vec (per
/// `feedback_tst_c_arena_pointer_alias_flake`).
fn drain_mux(mux: *mut TstMuxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 188];
    loop {
        let n = unsafe { tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len()) };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Locate the first PES start in `ts_bytes` on the given PID and return
/// the index into `ts_bytes` of the first byte of the PES header.
///
/// Returns `None` if no TS packet on `pid` has `payload_unit_start_indicator`
/// set.
fn find_pes_start(ts_bytes: &[u8], pid: u16) -> Option<usize> {
    assert_eq!(
        ts_bytes.len() % 188,
        0,
        "TS must be aligned to 188-byte packets"
    );
    for pkt in ts_bytes.chunks(188) {
        // TS header: sync(1) + error/PUSI/priority+PID_hi(1) + PID_lo(1) + AFC+CC(1)
        if pkt[0] != 0x47 {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        let pkt_pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        if pkt_pid != pid || !pusi {
            continue;
        }
        // Locate payload start within the packet.
        let afc = (pkt[3] >> 4) & 0x03;
        let payload_offset = match afc {
            // 0b01 = payload only
            1 => 4,
            // 0b11 = adaptation field + payload
            3 => 4 + 1 + pkt[4] as usize,
            _ => continue,
        };
        if payload_offset >= 188 {
            continue;
        }
        // The TS packet payload starts with the pointer_field when PUSI is set
        // for sections, but for PES streams the payload IS the PES header
        // directly (no pointer field). Return the index of the PES start code.
        let pkt_start = unsafe {
            // SAFETY: ts_bytes is a contiguous slice; pkt is a sub-slice of it.
            pkt.as_ptr().offset_from(ts_bytes.as_ptr()) as usize
        };
        return Some(pkt_start + payload_offset);
    }
    None
}

/// Parse the 90 kHz PTS value from a PES PTS field starting at `bytes[0]`.
///
/// ISO/IEC 13818-1 §2.4.3.6: the 5-byte PTS field encodes:
/// `'00X1'(4) | PTS[32:30](3) | marker(1) | PTS[29:15](15) | marker(1) |
///  PTS[14:0](15) | marker(1)`.
fn parse_pts_field(bytes: &[u8]) -> i64 {
    assert!(bytes.len() >= 5);
    let p32_30 = ((bytes[0] & 0x0E) as i64) << 29;
    let p29_15 = ((bytes[1] as i64) << 22) | (((bytes[2] & 0xFE) as i64) << 14);
    let p14_0 = ((bytes[3] as i64) << 7) | ((bytes[4] >> 1) as i64);
    p32_30 | p29_15 | p14_0
}

#[test]
fn push_video_to_with_dts_emits_pts_dts_flags_11() {
    // Build a single H.264 video stream on program 1, PID 0x100.
    let (mux, video_handle) = unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let handle = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264);
        let mux = tst_muxer_open(cfg);
        // Free the config immediately — muxer holds its own copy of the config.
        tst_mux_config_free(cfg);
        assert!(!mux.is_null(), "tst_muxer_open returned null");
        (mux, handle)
    };

    // NAL_IDR backing store is 'static so no lifetime issue.
    let rc = unsafe {
        tst_muxer_push_video_to_with_dts(
            mux,
            video_handle,
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            /*pts_90khz=*/ 9000,
            /*dts_90khz=*/ 6000,
            /*key_frame=*/ true,
        )
    };
    assert_eq!(rc, 0, "tst_muxer_push_video_to_with_dts returned {rc}");

    let ts = drain_mux(mux);
    assert!(!ts.is_empty(), "muxer produced no TS output");
    assert_eq!(
        ts.len() % 188,
        0,
        "TS output not aligned to 188-byte packets"
    );

    // Locate the PES start on PID 0x100.
    let pes_start =
        find_pes_start(&ts, 0x100).expect("no PES start found on PID 0x100 in muxer output");

    let pes = &ts[pes_start..];
    // PES packet layout:
    //   [0..3]  = 00 00 01 (start code) + stream_id
    //   [4..5]  = PES_packet_length (big-endian)
    //   [6]     = 10 | flags byte 1 (markers)
    //   [7]     = flags byte 2 — bits 7..6 are PTS_DTS_flags
    //   [8]     = PES_header_data_length
    //   [9..13] = PTS field (5 bytes) when PTS_DTS_flags != '00'
    //   [14..18]= DTS field (5 bytes) when PTS_DTS_flags == '11'
    assert!(
        pes.len() >= 19,
        "PES too short to contain PTS+DTS: {} bytes",
        pes.len()
    );
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "missing PES start code");

    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    assert_eq!(
        pts_dts_flags, 0b11,
        "expected PTS_DTS_flags=0b11 (both PTS and DTS present), got 0b{:02b}",
        pts_dts_flags
    );

    // Verify the PES_header_data_length accounts for 10 bytes (5 PTS + 5 DTS).
    let header_data_len = pes[8] as usize;
    assert!(
        header_data_len >= 10,
        "PES_header_data_length {header_data_len} too small for PTS+DTS (need 10)"
    );

    let pts = parse_pts_field(&pes[9..14]);
    let dts = parse_pts_field(&pes[14..19]);

    assert_eq!(pts, 9000, "PTS round-trip failed: expected 9000, got {pts}");
    assert_eq!(dts, 6000, "DTS round-trip failed: expected 6000, got {dts}");

    unsafe { tst_muxer_close(mux) };
}

#[test]
fn push_video_wire_to_with_dts_emits_pts_dts_flags_11() {
    // Verify the wire-form variant also emits PTS_DTS_flags='11'.
    // Wire bytes are passed verbatim — no Annex-B framing applied, so we
    // can reuse the same IDR bytes (they are already Annex-B, but the wire
    // path skips the validator).
    let (mux, video_handle) = unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let handle = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null(), "tst_muxer_open returned null");
        (mux, handle)
    };

    let rc = unsafe {
        tst_muxer_push_video_wire_to_with_dts(
            mux,
            video_handle,
            NAL_IDR.as_ptr(),
            NAL_IDR.len(),
            /*pts_90khz=*/ 9000,
            /*dts_90khz=*/ 6000,
            /*key_frame=*/ true,
        )
    };
    assert_eq!(rc, 0, "tst_muxer_push_video_wire_to_with_dts returned {rc}");

    let ts = drain_mux(mux);
    assert!(!ts.is_empty(), "muxer produced no TS output");

    let pes_start =
        find_pes_start(&ts, 0x100).expect("no PES start found on PID 0x100 in muxer output");
    let pes = &ts[pes_start..];

    assert!(
        pes.len() >= 19,
        "PES too short to contain PTS+DTS: {} bytes",
        pes.len()
    );
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "missing PES start code");

    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    assert_eq!(
        pts_dts_flags, 0b11,
        "expected PTS_DTS_flags=0b11 (both PTS and DTS present), got 0b{:02b}",
        pts_dts_flags
    );

    // Verify the PES_header_data_length accounts for 10 bytes (5 PTS + 5 DTS).
    let header_data_len = pes[8] as usize;
    assert!(
        header_data_len >= 10,
        "PES_header_data_length {header_data_len} too small for PTS+DTS (need 10)"
    );

    let pts = parse_pts_field(&pes[9..14]);
    let dts = parse_pts_field(&pes[14..19]);

    assert_eq!(pts, 9000, "PTS round-trip failed: expected 9000, got {pts}");
    assert_eq!(dts, 6000, "DTS round-trip failed: expected 6000, got {dts}");

    unsafe { tst_muxer_close(mux) };
}
