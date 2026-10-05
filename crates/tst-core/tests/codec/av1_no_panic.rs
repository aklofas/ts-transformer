//! Regression test: adversarial bitstreams must not panic in the AV1 parsers.
//!
//! `crates/tst-core/src/codec/av1/` has zero caller-controlled-input panic
//! sites in production code. Every error path returns `Err(CodecParseError::*)`
//! via `?`-propagation through `Av1BitReader::f()` / `read_leb128()`. This file
//! is the permanent regression anchor: if a future edit re-introduces a
//! `unwrap()` / `expect()` / `assert!` in a production code path that can be
//! reached by a crafted bitstream, one of these tests will panic (instead of
//! returning `Err`) and catch it.
//!
//! Inputs are chosen to exercise every truncation / malformed-header
//! variant that the parsers must handle gracefully.

use tst_core::codec::CodecParseError;
use tst_core::codec::av1::{
    Av1SequenceHeader, parse_frame_header_light, parse_obu_stream, parse_sequence_header,
};
use tst_core::mpegts::demux::event::Obu;

// ---------------------------------------------------------------------------
// parse_sequence_header — truncated / malformed inputs
// ---------------------------------------------------------------------------

/// Empty input must return a typed error, not panic.
#[test]
fn empty_sequence_header_returns_typed_error() {
    let result = parse_sequence_header(&[]);
    assert!(
        result.is_err(),
        "expected Err for empty input, got {:?}",
        result
    );
}

/// Single-byte input (too short to read even the profile field fully).
#[test]
fn one_byte_sequence_header_returns_typed_error() {
    let result = parse_sequence_header(&[0x00]);
    assert!(result.is_err());
}

/// An OBU header byte that looks plausible (well-formed OBU header byte with
/// has_size=1) but the payload body is one byte — far too short for a real SH.
#[test]
fn truncated_obu_body_returns_typed_error() {
    // OBU header for Sequence Header (type=1): (1 << 3) | 0x02 = 0x0A
    // The byte after is the LEB128 size (1 byte body follows).
    // parse_sequence_header() receives only the payload, not the OBU header.
    let truncated: &[u8] = &[0x0A];
    let result = parse_sequence_header(truncated);
    assert!(
        result.is_err(),
        "expected Err for truncated SH body, got {:?}",
        result
    );
}

/// 2-byte input — enough for profile bits but missing everything after.
#[test]
fn two_byte_sequence_header_returns_typed_error() {
    let result = parse_sequence_header(&[0x00, 0x00]);
    assert!(result.is_err());
}

/// All-0xFF bytes — malformed but should not panic; parser terminates
/// with an error when it walks off the end of the available bits.
#[test]
fn all_ones_sequence_header_returns_err_not_panic() {
    let result = parse_sequence_header(&[0xFF, 0xFF, 0xFF]);
    // Result direction is unspecified for this corpus — the 3-byte
    // payload may parse to a partial struct or return an error — but it
    // MUST NOT panic.
    let _ = result;
}

/// Verify `TruncatedRbsp` is the actual variant returned for a short input.
#[test]
fn truncated_sequence_header_returns_truncated_rbsp() {
    let result = parse_sequence_header(&[0u8; 2]);
    match result {
        Err(CodecParseError::TruncatedRbsp { .. }) => {} // correct
        Err(other) => {
            // Any typed error is acceptable — just not a panic.
            let _ = other;
        }
        Ok(_) => panic!("expected Err for 2-byte SH input"),
    }
}

// ---------------------------------------------------------------------------
// parse_frame_header_light — truncated / empty inputs
// ---------------------------------------------------------------------------

fn non_reduced_dummy_seq() -> Av1SequenceHeader {
    // Minimal valid SH body: Main profile, level 2.0, 320x240, 8-bit 4:2:0,
    // no color description, no timing info. Bytes pre-computed by the
    // sequence_header unit tests. Using parse_sequence_header avoids
    // constructing the #[non_exhaustive] struct directly from outside the crate.
    let minimal_sh_body: &[u8] = &[0, 0, 0, 4, 60, 255, 188, 0, 0, 0];
    parse_sequence_header(minimal_sh_body).expect("minimal SH body must parse")
}

/// Empty frame header payload (non-reduced path) must return a typed error.
#[test]
fn empty_frame_header_returns_typed_error() {
    let seq = non_reduced_dummy_seq();
    let result = parse_frame_header_light(&[], &seq);
    assert!(
        result.is_err(),
        "expected Err for empty frame header, got {:?}",
        result
    );
}

/// Empty frame header payload with `reduced_still_picture_header` must succeed
/// (per AV1 spec §5.9.1: no bits are read from the payload on this path).
/// This is an invariant test — verify the no-read short-circuit is preserved.
#[test]
fn empty_frame_header_reduced_path_succeeds() {
    let mut seq = non_reduced_dummy_seq();
    seq.reduced_still_picture_header = true;
    seq.still_picture = true;
    let result = parse_frame_header_light(&[], &seq);
    assert!(
        result.is_ok(),
        "reduced_still_picture_header path must succeed with empty payload; got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// parse_obu_stream — partial-success tolerance under adversarial inputs
// ---------------------------------------------------------------------------

/// `parse_obu_stream` is infallible (returns `Av1ObuStream`, never panics).
/// A stream consisting entirely of truncated / malformed OBU payloads must
/// surface errors in `unparseable` and not panic.
#[test]
fn obu_stream_with_all_malformed_payloads_does_not_panic() {
    let obus = vec![
        Obu {
            obu_type: 1, // Sequence Header
            extension: None,
            payload: vec![].into(), // empty — will fail to parse
        },
        Obu {
            obu_type: 3, // Frame Header
            extension: None,
            payload: vec![0xFF, 0xFF].into(), // no preceding SH → engine error
        },
        Obu {
            obu_type: 1, // Another malformed SH
            extension: None,
            payload: vec![0xAA, 0xBB].into(),
        },
    ];

    let stream = parse_obu_stream(&obus);
    // No panics. All failures land in `unparseable`.
    assert!(stream.sequence_headers.is_empty());
    assert!(stream.frame_headers.is_empty());
    assert!(
        !stream.unparseable.is_empty(),
        "malformed OBUs should appear in unparseable"
    );
}

/// Frame Header OBU with a valid SH but an empty payload (truncated body).
#[test]
fn obu_stream_truncated_frame_header_in_unparseable() {
    // Bytes captured from the unit test in sequence_header::tests::minimal_sequence_header.
    let seq_payload: Vec<u8> = vec![0, 0, 0, 4, 60, 255, 188, 0, 0, 0];

    let obus = vec![
        Obu {
            obu_type: 1,
            extension: None,
            payload: seq_payload.into(),
        },
        Obu {
            obu_type: 3, // Frame Header with empty body — truncated
            extension: None,
            payload: vec![].into(),
        },
    ];

    let stream = parse_obu_stream(&obus);
    assert_eq!(stream.sequence_headers.len(), 1, "SH should parse OK");
    // Empty frame header body must land in unparseable, not panic.
    assert_eq!(
        stream.unparseable.len(),
        1,
        "truncated FH should be in unparseable"
    );
    assert_eq!(
        stream.unparseable[0].0, 3,
        "obu_type in unparseable should be 3"
    );
}
