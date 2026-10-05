//! Integration test: real x265 SPS bytes → parse → field surface.
//!
//! Confirms the full SPS parse path (including the RPS walker) works end-to-end
//! on real x265 output — not just synthetic walker tests + synthetic SPS
//! fixtures.
//!
//! Note: real x265 with `repeat-headers=1` encodes RPS in slice headers
//! rather than the SPS, so these fixtures have `num_short_term_ref_pic_sets=0`
//! and the walker runs as a no-op. The walker itself is tested by the
//! synthetic helpers in `codec::h265::short_term_rps::tests` and the
//! `parse_sps_walks_past_short_term_rps_*` tests in `codec::h265::sps_tests`.
//! This integration test is the end-to-end check on real bytes (VUI, PTL,
//! conformance window, color signaling).

use tst_core::codec::h265::{H265Sps, parse_sps};
use tst_core::codec::{ColourPrimaries, MatrixCoefficients, TransferCharacteristics};

const REAL_MAIN40_SPS: &[u8] = include_bytes!("../fixtures/codec/h265/h265_1080p_main40_sps.bin");
const REAL_MAIN10_PQ_SPS: &[u8] =
    include_bytes!("../fixtures/codec/h265/h265_1080p_main10_50_pq_sps.bin");

#[test]
fn parse_real_x265_main40_sps_produces_full_field_set() {
    let sps: H265Sps = parse_sps(REAL_MAIN40_SPS).expect("parse real x265 Main SPS");
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.bit_depth_luma, 8);
    assert_eq!(sps.general_level_idc, 120);
    assert!(
        sps.frame_rate.is_some(),
        "real x265 emits VUI; frame_rate must populate, got {:?}",
        sps.frame_rate
    );
}

#[test]
fn parse_real_x265_main10_pq_sps_recovers_pq_color() {
    let sps: H265Sps = parse_sps(REAL_MAIN10_PQ_SPS).expect("parse real x265 Main10/PQ SPS");
    let color = sps
        .color
        .expect("VUI color present in real x265 Main10/PQ output");
    assert_eq!(color.primaries, ColourPrimaries::Bt2020);
    assert_eq!(color.transfer, TransferCharacteristics::SmpteSt2084);
    assert_eq!(color.matrix, MatrixCoefficients::Bt2020NonConstant);
    assert_eq!(sps.bit_depth_luma, 10);
}
