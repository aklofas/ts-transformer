use tst_core::codec::misp_time::{MispTimestamp, build_sei_nal};
use tst_webrtc::VideoCodec;
use tst_webrtc::sei::{TST_KLV_SEI_UUID, build_klv_sei_nal, extract_klv_seis, parse_klv_sei_rbsp};

fn au_with(sei: &[u8], codec: VideoCodec) -> Vec<u8> {
    let (sps, idr) = match codec {
        VideoCodec::H264 => (vec![0x67, 0x42, 0x00, 0x1e], vec![0x65, 0x88, 0x80]),
        _ => (vec![0x42, 0x01, 0x01], vec![0x26, 0x01, 0xAF]),
    };
    let mut v = vec![0, 0, 0, 1];
    v.extend(&sps);
    v.extend([0, 0, 1]);
    v.extend(sei);
    v.extend([0, 0, 1]);
    v.extend(&idr);
    v
}

fn build(codec: VideoCodec, pts: u64, klv: &[u8]) -> Vec<u8> {
    build_klv_sei_nal(codec, pts, klv).expect("H.264 and H.265 are supported")
}

#[test]
fn round_trip_small_record_h264() {
    let klv = b"\x06\x0e\x2b\x34\x02\x0b\x01\x01\x0e\x01\x03\x01\x01\x00\x00\x00\x03\x02\x01\x07"
        .to_vec();
    let nal = build(VideoCodec::H264, 0x1_2345_6789, &klv);
    assert_eq!(nal[0], 0x06);
    assert_eq!(nal[1], 0x05);
    let got = extract_klv_seis(&au_with(&nal, VideoCodec::H264), VideoCodec::H264);
    // KlvSei is #[non_exhaustive], so compare its fields.
    let fields: Vec<(u64, &[u8])> = got.iter().map(|s| (s.klv_pts_90k, &s.klv[..])).collect();
    assert_eq!(fields, vec![(0x1_2345_6789, &klv[..])]);
}

#[test]
fn round_trip_h265_prefix_sei() {
    let klv = vec![0xAA; 40];
    let nal = build(VideoCodec::H265, 7, &klv);
    assert_eq!(&nal[..2], &[0x4E, 0x01]);
    assert_eq!(
        extract_klv_seis(&au_with(&nal, VideoCodec::H265), VideoCodec::H265)[0].klv,
        klv
    );
}

#[test]
fn payload_size_above_255_uses_ff_runs() {
    let klv = vec![0x5A; 600]; // payload = 25 + 600 = 625 = 2*255 + 115
    let nal = build(VideoCodec::H264, 0, &klv);
    assert_eq!(&nal[2..5], &[0xFF, 0xFF, 115]);
    assert_eq!(
        extract_klv_seis(&au_with(&nal, VideoCodec::H264), VideoCodec::H264)[0]
            .klv
            .len(),
        600
    );
}

#[test]
fn emulation_prevention_survives_zero_runs_in_klv_and_pts() {
    let klv = vec![0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 0];
    let nal = build(VideoCodec::H264, 0x0000_0000_0000_0001, &klv);
    assert!(
        !nal.windows(3)
            .any(|w| w == [0, 0, 0] || w == [0, 0, 1] || w == [0, 0, 2]),
        "escaped NAL must contain no start-code-like triple"
    );
    assert_eq!(
        extract_klv_seis(&au_with(&nal, VideoCodec::H264), VideoCodec::H264)[0].klv,
        klv
    );
}

#[test]
fn foreign_sei_and_misp_timestamp_are_skipped() {
    let misp = build_sei_nal(VideoCodec::H264, &MispTimestamp::micros(1, 0x1F)).unwrap();
    let ours = build(VideoCodec::H264, 9, b"klv");
    let mut au = au_with(&misp, VideoCodec::H264);
    au.extend([0, 0, 1]);
    au.extend(&ours);
    let got = extract_klv_seis(&au, VideoCodec::H264);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].klv_pts_90k, 9);
}

#[test]
fn truncated_payload_is_skipped_not_panicked() {
    let nal = build(VideoCodec::H264, 1, &[1, 2, 3, 4, 5, 6, 7, 8]);
    for cut in 1..nal.len() {
        let _ = parse_klv_sei_rbsp(&nal[1..cut]);
    }
    assert!(parse_klv_sei_rbsp(&nal[1..nal.len() - 3]).is_none());
}

#[test]
fn uuid_is_the_documented_one() {
    assert_eq!(&TST_KLV_SEI_UUID[..6], b"tstKLV");
}
