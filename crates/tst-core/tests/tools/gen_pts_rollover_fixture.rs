//! Test tool: emit a synthetic MPEG-TS file with PTS values straddling
//! the 33-bit boundary. Used by the maintainer-local PTS-rollover
//! stress check.
//!
//! The initial PTS is set 5 seconds below 2^33 (in 90 kHz ticks); the
//! stream runs for `duration_secs` seconds; midway through, the PTS
//! wraps from 2^33-1 back to 0. Exercises the muxer's PTS-clamping
//! logic + the demuxer's wrap-handling.
//!
//! Usage:
//!     cargo run -p tst-core --bin gen-pts-rollover-fixture -- <output.ts> [duration_secs]

use std::env;
use std::fs::File;
use std::io::Write;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

const PTS_TICKS_PER_SEC: i64 = 90_000;
const PTS_MODULO: i64 = 1i64 << 33;
const FRAME_RATE: i64 = 30;
const PTS_TICKS_PER_FRAME: i64 = PTS_TICKS_PER_SEC / FRAME_RATE; // 3000 @ 30 fps

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: {} <output.ts> [duration_secs (default 10)]",
            args[0]
        );
        std::process::exit(2);
    }
    let output_path = &args[1];
    let duration_secs: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    // Initial PTS: 5 seconds BEFORE the rollover boundary, so a 10-second
    // stream wraps ~5 seconds in.
    let initial_pts: i64 = PTS_MODULO - 5 * PTS_TICKS_PER_SEC;
    let mut pts: i64 = initial_pts;

    let mut mux = Muxer::new(MuxerConfig::default()).expect("valid config");
    let mut out = File::create(output_path)?;
    let mut buf = [0u8; 1316];

    let total_frames = duration_secs * FRAME_RATE;
    for i in 0..total_frames {
        // One IDR every 2 seconds.
        let key = i % (FRAME_RATE * 2) == 0;
        // Same Annex-B NAL shape as mux_to_file.rs.
        let nal_type: u8 = if key { 5 } else { 1 };
        let nri: u8 = if key { 0b11 } else { 0b10 };
        let mut au: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01, (nri << 5) | nal_type];
        au.resize(800 + (i as usize % 200), 0xA5);
        mux.push_video(&au, Pts90khz::new(pts), key)
            .expect("push_video");

        let klv: Vec<u8> = (0..50).map(|j| (i as u8).wrapping_add(j as u8)).collect();
        mux.push_klv(&klv, Pts90khz::new(pts), 0x00)
            .expect("push_klv");

        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }

        // Walk the 33-bit PTS clock with explicit wraparound.
        pts = (pts + PTS_TICKS_PER_FRAME) % PTS_MODULO;
    }

    eprintln!(
        "wrote {} frames ({} s) to {} (initial PTS {}, final PTS {} — \
         straddles the 33-bit boundary at {})",
        total_frames, duration_secs, output_path, initial_pts, pts, PTS_MODULO,
    );
    Ok(())
}
