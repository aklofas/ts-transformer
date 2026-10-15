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
