//! Test tool: walk PCR samples in an MPEG-TS file; compute inter-PCR
//! delta median + 95th percentile (in milliseconds). Used by
//! release-validation.sh Step 9 (PCR jitter test).
//!
//! Fail-exit conditions (from the measured corpus PCR cadence):
//!   median > 67 ms
//!   p95    > 100 ms
//!
//! Usage:
//!     cargo run -p tst-core --bin measure-pcr-jitter -- <input.ts>
//!
//! PCR extraction is inlined (parse_ts_packet is pub(super); the
//! relevant bits are documented at ISO/IEC 13818-1 §2.4.3.4 / §2.4.3.5).

use std::env;
use std::fs::File;
use std::io::Read;

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const PCR_HZ: f64 = 27_000_000.0; // 27 MHz PCR clock.
const MEDIAN_THRESHOLD_MS: f64 = 67.0;
const P95_THRESHOLD_MS: f64 = 100.0;

/// Extract the 27 MHz PCR value from a 188-byte TS packet, if present.
/// Returns None when:
///   - packet is malformed
///   - adaptation field is absent
///   - PCR flag is clear
fn extract_pcr(buf: &[u8]) -> Option<u64> {
    if buf.len() != TS_PACKET_SIZE || buf[0] != TS_SYNC_BYTE {
        return None;
    }
    // Adaptation_field_control: bits 5..4 of byte 3.
    //   0b10 = adaptation only
    //   0b11 = adaptation + payload
    let adaptation_control = (buf[3] >> 4) & 0x03;
    if adaptation_control & 0b10 == 0 {
        return None;
    }
    let af_len = buf[4] as usize;
    if af_len < 7 || 5 + af_len > TS_PACKET_SIZE {
        return None;
    }
    let flags = buf[5];
    let pcr_flag = (flags & 0x10) != 0;
    if !pcr_flag {
        return None;
    }
    let b = &buf[6..12];
    let base = (((b[0] as u64) << 25)
        | ((b[1] as u64) << 17)
        | ((b[2] as u64) << 9)
        | ((b[3] as u64) << 1)
        | (((b[4] as u64) >> 7) & 0x01))
        & ((1u64 << 33) - 1);
    let ext = (((b[4] as u64) & 0x01) << 8) | (b[5] as u64);
    Some(base * 300 + ext) // 27 MHz units.
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <input.ts>", args[0]);
        std::process::exit(2);
    }
    let input = &args[1];

    let mut ts = Vec::new();
    File::open(input)?.read_to_end(&mut ts)?;

    let mut pcrs: Vec<u64> = Vec::new();
    for chunk in ts.chunks_exact(TS_PACKET_SIZE) {
        if let Some(pcr) = extract_pcr(chunk) {
            pcrs.push(pcr);
        }
    }

    if pcrs.len() < 2 {
        eprintln!(
            "FAIL: fewer than 2 PCR samples in {} (found {})",
            input,
            pcrs.len()
        );
        std::process::exit(1);
    }

    // Convert 27 MHz ticks to milliseconds: ms = ticks / 27_000.
    // Use the wrap-aware diff (PCR wraps at (1<<33)*300 ≈ every 26.5 h) so a
    // wrap-straddling delta yields the true ~small interval, not a 2^64
    // garbage value that a plain `wrapping_sub` would produce.
    let mut deltas_ms: Vec<f64> = pcrs
        .windows(2)
        .map(|w| tst_core::mpegts::common::pcr_diff_27mhz(w[1], w[0]) as f64 / (PCR_HZ / 1000.0))
        .collect();
    deltas_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let median = deltas_ms[deltas_ms.len() / 2];
    let p95_idx = (deltas_ms.len() * 95) / 100;
    let p95 = deltas_ms[p95_idx.min(deltas_ms.len() - 1)];
    let min_delta = deltas_ms[0]; // sorted ascending

    println!("PCR samples: {}", pcrs.len());
    println!("Inter-PCR delta median: {:.2} ms", median);
    println!("Inter-PCR delta p95:    {:.2} ms", p95);

    let mut failed = false;
    // `pcr_diff_27mhz` folds a wrap-straddle into a small POSITIVE delta, so a
    // NEGATIVE delta is a genuine backward PCR jump (PCR must be monotonic
    // aside from wrap). The upper-threshold median/p95 checks below would let
    // such a jump pass, so flag it explicitly.
    if min_delta < 0.0 {
        eprintln!(
            "FAIL: backward PCR jump — negative inter-PCR delta {:.2} ms (PCR must be monotonic aside from wrap)",
            min_delta
        );
        failed = true;
    }
    if median > MEDIAN_THRESHOLD_MS {
        eprintln!(
            "FAIL: median {:.2} ms > threshold {} ms",
            median, MEDIAN_THRESHOLD_MS
        );
        failed = true;
    }
    if p95 > P95_THRESHOLD_MS {
        eprintln!(
            "FAIL: p95 {:.2} ms > threshold {} ms",
            p95, P95_THRESHOLD_MS
        );
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
