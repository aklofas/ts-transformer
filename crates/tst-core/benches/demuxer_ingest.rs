//! Demuxer::feed throughput benches.
//!
//! Builds a synthetic TS stream once (~1000 packets = ~188 KB) using the
//! sender muxer, then iterates `Demuxer::feed` over it in two shapes:
//!
//! - `demux_feed_per_188`  — feeds exactly one 188-byte TS packet at a time,
//!   mirroring the `Receiver -> Demuxer` one-packet-per-call path.
//! - `demux_feed_whole`    — feeds the entire stream in one call, mirroring
//!   byte-sink fan-out / file ingest where the caller hands a large buffer.
//!
//! The gap between the two is the measurement target for the `feed_aligned`
//! optimisation: per-packet should approach whole-stream throughput
//! once the demuxer avoids the per-call sync_buf shuffle overhead.
//!
//! Run: `cargo bench -p tst-core --bench demuxer_ingest`
//!      `RUSTFLAGS="-C target-cpu=native" cargo bench -p tst-core --bench demuxer_ingest -- --quick`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

/// Build a synthetic TS stream containing 50 video frames and 50 KLV records.
///
/// Uses `MuxerConfig::default()` (one video + one KLV stream, PrivateData) so
/// this matches the shape most bench consumers will exercise. The first frame
/// is a keyframe (IDR-like NAL byte 0x65), the rest are P-frames. Buffer is
/// sized large enough that `push_*` never returns `BufferFull`.
fn build_synthetic_stream() -> Vec<u8> {
    let mut cfg = MuxerConfig::default();
    // 200 000 packets is large enough to never hit BufferFull while
    // accumulating all 50 frames before a single pull pass.
    cfg.buffer_packets = 200_000;
    let mut mux = Muxer::new(cfg).unwrap();

    let mut out = Vec::with_capacity(1_000 * 188);

    for i in 0u32..50 {
        let key = i == 0;

        // Minimal Annex-B NAL: start code + NAL header byte.
        // IDR slice (0x65) for the keyframe, non-IDR (0x41) for P-frames.
        // Pad to realistic sizes so the bench exercises realistic packet counts.
        let nal_type: u8 = if key { 0x65 } else { 0x41 };
        let payload_size = if key { 50_000 } else { 5_000 };
        let mut nal = Vec::with_capacity(5 + payload_size);
        nal.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, nal_type]);
        nal.resize(5 + payload_size, 0xA5);

        let pts: i64 = (i as i64) * 3_000; // 30 fps in 90 kHz ticks
        mux.push_video(&nal, Pts90khz::new(pts), key).unwrap();

        // 200-byte KLV blob (synthetic ST 0601 order of magnitude).
        let klv = vec![0x42u8; 200];
        mux.push_klv(&klv, Pts90khz::new(pts), 0x00).unwrap();

        // Drain after each frame so the internal queue stays bounded.
        let mut buf = [0u8; 1_316];
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    assert_eq!(
        out.len() % 188,
        0,
        "synthetic stream length must be a multiple of 188 bytes"
    );
    out
}

fn bench_feed_per_packet(c: &mut Criterion) {
    // Build the stream outside the timed loop — we are benchmarking the
    // demuxer, not the muxer.
    let stream = build_synthetic_stream();
    let n_packets = stream.len() / 188;

    c.bench_function("demux_feed_per_188", |b| {
        b.iter(|| {
            let mut d = Demuxer::new();
            for i in 0..n_packets {
                let start = i * 188;
                // Use feed_aligned — the fast path for callers that
                // already hold a single aligned 188-byte TS packet, which is
                // exactly the shape produced by pipeline::Receiver.
                // The stream is well-formed so pkt[0] == 0x47; unwrap is safe.
                let pkt: &[u8; 188] = stream[start..start + 188].try_into().unwrap();
                d.feed_aligned(black_box(pkt)).unwrap();
                // Drain events so the demuxer's internal queue does not grow
                // unboundedly across packets; mimics a real consumer.
                while let Some(e) = d.next_event() {
                    black_box(e);
                }
            }
        })
    });
}

fn bench_feed_whole_stream(c: &mut Criterion) {
    let stream = build_synthetic_stream();

    c.bench_function("demux_feed_whole", |b| {
        b.iter(|| {
            let mut d = Demuxer::new();
            // One call with the entire stream — best-case for the demuxer's
            // internal sync-buffer path because it can find sync once and then
            // iterate without per-call overhead.
            d.feed(black_box(&stream)).unwrap();
            while let Some(e) = d.next_event() {
                black_box(e);
            }
        })
    });
}

criterion_group!(benches, bench_feed_per_packet, bench_feed_whole_stream);
criterion_main!(benches);
