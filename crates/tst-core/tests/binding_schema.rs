//! `binding_record!` declares the struct unchanged and a schema that names
//! every field once, with the type name the generator maps.
use tst_core::binding_schema::{FieldMark, RecordSchema};
use tst_core::klv::st0601::WavelengthRecord;

#[test]
fn wavelength_record_schema_names_its_four_fields_in_order() {
    let s: RecordSchema = WavelengthRecord::BINDING_SCHEMA;
    assert_eq!(s.name, "WavelengthRecord");
    assert!(
        s.rust_path.ends_with("::WavelengthRecord"),
        "{}",
        s.rust_path
    );
    assert!(!s.non_exhaustive);
    let names: Vec<_> = s.fields.iter().map(|f| f.name).collect();
    assert_eq!(names, ["id", "min_nm", "max_nm", "name"]);
    let tys: Vec<_> = s.fields.iter().map(|f| f.ty).collect();
    assert_eq!(tys, ["u64", "f64", "f64", "String"]);
    assert!(s.fields.iter().all(|f| f.mark == FieldMark::Plain));
}

#[test]
fn wavelength_record_docs_are_joined_lines() {
    let s = WavelengthRecord::BINDING_SCHEMA;
    assert!(
        s.doc
            .starts_with(" One record of Item 128, Wavelengths List (ST 0601.19 §8.128) — a\n"),
        "{:?}",
        s.doc
    );
    assert!(s.doc.contains("\n sensor wavelength band definition."));
    let docs: Vec<_> = s.fields.iter().map(|f| f.doc).collect();
    assert_eq!(
        docs,
        [
            " BER-OID wavelength band id.",
            " Band minimum wavelength, nanometres.",
            " Band maximum wavelength, nanometres.",
            " Band name."
        ]
    );
}

#[test]
fn the_struct_is_unchanged() {
    let w = WavelengthRecord {
        id: 1,
        min_nm: 400.0,
        max_nm: 700.0,
        name: "vis".into(),
    };
    let c = w.clone();
    assert_eq!(w, c);
    assert!(format!("{w:?}").contains("min_nm"));
}

tst_core::binding_record! {
    /// A fixture record for the schema tests.
    #[derive(Debug, Clone, PartialEq, Default)]
    #[non_exhaustive]
    pub struct SchemaFixture {
        /// Plain field.
        pub a: u8,
        @[custom] pub b: Option<String>,
        @[skip] pub c: Vec<u8>,
        @[java("dCode")] pub d: Option<u16>,
        /// First line of two.
        /// Second line.
        pub e: u32,
        pub f: u8,
        @[custom]
        @[java("eCode")]
        pub e2: Option<u8>,
    }
}

#[test]
fn marks_and_java_override_are_reported() {
    let s = SchemaFixture::BINDING_SCHEMA;
    let by: std::collections::BTreeMap<_, _> = s.fields.iter().map(|f| (f.name, f)).collect();
    assert_eq!(by["b"].mark, FieldMark::Custom);
    assert!(by["b"].ty.contains("String"), "{}", by["b"].ty);
    assert_eq!(by["c"].mark, FieldMark::Skip);
    assert_eq!(by["d"].mark, FieldMark::Plain);
    assert_eq!(by["d"].java, Some("dCode"));
    assert_eq!(by["a"].java, None);
    assert_eq!(by["a"].doc, " Plain field.");
    assert_eq!(by["e"].doc, " First line of two.\n Second line.");
    assert_eq!(by["f"].doc, "");
    assert_eq!(by["e2"].mark, FieldMark::Custom);
    assert_eq!(by["e2"].java, Some("eCode"));
    assert_eq!(s.doc, " A fixture record for the schema tests.");
    assert!(s.non_exhaustive);
}
