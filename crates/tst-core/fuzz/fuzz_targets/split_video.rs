#![no_main]

//! Fuzz target — `split_video` / `split_video_strict` panic-freedom.
//!
//! Exercises the NAL-unit walker (`split_nals`), OBU walker (`split_obus`),
//! and the AV1 binding-unwrap path (`unwrap_av1_binding`) on arbitrary bytes
//! across all four `VideoCodec` variants and both `Av1CarriageMode` variants.
//!
//! `split_video` returns `(VideoPayload, Vec<NonConformantIssue>)` and must
//! never panic. `split_video_strict` returns `Result` and must also never
//! panic — a returned `Err` is a normal outcome, not a failure.
//!
//! # Input layout
//!
//! ```text
//! [0]     selector byte
//!           bits [1:0] → VideoCodec: 0=H264 1=H265 2=H266 3=Av1
//!           bit  [2]   → Av1CarriageMode: 0=Mpeg2TsBinding 1=InteropRawObu
//! [1..]   raw elementary-stream bytes
//! ```

use libfuzzer_sys::fuzz_target;
use tst_core::mpegts::demux::{VideoCodec, split_video, split_video_strict};
use tst_core::mpegts::mux::Av1CarriageMode;
use tst_core::shared::SharedBytes;

fuzz_target!(|data: &[u8]| {
    // Need at least the selector byte.
    if data.is_empty() {
        return;
    }

    let selector = data[0];
    let payload = data[1..].to_vec();

    // Derive VideoCodec from bits [1:0] — all 4 variants reachable.
    let codec = match selector & 0b11 {
        0 => VideoCodec::H264,
        1 => VideoCodec::H265,
        2 => VideoCodec::H266,
        _ => VideoCodec::Av1, // 3
    };

    // Derive Av1CarriageMode from bit [2].
    let carriage = if (selector >> 2) & 1 == 0 {
        Av1CarriageMode::Mpeg2TsBinding
    } else {
        Av1CarriageMode::InteropRawObu
    };

    let raw = SharedBytes::from_vec(payload);

    // split_video: returns (VideoPayload, Vec<NonConformantIssue>) — must never panic.
    let _ = split_video(&raw, codec, carriage);

    // split_video_strict: returns Result — Err is normal, panic is not.
    let _ = split_video_strict(&raw, codec, carriage);
});
