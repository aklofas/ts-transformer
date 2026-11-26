//! `parse_parameter_sets` collector tests.

use crate::codec::NalUnit;
use crate::codec::h266::parse_parameter_sets;
use crate::test_prelude::*;

#[test]
fn parse_parameter_sets_partial_success_one_bad_sps() {
    // Bad SPS (truncated) + good PPS (minimal valid bytes from pps.rs
    // tests). The collector warns and skips the SPS; the PPS names that
    // SPS, which did not parse in this call, so it is dropped too. Ok (the
    // PPS parsed, so not every NAL failed) with all three lists empty.
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
    assert_eq!(sets.ppses.len(), 0);
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

fn nal(nal_type: u8, payload: &[u8]) -> NalUnit {
    NalUnit::H266 {
        nal_type,
        layer_id: 0,
        temporal_id_plus1: 1,
        payload: payload.to_vec().into(),
    }
}

const SPS: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h266/h266_320x240_main10_sps.bin");
const REAL_SPS: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h266/h266_320x240_main10_real_sps.bin");
/// `pps_pic_parameter_set_id = 0`, `pps_seq_parameter_set_id = 0`.
const PPS: &[u8] = &[0x00, 0x20];
/// `pps_pic_parameter_set_id = 0`, `pps_seq_parameter_set_id = 3`.
const PPS_NAMING_SPS_3: &[u8] = &[0x00, 0xE0];

#[test]
fn pps_referencing_missing_sps_is_dropped() {
    let sets = parse_parameter_sets(&[nal(15, SPS), nal(16, PPS_NAMING_SPS_3)]).expect("parse");
    assert_eq!(sets.spses.len(), 1);
    assert_eq!(sets.ppses.len(), 0, "a PPS naming SPS 3 must be dropped");
}

#[test]
fn a_lone_pps_gives_ok_with_empty_results() {
    let sets = parse_parameter_sets(&[nal(16, PPS)]).expect("a lone PPS is not an error");
    assert!(sets.vpses.is_empty());
    assert!(sets.spses.is_empty());
    assert!(sets.ppses.is_empty());
}

/// The order inside a call does not matter: a PPS before its SPS is kept.
#[test]
fn pps_with_its_sps_in_the_same_call_is_kept() {
    for nals in [[nal(15, SPS), nal(16, PPS)], [nal(16, PPS), nal(15, SPS)]] {
        let sets = parse_parameter_sets(&nals).expect("parse");
        assert_eq!(sets.ppses.len(), 1);
        assert_eq!(sets.ppses[0].sps_id, 0);
        assert_eq!(sets.spses[0].width, 320);
    }
}

/// Several SPS with one id in a call: the last one that parsed is used, and a
/// PPS naming that id is kept.
#[test]
fn with_two_sps_of_one_id_the_last_that_parsed_is_used() {
    let sets =
        parse_parameter_sets(&[nal(15, SPS), nal(15, &SPS[..4]), nal(16, PPS)]).expect("parse");
    assert_eq!(sets.spses.len(), 1);
    assert_eq!(sets.spses[0].raw_rbsp, SPS);
    assert_eq!(sets.ppses.len(), 1);

    let sets = parse_parameter_sets(&[nal(15, SPS), nal(15, REAL_SPS)]).expect("parse");
    assert_eq!(sets.spses.len(), 1);
    assert_eq!(sets.spses[0].raw_rbsp, REAL_SPS);
}

/// The lists come out in ascending id order, not in the order of the call.
#[test]
fn the_lists_are_in_ascending_id_order() {
    // pps_pic_parameter_set_id 5 and 1, both naming SPS 0.
    let sets =
        parse_parameter_sets(&[nal(15, SPS), nal(16, &[0x14, 0x20]), nal(16, &[0x04, 0x20])])
            .expect("parse");
    let ids: Vec<u8> = sets.ppses.iter().map(|p| p.pps_id).collect();
    assert_eq!(ids, [1, 5]);
}
