//! Syncer bench — exercises the TS-alignment state machine inside `Receiver`
//! through its public API. Three scenarios cover the hot path (already aligned),
//! the hunt-then-lock cycle (misaligned prefix), and periodic interior loss
//! (rare resync after a dropped byte).
//!
//! Run with:
//!   SRT_FORCE_VENDORED=1 RUSTFLAGS="-C target-cpu=native" \
//!     cargo bench -p tst-pipeline --bench syncer

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tst_pipeline::{Receiver, ReceiverConfig, RecvTransport, TransportError};

// ---------------------------------------------------------------------------
// Mock transport — feeds a pre-built byte buffer in fixed-size chunks.
// ---------------------------------------------------------------------------

struct BufTransport {
    data: Vec<u8>,
    pos: usize,
    /// How many bytes to return per `recv_bytes` call (mirrors one SRT datagram).
    chunk: usize,
}

impl BufTransport {
    fn new(data: Vec<u8>, chunk: usize) -> Self {
        Self {
            data,
            pos: 0,
            chunk,
        }
    }
}

impl RecvTransport for BufTransport {
    fn max_payload(&self) -> usize {
        self.chunk
    }

    fn recv_bytes(&mut self, dst: &mut [u8]) -> Result<usize, TransportError> {
        if self.pos >= self.data.len() {
            return Err(TransportError::Closed);
        }
        // Deliver up to `chunk` bytes, but never more than dst can hold.
        let end = (self.pos + self.chunk)
            .min(self.data.len())
            .min(self.pos + dst.len());
        let n = end - self.pos;
        dst[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos += n;
        Ok(n)
    }

    fn is_alive(&self) -> bool {
        self.pos < self.data.len()
    }

    fn close(&mut self) {
        // Force EOF so any in-progress recv stops immediately.
        self.pos = self.data.len();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build one 188-byte TS packet with a valid sync byte, PID, and CC.
/// Payload bytes are deterministic so the buffer isn't compressible.
fn synthetic_packet(pid: u16, cc: u8, seq: usize) -> [u8; 188] {
    let mut p = [0u8; 188];
    p[0] = 0x47; // sync byte — mandatory for Syncer to lock
    p[1] = ((pid >> 8) as u8) & 0x1F;
    p[2] = (pid & 0xFF) as u8;
    // Payload-only adaptation_field_control (0x10) | cc
    p[3] = 0x10 | (cc & 0x0F);
    // Fill payload with a XOR pattern so adjacent packets differ
    for (j, byte) in p[4..].iter_mut().enumerate() {
        *byte = (seq.wrapping_add(j + 4) as u8) ^ 0x5A;
    }
    p
}

/// N back-to-back 188-byte packets — the aligned, already-synced hot path.
fn aligned_buffer(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n * 188);
    for i in 0..n {
        v.extend_from_slice(&synthetic_packet(0x0100, (i & 0x0F) as u8, i));
    }
    v
}

/// Drive a fresh `Receiver` until it has delivered `want` packets or the
/// transport closes. Returns how many packets were delivered.
fn drain(data: Vec<u8>, chunk: usize, want: usize) -> usize {
    let t = BufTransport::new(data, chunk);
    let mut r = Receiver::new(t, ReceiverConfig::default());
    let mut count = 0usize;
    while count < want {
        match r.next_packet() {
            Ok(_) => count += 1,
            Err(_) => break,
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Bench cases
// ---------------------------------------------------------------------------

/// Hot path: 1 000 packets delivered in 1 316-byte chunks (7 packets each,
/// matching a typical SRT datagram size). The Syncer locks immediately on the
/// first byte and never needs to re-hunt.
fn bench_aligned_steady(c: &mut Criterion) {
    // Pre-build the buffer outside the timed loop — we're measuring the
    // receive/sync path, not Vec allocation.
    let buf = aligned_buffer(1000);
    c.bench_function("syncer_aligned_steady_1000", |b| {
        b.iter(|| {
            // Clone inside the loop so each iteration gets a fresh transport
            // with pos=0; the clone cost is dominated by the actual bench work.
            let n = drain(black_box(buf.clone()), 1316, 1000);
            black_box(n);
        })
    });
}

/// Hunt-then-lock: 100 random bytes prepended before the aligned stream.
/// The Syncer must scan forward until it finds the 0x47 sync byte, then
/// verify the next packet boundary (two-sync confirmation) before locking.
/// This exercises the full acquire cycle, which typically fires once per
/// connection setup in production.
fn bench_hunt_then_lock(c: &mut Criterion) {
    // 100 bytes that do NOT start with 0x47 so the Syncer is forced to hunt.
    let mut buf: Vec<u8> = (0u8..100).map(|b| b.wrapping_add(1)).collect(); // 1..=100, none is 0x47
    buf.extend_from_slice(&aligned_buffer(1000));
    c.bench_function("syncer_hunt_then_lock_1000", |b| {
        b.iter(|| {
            let n = drain(black_box(buf.clone()), 1316, 1000);
            black_box(n);
        })
    });
}

/// Interior misalignment: every 32 packets one byte is dropped from the
/// stream, forcing a resync cycle. This is the rare-but-real case of a
/// partially-corrupt or truncated datagram arriving mid-stream. The Syncer
/// loses lock, re-hunts, and re-locks inside the same call sequence.
fn bench_misaligned_interior(c: &mut Criterion) {
    let mut buf = aligned_buffer(1000);
    // Drop one byte at each 32-packet boundary. Work backwards so earlier
    // removals don't shift the indices of later targets.
    let mut drop_idx = 32 * 188;
    while drop_idx < buf.len() {
        buf.remove(drop_idx);
        // After removal the next boundary is 32 more packets ahead,
        // but every packet after drop_idx is now shifted by 1, so the
        // raw offset advances by exactly 32 * 188 again.
        drop_idx += 32 * 188;
    }
    // We expect fewer than 1000 packets because some are lost at each resync.
    // Use 800 as a conservative lower bound; the bench ends whenever the
    // transport closes.
    c.bench_function("syncer_misaligned_interior", |b| {
        b.iter(|| {
            let _ = drain(black_box(buf.clone()), 1316, 800);
        })
    });
}

criterion_group!(
    benches,
    bench_aligned_steady,
    bench_hunt_then_lock,
    bench_misaligned_interior
);
criterion_main!(benches);
