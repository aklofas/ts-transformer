//! Integration test: real VVenC SPS bytes → parse → field surface.
//!
//! Confirms the full SPS parse path works end-to-end
//! on real VVenC output — body walk (AbsDeltaPocSt fix, timing_hrd walk) +
//! VUI walker (parse_h266_vui per §7.3.2.5 / §E.2.1).
//!
//! The fixture is a 236-byte SPS EBSP payload from a VVenC 320×240 Main10
//! elementary stream encoded at 30fps with `--preset faster`. VVenC does not
//! emit `sps_vui_parameters_present_flag=1` in this configuration, so
//! `color_info` is `None`. Frame rate IS recovered from
//! `general_timing_hrd_parameters()` (§7.3.5.1): `num_units_in_tick=1`,
//! `time_scale=30` → 30 fps.

use tst_core::codec::ChromaFormat;
use tst_core::codec::h266::{H266Sps, parse_sps};

const REAL_VVENC_SPS: &[u8] =
    include_bytes!("../fixtures/codec/h266/h266_320x240_main10_real_sps.bin");

#[test]
fn parse_real_vvenc_main10_sps_recovers_dimensions_and_frame_rate() {
    let sps: H266Sps = parse_sps(REAL_VVENC_SPS).expect("parse real VVenC SPS");
    assert_eq!(sps.width, 320, "width");
    assert_eq!(sps.height, 240, "height");
    assert_eq!(sps.bit_depth_luma, 10, "10-bit luma");
    assert_eq!(sps.bit_depth_chroma, 10, "10-bit chroma");
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420, "4:2:0 chroma");
    assert_eq!(sps.crop_left, 0);
    assert_eq!(sps.crop_right, 0);
    assert_eq!(sps.crop_top, 0);
    assert_eq!(sps.crop_bottom, 0);

    // general_timing_hrd_parameters() provides num_units=1, time_scale=30.
    let fr = sps
        .frame_rate
        .expect("frame_rate recovered from general_timing_hrd_parameters");
    let ratio = fr.num as f64 / fr.den as f64;
    assert!(
        (ratio - 30.0).abs() < 0.5,
        "frame_rate ≈ 30 fps; got {:?}",
        fr
    );

    // VVenC at this encoding profile does not emit VUI
    // (sps_vui_parameters_present_flag=0), so color_info is None.
    assert!(
        sps.color_info.is_none(),
        "VVenC fixture has no VUI in this profile; color_info must be None"
    );
}
