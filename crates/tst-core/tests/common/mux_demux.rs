//! MPEG-TS helpers shared by the `mpegts`, `codec` and `conformance` test
//! binaries: drain a `Muxer` to bytes and a `Demuxer` to events.

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
pub fn collect_events_with(builder: DemuxerConfigBuilder, ts_bytes: &[u8]) -> Vec<DemuxEvent> {
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
