#![no_main]

//! Fuzz target — end-to-end demuxer panic-freedom across the config knobs
//! and feed shapes the review found unfuzzed (CORR-01 lived in the
//! `sync_buf_cap` path; `unwrap_timestamps`, chunked `feed`, `feed_aligned`
//! and `reset_sync` were never driven).
//!
//! # Input layout
//!
//! ```text
//! [0]     selector byte
//!           bits [1:0] → StrictMode: 0=Off 1=TimingOnly 2=DescriptorsOnly 3=Full
//!           bit  [2]   → cfi_tolerance
//!           bit  [3]   → unwrap_timestamps
//!           bit  [4]   → sync_buf_cap: 0 = default (4 MiB), 1 = 4 KiB
//!                        (far below the 1 MiB compaction floor)
//!           bits [6:5] → feed shape: 0 = one `feed` of the whole payload
//!                        1 = `feed` in 188-byte chunks
//!                        2 = `feed` in 7-byte chunks (never packet-aligned)
//!                        3 = `feed_aligned` on every full 188-byte chunk
//!           bit  [7]   → call `reset_sync` after the first half of the chunks
//! [1..]   MPEG-TS packet bytes
//! ```
//!
//! Errors from `feed` / `feed_aligned` (`StrictRejection`, `Unrecoverable`,
//! `SyncBufExhausted`, a non-0x47 first byte) are normal outcomes and are
//! discarded. Only panics count as failures.

use libfuzzer_sys::fuzz_target;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};

fuzz_target!(|data: &[u8]| {
    // Need at least the selector byte.
    if data.is_empty() {
        return;
    }

    let selector = data[0];
    let payload = &data[1..];

    let strict = match selector & 0b11 {
        0 => StrictMode::Off,
        1 => StrictMode::TimingOnly,
        2 => StrictMode::DescriptorsOnly,
        _ => StrictMode::Full, // 3
    };
    let cfi_tolerance = (selector >> 2) & 1 == 1;
    let unwrap_timestamps = (selector >> 3) & 1 == 1;
    let small_cap = (selector >> 4) & 1 == 1;
    let shape = (selector >> 5) & 0b11;
    let reset_midway = (selector >> 7) & 1 == 1;

    let mut builder = DemuxerConfig::builder()
        .strict(strict)
        .cfi_tolerance(cfi_tolerance)
        .unwrap_timestamps(unwrap_timestamps);
    if small_cap {
        builder = builder.sync_buf_cap(4 * 1024);
    }
    let mut d = Demuxer::with_config(builder.build());

    if shape == 0 {
        let _ = d.feed(payload);
        while d.next_event().is_some() {}
    } else {
        let chunk = if shape == 2 { 7 } else { 188 };
        let chunks: Vec<&[u8]> = payload.chunks(chunk).collect();
        let half = chunks.len() / 2;
        for (i, c) in chunks.iter().enumerate() {
            if reset_midway && i == half {
                d.reset_sync();
            }
            if shape == 3 {
                // Only full 188-byte chunks qualify; the tail is skipped.
                if let Ok(pkt) = <&[u8; 188]>::try_from(*c) {
                    let _ = d.feed_aligned(pkt);
                }
            } else {
                let _ = d.feed(c);
            }
            while d.next_event().is_some() {}
        }
    }

    // flush() is infallible — it must never panic.
    d.flush();
    while d.next_event().is_some() {}
});
