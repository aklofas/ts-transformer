#![no_main]
//! Fuzz target — H.265 light slice segment header parser, no-SPS and
//! SPS-context paths.
//!
//! # Input layout
//!
//! ```text
//! [0]     selector byte:
//!           bit 7 = 0 → no-SPS path, nal_unit_type 19 (IDR_W_RADL)
//!           bit 7 = 1 → SPS-context path (the 1080p Main fixture's SPS):
//!                        log2_max_pic_order_cnt_lsb_minus4 = sel % 13,
//!                        bit 1 sets separate_colour_plane (the first
//!                        segment then carries colour_plane_id),
//!                        bit 0 → nal_unit_type: 0 = TRAIL_R (type 1),
//!                        1 = IDR_W_RADL (type 19)
//! [1..]   slice RBSP bytes (fuzz-driven)
//! ```

use libfuzzer_sys::fuzz_target;
use tst_core::codec::h265::{parse_slice_header_light, parse_sps};

const SPS_1080P_MAIN40: &[u8] =
    include_bytes!("../../tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rbsp)) = data.split_first() else {
        return;
    };
    if sel & 0x80 == 0 {
        // No-SPS path (most adversarial — exercises every bit-cursor branch
        // without bit-width SPS dependency); type 19 also takes the idr branch.
        let _ = parse_slice_header_light(rbsp, None, 19);
        return;
    }
    let sps = parse_sps(SPS_1080P_MAIN40);
    debug_assert!(sps.is_ok(), "the fixture SPS must parse: SPS-context branch is dead");
    let Ok(mut sps) = sps else {
        return;
    };
    // The fields the light parser reads from the SPS, chosen by the selector.
    sps.log2_max_pic_order_cnt_lsb_minus4 = sel % 13;
    sps.separate_colour_plane = sel & 0b10 != 0;
    let nal_type: u8 = if sel & 1 == 0 { 1 } else { 19 };
    let _ = parse_slice_header_light(rbsp, Some(&sps), nal_type);
});
