//! `parse_parameter_sets` collector tests.

use crate::codec::NalUnit;
use crate::codec::h265::parse_parameter_sets;
use crate::test_prelude::*;

fn nal(nt: u8, payload: Vec<u8>) -> NalUnit {
    NalUnit::H265 {
        nal_type: nt,
        layer_id: 0,
        temporal_id_plus1: 1,
        payload: payload.into(),
    }
}

const VPS: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main40_vps.bin");
const SPS: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");
const PPS: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main40_pps.bin");
const SPS_MAIN10: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main10_50_pq_sps.bin");

#[test]
fn parse_parameter_sets_collects_all_three() {
    let nals = vec![
        nal(32, VPS.into()),
        nal(33, SPS.into()),
        nal(34, PPS.into()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.vps_by_id.len(), 1);
    assert_eq!(ps.sps_by_id.len(), 1);
    assert_eq!(ps.pps_by_id.len(), 1);
    assert_eq!(ps.sps_by_id[&0].width, 1920);
}

#[test]
fn parse_parameter_sets_skips_h264_nals_silently() {
    let nals = vec![
        NalUnit::H264 {
            nal_type: 7,
            ref_idc: 3,
            payload: vec![0; 8].into(),
        },
        nal(33, SPS.into()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
}

#[test]
fn parse_parameter_sets_empty_returns_ok_empty() {
    let ps = parse_parameter_sets(&[]).expect("parse");
    assert!(ps.vps_by_id.is_empty());
}

#[test]
fn parse_parameter_sets_only_slice_nals_returns_ok_empty() {
    let nals = vec![nal(0, vec![0; 16])];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert!(ps.sps_by_id.is_empty());
}

#[test]
fn parse_parameter_sets_partial_success_one_bad_vps() {
    let nals = vec![
        nal(32, VPS.into()),
        nal(32, vec![0xff; 8]),
        nal(33, SPS.into()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.vps_by_id.len(), 1);
    assert_eq!(ps.sps_by_id.len(), 1);
}

/// `pps_pic_parameter_set_id = 0`, `pps_seq_parameter_set_id = 3`.
const PPS_NAMING_SPS_3: &[u8] = &[0x92];

#[test]
fn pps_referencing_missing_sps_is_dropped() {
    let nals = vec![nal(33, SPS.into()), nal(34, PPS_NAMING_SPS_3.into())];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
    assert!(ps.pps_by_id.is_empty(), "PPS naming SPS 3 must be dropped");
}

#[test]
fn a_pps_whose_sps_failed_in_the_same_call_is_dropped() {
    let nals = vec![nal(33, SPS[..4].into()), nal(34, PPS.into())];
    let ps = parse_parameter_sets(&nals).expect("the PPS parsed: Ok");
    assert!(ps.sps_by_id.is_empty());
    assert!(ps.pps_by_id.is_empty());
}

/// The order inside a call does not matter: a PPS before its SPS is kept.
#[test]
fn pps_with_its_sps_in_the_same_call_is_kept() {
    for nals in [
        vec![nal(33, SPS.into()), nal(34, PPS.into())],
        vec![nal(34, PPS.into()), nal(33, SPS.into())],
    ] {
        let ps = parse_parameter_sets(&nals).expect("parse");
        assert_eq!(ps.pps_by_id[&0].pps_seq_parameter_set_id, 0);
        assert_eq!(ps.sps_by_id[&0].width, 1920);
    }
}

#[test]
fn a_lone_pps_gives_ok_with_empty_results() {
    let ps = parse_parameter_sets(&[nal(34, PPS.into())]).expect("a lone PPS is not an error");
    assert!(ps.vps_by_id.is_empty());
    assert!(ps.sps_by_id.is_empty());
    assert!(ps.pps_by_id.is_empty());
}

/// Several SPS with one id in a call: the last one that parsed is used, and a
/// PPS naming that id is kept.
#[test]
fn with_two_sps_of_one_id_the_last_that_parsed_is_used() {
    let ps = parse_parameter_sets(&[
        nal(33, SPS[..4].into()),
        nal(33, SPS.into()),
        nal(34, PPS.into()),
    ])
    .expect("parse");
    assert_eq!(ps.sps_by_id[&0].raw_rbsp, SPS);
    assert_eq!(ps.pps_by_id.len(), 1);

    let other = SPS_MAIN10.to_vec();
    let ps = parse_parameter_sets(&[nal(33, SPS.into()), nal(33, other.clone())]).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
    assert_eq!(ps.sps_by_id[&0].raw_rbsp, other);
}
