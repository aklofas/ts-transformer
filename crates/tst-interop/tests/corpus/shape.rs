use tst_interop::corpus::shape::{ShapeClass, classify, rules};
use tst_test_helpers::rawts::{ParsedStream, PmtStream, parsed};

#[test]
fn rules_parse_and_name_the_three_catalog_shapes_in_order() {
    let keys: Vec<&str> = rules().iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["b", "c", "a"]);
}

/// The `baseline` profile muxes H.264 (0x1B) plus async KLV (0x06) with the
/// KLVA registration. The audio/KLV roundtrip scenario is not used: it
/// carries synchronous KLV (0x15), which is not shape A.
#[test]
fn a_generated_h264_klv_stream_is_shape_a() {
    let bytes = crate::gen_profile_bytes("baseline", 2.0);
    assert_eq!(classify(&parsed(&bytes)), ShapeClass::A);
}

#[test]
fn a_stream_with_no_pmt_is_other() {
    assert_eq!(
        classify(&parsed(&[0x47, 0x1F, 0xFF, 0x10].repeat(47))),
        ShapeClass::Other
    );
}

fn hand_built(streams: &[(u8, bool)]) -> ParsedStream {
    ParsedStream {
        streams: streams
            .iter()
            .enumerate()
            .map(|(i, &(stream_type, klva))| PmtStream {
                pid: 0x100 + i as u16,
                stream_type,
                klva,
            })
            .collect(),
        ..ParsedStream::default()
    }
}

#[test]
fn rule_c_precedes_rule_a_when_a_stream_type_0x24_is_present() {
    let s = hand_built(&[(0x1B, false), (0x06, true), (0x24, false)]);
    assert_eq!(classify(&s), ShapeClass::C);
}

#[test]
fn a_stream_with_0x15_and_0xf0_is_shape_b() {
    let s = hand_built(&[(0x15, false), (0xF0, false)]);
    assert_eq!(classify(&s), ShapeClass::B);
}
