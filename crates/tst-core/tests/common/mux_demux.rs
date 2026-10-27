//! MPEG-TS helpers shared by the `mpegts`, `codec` and `conformance` test
//! binaries: drain a `Muxer` to bytes and a `Demuxer` to events.

use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfigBuilder};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Pull every queued TS packet out of `mux`. The chunk size does not change
/// the result: `Muxer::pull` copies whole 188-byte packets until the queue
/// is empty.
pub fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// The `(elementary_PID, stream_type)` pairs of the PMT section carried
/// whole in the one TS packet `pkt` (PUSI set, section ending in that
/// packet). The walk is bounded by `section_length`, so the CRC and the
/// stuffing after the section are never read as ES entries.
pub fn es_entries(pkt: &[u8]) -> impl Iterator<Item = (u16, u8)> + '_ {
    assert_eq!(pkt[0], 0x47, "TS sync byte");
    assert_ne!(pkt[1] & 0x40, 0, "PMT packet carries a section start");
    let mut off = 4;
    if pkt[3] & 0x20 != 0 {
        off += 1 + pkt[4] as usize;
    }
    off += 1 + pkt[off] as usize; // pointer_field
    let sec = &pkt[off..];
    assert_eq!(sec[0], 0x02, "table_id is a PMT");
    let section_length = ((sec[1] as usize & 0x0F) << 8) | sec[2] as usize;
    let end = 3 + section_length - 4; // the ES loop stops before the CRC_32
    assert!(end <= sec.len(), "PMT section ends inside this packet");
    let program_info_length = ((sec[10] as usize & 0x0F) << 8) | sec[11] as usize;
    let mut i = 12 + program_info_length;
    core::iter::from_fn(move || {
        if i + 5 > end {
            return None;
        }
        let stream_type = sec[i];
        let pid = ((sec[i + 1] as u16 & 0x1F) << 8) | sec[i + 2] as u16;
        let es_info_length = ((sec[i + 3] as usize & 0x0F) << 8) | sec[i + 4] as usize;
        i += 5 + es_info_length;
        Some((pid, stream_type))
    })
}

/// Every event `d` has queued, in order.
pub fn collect_events(d: &mut Demuxer) -> Vec<DemuxEvent> {
    core::iter::from_fn(|| d.next_event()).collect()
}

/// Feed `bytes` to a default `Demuxer` and return its events, without a
/// `flush()` (a PES still open at the end is not emitted).
pub fn demux_bytes(bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut d = Demuxer::new();
    d.feed(bytes).unwrap();
    collect_events(&mut d)
}

/// [`demux_bytes`] followed by `flush()`, so the trailing PES is emitted.
pub fn demux_bytes_flushed(bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut d = Demuxer::new();
    d.feed(bytes).unwrap();
    d.flush();
    collect_events(&mut d)
}

/// [`demux_bytes`] through a demuxer built from `builder`.
pub fn collect_events_with(builder: &DemuxerConfigBuilder, ts_bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut d = Demuxer::with_config(builder.build());
    d.feed(ts_bytes).unwrap();
    collect_events(&mut d)
}

/// One program (number 1, PMT PID 0x1000): H.264 video on PID 0x1011 and
/// synchronous KLV (Metadata AU cells) on `klv_pid`.
pub fn h264_sync_klv_muxer(klv_pid: u16) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_klv(klv_pid, KlvStreamType::SynchronousMetadata, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Muxer::new(b.build().unwrap()).unwrap()
}

/// Clear `PTS_DTS_flags` and zero `PES_header_data_length` in the first PES
/// whose start code carries `stream_id`, so the demuxer sees a PES with no
/// timestamp.
pub fn patch_pes_strip_pts(mut bytes: Vec<u8>, stream_id: u8) -> Vec<u8> {
    let needle = [0x00u8, 0x00, 0x01, stream_id];
    let pos = bytes
        .windows(4)
        .position(|w| w == needle)
        .expect("PES start code should appear in the muxed TS");
    let flags2_off = pos + 7;
    let hdr_len_off = pos + 8;
    bytes[flags2_off] &= 0x3F;
    bytes[hdr_len_off] = 0;
    bytes
}

/// Byte offset of the first byte after the PES header of the n-th PUSI TS
/// packet on `klv_pid`, i.e. where the 5-byte AU cell header starts. `None`
/// if fewer than `n + 1` PUSI packets exist on that PID.
pub fn locate_nth_au_cell_offset(klv_pid: u16, ts_bytes: &[u8], n: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (pkt_idx, pkt) in ts_bytes.chunks_exact(188).enumerate() {
        let pkt_pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pkt_pid != klv_pid {
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
        if idx + 9 > 188 {
            continue;
        }
        let pes_header_data_length = pkt[idx + 8] as usize;
        idx += 9 + pes_header_data_length;
        if seen == n {
            return Some(pkt_idx * 188 + idx);
        }
        seen += 1;
    }
    None
}

/// Rewrite the 5-byte AU cell header + N inner bytes at `offset`. Caller
/// MUST ensure `5 + inner.len()` does not exceed the original cell footprint
/// (we push back into the same byte run).
pub fn patch_au_cell(
    ts_bytes: &mut [u8],
    offset: usize,
    cfi: CellFragmentIndication,
    seq: u8,
    inner: &[u8],
) {
    let hdr = AuCellHeader {
        metadata_service_id: 0x00,
        sequence_number: seq,
        cell_fragment_indication: cfi,
        decoder_config_flag: false,
        random_access_indicator: true,
    };
    let mut buf = Vec::with_capacity(5 + inner.len());
    write_metadata_au_cell(&mut buf, hdr, inner).unwrap();
    ts_bytes[offset..offset + buf.len()].copy_from_slice(&buf);
}
