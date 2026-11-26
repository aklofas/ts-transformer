#![no_main]

//! Fuzz target — the MISP timestamp SEI's extract and insert on arbitrary
//! bytes, H.264 and H.265.
//!
//! `extract` reads the whole input as an access unit. The insert takes the
//! first byte as a split point: the next that many bytes are the SEI block
//! to insert (one NAL unit or several joined by start codes, as a caller may
//! pass), the rest is the access unit. The builder's own MISP SEI is
//! inserted into the same access unit too. Neither may panic, and an insert
//! that succeeds returns at least the bytes it was given.

use libfuzzer_sys::fuzz_target;
use tst_core::codec::misp_time::{self, MispTimestamp};
use tst_core::mpegts::mux::VideoCodec;

fuzz_target!(|data: &[u8]| {
    let _ = misp_time::extract(data, VideoCodec::H264);
    let _ = misp_time::extract(data, VideoCodec::H265);

    let Some((&split, rest)) = data.split_first() else {
        return;
    };
    let (block, au) = rest.split_at(usize::from(split).min(rest.len()));
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        let built = misp_time::build_sei_nal(codec, &MispTimestamp::micros(1, 0x1F))
            .expect("the builder takes a microsecond timestamp on both codecs");
        for sei in [block, &built[..]] {
            if let Ok(out) = misp_time::insert_sei_before_first_vcl(au, sei, codec) {
                assert!(out.len() >= au.len() + sei.len());
                let _ = misp_time::extract(&out, codec);
            }
        }
    }
});
