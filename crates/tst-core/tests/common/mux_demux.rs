//! MPEG-TS helpers shared by the `mpegts`, `codec` and `conformance` test
//! binaries: drain a `Muxer` to bytes and a `Demuxer` to events.

use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfigBuilder};
use tst_core::mpegts::mux::Muxer;

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
