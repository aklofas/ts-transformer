//! Multi-program integration test for tst-c.
//!
//! Builds a 2-program mux config via the C ABI entry points
//! (`tst_mux_config_add_program`, `_add_video_stream`, `_add_klv_stream`),
//! opens a muxer, pushes to both programs, pulls the output, and verifies
//! the PAT carries exactly 2 program entries.

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
use tstrans::muxer::{
    tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_klv_to, tst_muxer_push_video_to,
};

// Annex-B IDR NAL: 4-byte start code + nal_unit_type 0x65 (IDR slice).
// This is a non-decodable stub — it exists only so the muxer sees a valid
// Annex-B envelope and produces PES+PCR packets on the correct PID.
const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x82, 0x00, 0x00];

// Minimal 17-byte ST 0601 KLV blob: 16-byte UL + 1-byte BER length (0).
const KLV_BLOB: &[u8] = &[
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00,
];

#[test]
fn two_program_muxer_pat_carries_both_programs() {
    unsafe {
        // ── Build config: 2 programs ──────────────────────────────────────
        let cfg = tst_mux_config_new();
        assert!(!cfg.is_null());

        // Program 1: PMT on 0x1000, video on 0x1011, KLV on 0x1031.
        let p1 = tst_mux_config_add_program(cfg, 1, 0x1000);
        assert_ne!(p1, TstProgramHandle(u32::MAX), "p1 handle must be valid");
        let v1 = tst_mux_config_add_video_stream(cfg, p1, 0x1011, TstVideoCodec::H264 as i32);
        assert_ne!(v1, TST_INVALID_STREAM_HANDLE);
        let k1 = tst_mux_config_add_klv_stream(
            cfg,
            p1,
            0x1031,
            TstKlvStreamType::PrivateData as i32,
            false,
        );
        assert_ne!(k1, TST_INVALID_STREAM_HANDLE);

        // Program 2: PMT on 0x1100, video on 0x1111, KLV on 0x1131.
        // PIDs must be unique across programs — use a distinct range.
        let p2 = tst_mux_config_add_program(cfg, 2, 0x1100);
        assert_ne!(p2, TstProgramHandle(u32::MAX), "p2 handle must be valid");
        assert_ne!(p1, p2, "program handles must be distinct");
        let v2 = tst_mux_config_add_video_stream(cfg, p2, 0x1111, TstVideoCodec::H264 as i32);
        assert_ne!(v2, TST_INVALID_STREAM_HANDLE);
        assert_ne!(v1, v2, "cross-program video handles must be distinct");
        let k2 = tst_mux_config_add_klv_stream(
            cfg,
            p2,
            0x1131,
            TstKlvStreamType::PrivateData as i32,
            false,
        );
        assert_ne!(k2, TST_INVALID_STREAM_HANDLE);

        // ── Open muxer ────────────────────────────────────────────────────
        let mux = tst_muxer_open(cfg);
        // Free config immediately after open — muxer owns its own copy.
        tst_mux_config_free(cfg);
        assert!(!mux.is_null(), "tst_muxer_open returned null");

        // ── Push 30 ticks to both programs ────────────────────────────────
        // 30 ticks at 3003 PTS units apart (~30fps at 90kHz) covers the
        // default PSI interval (100ms) so PAT/PMT are guaranteed to emit.
        for tick in 0u64..30 {
            let pts = 90_000i64 + (tick * 3_003) as i64;
            let rc1 = tst_muxer_push_video_to(mux, v1, NAL_IDR.as_ptr(), NAL_IDR.len(), pts, true);
            assert_eq!(
                rc1, 0,
                "push video to program 1 failed at tick {tick}: rc={rc1}"
            );
            let rc2 = tst_muxer_push_video_to(mux, v2, NAL_IDR.as_ptr(), NAL_IDR.len(), pts, true);
            assert_eq!(
                rc2, 0,
                "push video to program 2 failed at tick {tick}: rc={rc2}"
            );
            let rk1 = tst_muxer_push_klv_to(mux, k1, KLV_BLOB.as_ptr(), KLV_BLOB.len(), pts);
            assert_eq!(
                rk1, 0,
                "push KLV to program 1 failed at tick {tick}: rc={rk1}"
            );
            let rk2 = tst_muxer_push_klv_to(mux, k2, KLV_BLOB.as_ptr(), KLV_BLOB.len(), pts);
            assert_eq!(
                rk2, 0,
                "push KLV to program 2 failed at tick {tick}: rc={rk2}"
            );
        }

        // ── Pull all available output ─────────────────────────────────────
        let mut buf = vec![0u8; 1024 * 1024];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n > 0, "muxer produced no output bytes");
        assert_eq!(n % 188, 0, "pull returned non-aligned byte count: {n}");
        buf.truncate(n);

        // ── Verify PAT (PID 0x0000) contains both program entries ─────────
        //
        // PAT layout (starting at byte 0 of the 188-byte TS packet):
        //   byte 0:      sync byte 0x47
        //   bytes 1-2:   PUSI | PID high | PID low  (PID 0x0000 for PAT)
        //   byte 3:      continuity counter | adaptation field flags
        //   byte 4:      pointer_field (usually 0)
        //   byte 5:      table_id (0x00 for PAT)
        //   bytes 6-7:   section_syntax_indicator | section_length
        //   bytes 8-9:   transport_stream_id
        //   byte 10:     version + current_next_indicator
        //   byte 11:     section_number
        //   byte 12:     last_section_number
        //   bytes 13+:   program loop (4 bytes each):
        //                  program_number[15:0] | reserved[2:0]+PMT_PID[12:0]
        //
        // We find the PAT, then decode the program loop to verify both
        // program_number=1/PMT_PID=0x1000 and program_number=2/PMT_PID=0x1100.

        let pat_packet = buf
            .chunks_exact(188)
            .find(|pkt| {
                let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
                pid == 0x0000
            })
            .expect("PAT (PID 0x0000) must be present in output");

        // Offset 4: pointer_field. Skip it to reach the PAT section.
        let pointer = pat_packet[4] as usize;
        let section_base = 5 + pointer;

        // table_id must be 0x00 (PAT).
        assert_eq!(
            pat_packet[section_base], 0x00,
            "expected PAT table_id=0x00, got 0x{:02x}",
            pat_packet[section_base]
        );

        // section_length is in bytes 1-2 of the section (after table_id).
        // Bits [11:0] of bytes [section_base+1..section_base+2].
        let section_len = (((pat_packet[section_base + 1] as u16) & 0x0F) << 8)
            | pat_packet[section_base + 2] as u16;

        // Program loop starts after: table_id(1) + section_length(2) +
        // transport_stream_id(2) + version+current(1) + section_number(1) +
        // last_section_number(1) = 8 bytes into the section.
        let prog_loop_start = section_base + 8;

        // CRC32 is the last 4 bytes of the section; program loop ends before it.
        // Each loop entry is 4 bytes. Extract entries.
        let loop_end = section_base + 3 + section_len as usize - 4; // -4 for CRC
        let loop_bytes = &pat_packet[prog_loop_start..loop_end];
        assert!(
            loop_bytes.len() >= 8,
            "PAT program loop too short ({} bytes) — expected at least 2 entries",
            loop_bytes.len()
        );

        let prog1_num = u16::from_be_bytes([loop_bytes[0], loop_bytes[1]]);
        let prog1_pid = u16::from_be_bytes([loop_bytes[2] & 0x1F, loop_bytes[3]]);
        let prog2_num = u16::from_be_bytes([loop_bytes[4], loop_bytes[5]]);
        let prog2_pid = u16::from_be_bytes([loop_bytes[6] & 0x1F, loop_bytes[7]]);

        assert_eq!(prog1_num, 1, "first PAT entry program_number should be 1");
        assert_eq!(
            prog1_pid, 0x1000,
            "first PAT entry PMT PID should be 0x1000, got 0x{prog1_pid:04x}"
        );
        assert_eq!(prog2_num, 2, "second PAT entry program_number should be 2");
        assert_eq!(
            prog2_pid, 0x1100,
            "second PAT entry PMT PID should be 0x1100, got 0x{prog2_pid:04x}"
        );

        tst_muxer_close(mux);
    }
}
