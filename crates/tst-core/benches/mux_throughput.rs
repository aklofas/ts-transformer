//! Criterion throughput benches for `mpegts::mux::Muxer`.
//!
//! Three benchmarks:
//! - push_video at typical AU sizes (1KB, 50KB, 500KB)
//! - push_klv at typical 1KB blob
//! - end-to-end: push 30 mixed frames + drain via 1316-byte buffer
//!
//! Run: `cargo bench -p tst-core`. Locks current numbers as baseline; later
//! runs report regressions.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

fn synthetic_au(size: usize, key: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + size);
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    let nal_type: u8 = if key { 5 } else { 1 };
    let nri: u8 = if key { 0b11 } else { 0b10 };
    v.push((nri << 5) | nal_type);
    v.resize(4 + size, 0xA5);
    v
}

fn bench_push_video(c: &mut Criterion) {
    let mut group = c.benchmark_group("push_video");
    for size in [1024, 50_000, 500_000] {
        let nal = synthetic_au(size, true);
        group.bench_with_input(BenchmarkId::from_parameter(size), &nal, |b, nal| {
            b.iter(|| {
                let mut cfg = MuxerConfig::default();
                cfg.buffer_packets = 100_000;
                let mut mux = Muxer::new(cfg).unwrap();
                mux.push_video(black_box(nal), Pts90khz::new(0), true)
                    .unwrap();
            });
        });
    }
    group.finish();
}

fn bench_push_klv(c: &mut Criterion) {
    let klv = vec![0xC3u8; 1024];
    c.bench_function("push_klv_1kb", |b| {
        b.iter(|| {
            let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
            // Need video first to anchor PSI emission.
            mux.push_video(&synthetic_au(500, true), Pts90khz::new(0), true)
                .unwrap();
            mux.push_klv(black_box(&klv), Pts90khz::new(0), 0x00)
                .unwrap();
        });
    });
}

fn bench_mux_end_to_end(c: &mut Criterion) {
    c.bench_function("mux_end_to_end_30frames", |b| {
        let frames: Vec<Vec<u8>> = (0..30)
            .map(|i| synthetic_au(2000 + (i % 10) * 200, i == 0))
            .collect();
        let klv = vec![0xC3u8; 64];
        b.iter(|| {
            let mut cfg = MuxerConfig::default();
            cfg.buffer_packets = 200_000;
            let mut mux = Muxer::new(cfg).unwrap();
            for (i, f) in frames.iter().enumerate() {
                mux.push_video(f, Pts90khz::new((i as i64) * 3000), i == 0)
                    .unwrap();
                mux.push_klv(&klv, Pts90khz::new((i as i64) * 3000), 0x00)
                    .unwrap();
            }
            let mut buf = vec![0u8; 1316];
            loop {
                let n = mux.pull(&mut buf);
                if n == 0 {
                    break;
                }
                black_box(&buf[..n]);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_push_video,
    bench_push_klv,
    bench_mux_end_to_end
);
criterion_main!(benches);
