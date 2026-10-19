#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_webrtc::VideoCodec;
use tst_webrtc::sei::{build_klv_sei_nal, extract_klv_seis, parse_klv_sei_rbsp};

fuzz_target!(|data: &[u8]| {
    // Property 1: the parsers never panic on arbitrary bytes, whether read
    // as one SEI body or as an Annex-B access unit of either codec.
    let _ = parse_klv_sei_rbsp(data);
    let _ = extract_klv_seis(data, VideoCodec::H264);
    let _ = extract_klv_seis(data, VideoCodec::H265);

    // Property 2: the builder's own output parses back to the same PTS and
    // KLV bytes (the first 8 input bytes are the PTS, the rest the record).
    if data.len() >= 9 {
        let pts = u64::from_le_bytes(data[..8].try_into().unwrap());
        let nal = build_klv_sei_nal(VideoCodec::H264, pts, &data[8..]).expect("H.264 is supported");
        let got = parse_klv_sei_rbsp(&nal[1..]).expect("own output parses");
        assert_eq!(got.klv, &data[8..]);
        assert_eq!(got.klv_pts_90k, pts);
    }
});
