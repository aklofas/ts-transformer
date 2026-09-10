#![no_main]

//! Fuzz target — `codec::nal_framing` panic-freedom + converter agreement.
//!
//! Exercises the Annex-B ↔ length-prefixed converters and the
//! parameter-set extractor on arbitrary bytes. The property under test is
//! that none of them panics on any input: an `Err` return is a normal
//! outcome, an unwinding panic (slice index, arithmetic overflow, a
//! tripped `unreachable!`) is a bug.
//!
//! Beyond panic-freedom the target pins the three sizing/writing entry
//! points to each other — `annexb_to_length_prefixed` (the `Vec` path),
//! `annexb_to_length_prefixed_len` (sizing) and
//! `annexb_to_length_prefixed_into` (direct write) must accept and reject
//! exactly the same inputs and produce exactly the same bytes. That
//! agreement is what the C ABI's size-then-write two-call idiom rests on,
//! so a divergence found here is a real defect, not just a missing
//! assertion.
//!
//! # Input layout
//!
//! ```text
//! [0]     length_size byte — passed through RAW, not masked: values
//!           other than 1/2/4 must produce CodecParseError::InvalidLengthSize
//!           from all three entry points rather than a panic.
//! [1]     codec byte → 0 = H264, 1 = H265; any other value skips the
//!           parameter-set extraction (extract_parameter_sets is only
//!           implemented for those two — H266/AV1 return empty by
//!           construction and are covered by the module's unit tests).
//! [2..]   raw Annex-B bytes
//! ```

use libfuzzer_sys::fuzz_target;
use tst_core::codec::nal_framing::{
    annexb_to_length_prefixed, annexb_to_length_prefixed_into, annexb_to_length_prefixed_len,
    extract_parameter_sets, length_prefixed_to_annexb,
};
use tst_core::mpegts::demux::VideoCodec;

fuzz_target!(|data: &[u8]| {
    // Need the two selector bytes; the Annex-B tail may legitimately be
    // empty (an empty buffer converts to an empty output).
    if data.len() < 2 {
        return;
    }
    let length_size = data[0];
    let codec_byte = data[1];
    let annexb = &data[2..];

    let vec_result = annexb_to_length_prefixed(annexb, length_size);
    let len_result = annexb_to_length_prefixed_len(annexb, length_size);

    match (&vec_result, &len_result) {
        (Ok(prefixed), Ok(needed)) => {
            assert_eq!(prefixed.len(), *needed, "_len disagrees with vec.len()");

            // _into must write byte-identical output into an
            // exactly-sized buffer and report the same count.
            let mut out = vec![0u8; *needed];
            let written = annexb_to_length_prefixed_into(annexb, length_size, &mut out)
                .expect("_into refused an input the Vec path accepted");
            assert_eq!(written, *needed, "_into wrote a different byte count");
            assert_eq!(out, *prefixed, "_into wrote different bytes");

            // Documented contract: a buffer shorter than the conversion
            // needs is refused with BufferTooSmall and leaves `out`
            // completely unmodified. Only probe when there is a strictly
            // smaller non-degenerate buffer to probe with.
            if *needed > 0 {
                let mut short = vec![0xA5u8; *needed - 1];
                let err = annexb_to_length_prefixed_into(annexb, length_size, &mut short)
                    .expect_err("_into accepted an undersized buffer");
                assert!(
                    matches!(err, tst_core::codec::CodecParseError::BufferTooSmall { .. }),
                    "undersized _into raised {err:?}, not BufferTooSmall"
                );
                assert!(
                    short.iter().all(|&b| b == 0xA5),
                    "_into modified the caller's buffer on the error path"
                );
            }

            // Round-trip. `length_prefixed_to_annexb` is the inverse only
            // up to start-code width normalization (a 3-byte start code
            // comes back as a 4-byte one), so the original bytes are NOT
            // recoverable in general and asserting that would be wrong.
            // What the pair does guarantee is that the normalized form is
            // a fixed point: re-converting it must reproduce the very same
            // length-prefixed bytes, since the NAL bodies are unchanged.
            let annexb2 = length_prefixed_to_annexb(prefixed, length_size)
                .expect("decoding our own length-prefixed output failed");
            let reprefixed = annexb_to_length_prefixed(&annexb2, length_size)
                .expect("re-encoding our own Annex-B output failed");
            assert_eq!(reprefixed, *prefixed, "round-trip is not a fixed point");
        }
        (Err(ve), Err(le)) => {
            assert_eq!(ve, le, "_len raised a different error than the Vec path");
            // _into sizes before it checks capacity, so it must surface
            // the identical error regardless of the buffer it is handed —
            // an empty one included.
            let mut out: Vec<u8> = Vec::new();
            let into_err = annexb_to_length_prefixed_into(annexb, length_size, &mut out)
                .expect_err("_into accepted an input the Vec path refused");
            assert_eq!(&into_err, ve, "_into raised a different error");
        }
        (vr, lr) => panic!("_len and the Vec path disagree on acceptance: {vr:?} vs {lr:?}"),
    }

    // Independent of the conversions above: the length-prefixed decoder
    // must survive arbitrary bytes (truncated prefixes, lengths that
    // overrun the buffer, invalid length_size).
    let _ = length_prefixed_to_annexb(annexb, length_size);

    // Parameter-set extraction is non-fallible by contract — it must
    // return empty sets rather than error or panic on malformed input.
    let codec = match codec_byte {
        0 => Some(VideoCodec::H264),
        1 => Some(VideoCodec::H265),
        _ => None,
    };
    if let Some(codec) = codec {
        let sets = extract_parameter_sets(annexb, codec);
        // Every collected NAL is a complete NAL body with its header
        // byte, so none can be empty — `classify_parameter_set` reads
        // byte 0 to classify and returns early on an empty NAL.
        for nal in sets.vps.iter().chain(&sets.sps).chain(&sets.pps) {
            assert!(!nal.is_empty(), "extracted an empty parameter-set NAL");
        }
    }
});
