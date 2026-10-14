//! Verifies KLV PES stream_id selection per stream_type.
//!
//! Per H.222.0 V9 Table 2-22:
//! - Async KLV (stream_type 0x06 PrivateData)  → PES stream_id 0xBD (private_stream_1)
//! - Sync  KLV (stream_type 0x15 SynchronousMetadata) → PES stream_id 0xFC (metadata)

use crate::common::mux_demux::drain_mux as drain;
use crate::common::synthetic::minimal_klv as synthetic_klv;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Extract the PES `stream_id` byte from the first PUSI packet on `pid`.
///
/// Returns `None` if no such packet is found in the buffer. Skips the
/// adaptation field (if present) to locate the PES start, then reads byte 3
/// of the PES packet (start_code[3] + stream_id).
fn pes_stream_id_on_pid(ts_buf: &[u8], pid: u16) -> Option<u8> {
    for pkt in ts_buf.chunks_exact(188) {
        let pkt_pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pkt_pid != pid {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        if !pusi {
            continue;
        }
        let af_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if af_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        // PES layout: 00 00 01 <stream_id> ...
        if idx + 4 <= 188 {
            debug_assert_eq!(pkt[idx], 0x00);
            debug_assert_eq!(pkt[idx + 1], 0x00);
            debug_assert_eq!(pkt[idx + 2], 0x01);
            return Some(pkt[idx + 3]);
        }
    }
    None
}

#[test]
fn async_klv_pes_uses_stream_id_0xbd() {
    // stream_type 0x06 PrivateData — async KLV must use PES stream_id 0xBD
    // (private_stream_1) per ffmpeg + GStreamer convention.
    // H.222.0 V9 Table 2-22 reserves 0xFC for metadata streams
    // (stream_type 0x15) only.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(
            0x1031,
            KlvStreamType::PrivateData,
            /*carries_pts=*/ false,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    mux.push_klv(&synthetic_klv(), Pts90khz::new(0), 0x00)
        .unwrap();
    let ts_buf = drain(&mut mux);

    let stream_id =
        pes_stream_id_on_pid(&ts_buf, 0x1031).expect("must find a PUSI packet on KLV PID 0x1031");
    assert_eq!(
        stream_id, 0xBD,
        "async KLV (stream_type 0x06) must use PES stream_id 0xBD (private_stream_1)"
    );
}

#[test]
fn sync_klv_pes_keeps_stream_id_0xfc() {
    // stream_type 0x15 SynchronousMetadata — sync KLV must use PES stream_id
    // 0xFC per H.222.0 V9 Table 2-22 (reserved for metadata streams).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(
            0x1031,
            KlvStreamType::SynchronousMetadata,
            /*carries_pts=*/ true,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    mux.push_klv(&synthetic_klv(), Pts90khz::new(90_000), 0x00)
        .unwrap();
    let ts_buf = drain(&mut mux);

    let stream_id =
        pes_stream_id_on_pid(&ts_buf, 0x1031).expect("must find a PUSI packet on KLV PID 0x1031");
    assert_eq!(
        stream_id, 0xFC,
        "sync KLV (stream_type 0x15) must use PES stream_id 0xFC (metadata)"
    );
}
