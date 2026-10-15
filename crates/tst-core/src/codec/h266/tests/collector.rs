//! `parse_parameter_sets` collector tests.

use crate::codec::NalUnit;
use crate::codec::h266::parse_parameter_sets;

#[test]
fn parse_parameter_sets_partial_success_one_bad_sps() {
    // Bad SPS (truncated) + good PPS (minimal valid bytes from pps.rs
    // tests). The collector should warn-and-skip the SPS, keep the
    // PPS, and return Ok with vpses/spses empty + ppses=1.
    let bad_sps = NalUnit::H266 {
        nal_type: 15, // SPS_NUT
        layer_id: 0,
        temporal_id_plus1: 1,
        payload: vec![0x00].into(), // truncated — fails on max_sublayers read
    };
    let good_pps = NalUnit::H266 {
        nal_type: 16, // PPS_NUT
        layer_id: 0,
        temporal_id_plus1: 1,
        payload: vec![0x00, 0x20].into(), // minimal valid PPS
    };
    let result = parse_parameter_sets(&[bad_sps, good_pps]);
    let sets = result.expect("should not fail when at least one parses");
    assert_eq!(sets.vpses.len(), 0);
    assert_eq!(sets.spses.len(), 0);
    assert_eq!(sets.ppses.len(), 1);
}

#[test]
fn parse_parameter_sets_all_bad_returns_err() {
    // Single VPS with empty payload — the only parameter-set NAL fails,
    // so the collector returns EngineError per the all_failed branch.
    let bad_vps = NalUnit::H266 {
        nal_type: 14,
        layer_id: 0,
        temporal_id_plus1: 1,
        payload: vec![].into(),
    };
    let result = parse_parameter_sets(&[bad_vps]);
    assert!(result.is_err());
}

#[test]
fn parse_parameter_sets_no_h266_nals_returns_empty_ok() {
    // Empty input is OK — no NALs seen means no failures, so the
    // collector returns an empty H266ParameterSets.
    let sets = parse_parameter_sets(&[]).expect("empty input ok");
    assert!(sets.vpses.is_empty());
    assert!(sets.spses.is_empty());
    assert!(sets.ppses.is_empty());
}
