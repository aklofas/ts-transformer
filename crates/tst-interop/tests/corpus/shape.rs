use tst_interop::corpus::shape::{ShapeClass, classify, rules};
use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_test_helpers::rawts::parsed;

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
    let path = std::env::temp_dir().join(format!("tst-corpus-shape-a-{}.ts", std::process::id()));
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    tst_interop::r#gen::run(p, 2.0, &path, KlvSet::Compact, 0, AuSizeMode::Compact).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(classify(&parsed(&bytes)), ShapeClass::A);
}

#[test]
fn a_stream_with_no_pmt_is_other() {
    assert_eq!(
        classify(&parsed(&[0x47, 0x1F, 0xFF, 0x10].repeat(47))),
        ShapeClass::Other
    );
}
