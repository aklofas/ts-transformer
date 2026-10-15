//! The Annex-B scanner, the NAL-type table and the SEI helpers as a
//! consumer outside tst-core uses them (an RTP packetizer, a WebRTC mount).

use tst_core::codec::VideoCodec;
use tst_core::codec::annexb::{StartCode, nals, start_codes};
use tst_core::codec::misp_time::{
    append_rbsp_escaped, build_user_data_unregistered_sei_nal, insert_sei_before_first_vcl,
};
use tst_core::codec::nal::{header_len, is_idr, is_vcl, nal_unit_type};

#[test]
fn nals_split_three_and_four_byte_start_codes() {
    let au = [
        0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x65, 0xCC, 0xDD,
    ];
    let v: Vec<&[u8]> = nals(&au).collect();
    assert_eq!(
        v,
        vec![
            &[0x67, 0xAA][..],
            &[0x68, 0xBB][..],
            &[0x65, 0xCC, 0xDD][..]
        ]
    );
    let codes: Vec<StartCode> = start_codes(&au).collect();
    assert_eq!(
        codes,
        vec![
            StartCode {
                prefix_start: 0,
                data_start: 4
            },
            StartCode {
                prefix_start: 6,
                data_start: 9
            },
            StartCode {
                prefix_start: 11,
                data_start: 14
            },
        ]
    );
}

#[test]
fn nal_type_table_matches_the_specs() {
    // H.264 §7.3.1: forbidden(1) | nal_ref_idc(2) | type(5).
    assert_eq!(nal_unit_type(VideoCodec::H264, &[0x65]), Some(5));
    assert!(is_idr(VideoCodec::H264, 5) && is_vcl(VideoCodec::H264, 5));
    assert!(
        is_vcl(VideoCodec::H264, 1) && !is_vcl(VideoCodec::H264, 6) && !is_idr(VideoCodec::H264, 1)
    );
    // H.265 §7.3.1.2: forbidden(1) | type(6) | layer(6) | tid+1(3). IDR_W_RADL = 19.
    assert_eq!(nal_unit_type(VideoCodec::H265, &[0x26, 0x01]), Some(19));
    assert!(
        is_idr(VideoCodec::H265, 19)
            && is_idr(VideoCodec::H265, 20)
            && !is_idr(VideoCodec::H265, 21)
    );
    assert!(is_vcl(VideoCodec::H265, 31) && !is_vcl(VideoCodec::H265, 32));
    // H.266 §7.3.1.2: type is in byte 1's top five bits. IDR_W_RADL = 7.
    assert_eq!(
        nal_unit_type(VideoCodec::H266, &[0x00, (7 << 3) | 1]),
        Some(7)
    );
    assert!(
        is_idr(VideoCodec::H266, 7) && is_idr(VideoCodec::H266, 8) && is_vcl(VideoCodec::H266, 11)
    );
    assert!(!is_vcl(VideoCodec::H266, 12)); // OPI_NUT
    assert_eq!(header_len(VideoCodec::H264), Some(1));
    assert_eq!(header_len(VideoCodec::H265), Some(2));
    assert_eq!(header_len(VideoCodec::Av1), None);
    assert_eq!(nal_unit_type(VideoCodec::Av1, &[0x12, 0x00]), None);
    assert_eq!(nal_unit_type(VideoCodec::H264, &[]), None);
}

#[test]
fn escaper_inserts_emulation_prevention_after_two_zeros() {
    let mut out = Vec::new();
    append_rbsp_escaped(&mut out, &[0, 0, 1, 0, 0, 0, 0, 0, 3]);
    assert_eq!(out, vec![0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 3]);
}

#[test]
fn sei_lands_before_the_first_vcl_nal() {
    let au = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0xCC];
    let sei = [0x06, 0x05, 0x01, 0xEE, 0x80];
    let out = insert_sei_before_first_vcl(&au, &sei, VideoCodec::H264).unwrap();
    let at = out.windows(5).position(|w| w == sei).expect("sei present");
    let idr = out.windows(2).position(|w| w == [0x65, 0xCC]).unwrap();
    assert!(at < idr);
}

#[test]
fn user_data_unregistered_sei_uses_ff_size_runs() {
    // H.264 §7.3.2.3.1: payloadSize = 255*k + last byte; 16-byte UUID + 600 = 616 = 2*255 + 106.
    let uuid = [0x11; 16];
    let nal = build_user_data_unregistered_sei_nal(VideoCodec::H264, &uuid, &[0x5A; 600]).unwrap();
    assert_eq!(&nal[..5], &[0x06, 0x05, 0xFF, 0xFF, 106]);
    assert_eq!(*nal.last().unwrap(), 0x80);
    let h265 = build_user_data_unregistered_sei_nal(VideoCodec::H265, &uuid, b"x").unwrap();
    assert_eq!(&h265[..4], &[0x4E, 0x01, 0x05, 17]);
    assert!(build_user_data_unregistered_sei_nal(VideoCodec::H266, &uuid, b"x").is_err());
}
