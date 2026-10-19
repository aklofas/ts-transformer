//! Emitter tests for the `print-records` binding generator: the committed
//! text it produces for the pilot record, the shapes it takes for records the
//! pilot does not cover, and the registry's completeness.

// The bin's modules are compiled whole; the tests reach only some of them.
#![allow(dead_code)]

#[path = "tools/print_records/emit_java.rs"]
mod emit_java;
#[path = "tools/print_records/emit_jvm_rs.rs"]
mod emit_jvm_rs;
#[path = "tools/print_records/emit_manifest.rs"]
mod emit_manifest;
#[path = "tools/print_records/emit_py.rs"]
mod emit_py;
#[path = "tools/print_records/emit_pyclass.rs"]
mod emit_pyclass;
#[path = "tools/print_records/emit_pyi.rs"]
mod emit_pyi;
#[path = "tools/print_records/schema.rs"]
mod schema;
#[path = "tools/print_records/stats.rs"]
mod stats;
#[path = "tools/print_records/typemap.rs"]
mod typemap;
#[path = "tools/print_records/write.rs"]
mod write;

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use schema::{Direction, Entry};
use tst_core::codec::ChromaFormat;
use tst_core::klv::st0601::WavelengthRecord;
use tst_core::klv::universal_label::UniversalLabel;
use tst_rtp::ClockAlignment;

tst_core::binding_record! {
    /// A record that holds a list of bands.
    #[derive(Debug, Clone, PartialEq)]
    pub struct ListFixture {
        /// Fixture id.
        pub id: u64,
        /// The bands.
        pub bands: Vec<WavelengthRecord>,
    }
}

tst_core::binding_record! {
    /// Optional fields on the constructor path.
    #[derive(Debug, Clone, PartialEq)]
    pub struct OptFixture {
        /// Required.
        pub id: u8,
        /// Optional text.
        pub name: Option<String>,
        /// Optional count.
        pub count: Option<u64>,
    }
}

tst_core::binding_record! {
    /// More than eight crossing fields, every shape of field.
    #[derive(Debug, Clone)]
    #[non_exhaustive]
    pub struct RichFixture {
        pub a: u8,
        pub b: i16,
        pub c: f32,
        pub d: bool,
        pub e: String,
        pub f: Vec<u8>,
        pub g: WavelengthRecord,
        pub h: Vec<WavelengthRecord>,
        @[skip] pub scratch: u8,
        @[java("typeCode")] pub kind: u8,
        @[custom] pub label: Option<UniversalLabel>,
        pub opt_text: Option<String>,
        pub opt_count: Option<u32>,
        pub opt_bytes: Option<Vec<u8>>,
        pub opt_band: Option<WavelengthRecord>,
    }
}

tst_core::binding_record! {
    /// Fields whose Python default is declared.
    #[derive(Debug, Clone, PartialEq)]
    pub struct DefaultFixture {
        pub id: u8,
        @[py_default("\"x\"")]
        pub name: String,
        @[py_default("5_000_000")]
        pub count: u64,
        pub note: Option<String>,
    }
}

tst_core::binding_record! {
    /// A small value held by a native class.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LeafFixture {
        /// The numerator.
        pub num: u32,
        /// The denominator.
        pub den: u32,
    }
}

tst_core::binding_record! {
    /// A parsed header held by a native class.
    ///
    /// Every shape of field a native class holds.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct NativeFixture {
        /// Plain scalar.
        pub id: u8,
        pub name: String,
        pub flag: bool,
        /// The original bytes.
        pub raw: Vec<u8>,
        pub header: [u8; 4],
        pub chroma: ChromaFormat,
        pub rate: Option<LeafFixture>,
        pub count: Option<u32>,
        pub leaves: Vec<LeafFixture>,
        pub by_id: BTreeMap<u8, LeafFixture>,
        @[custom] pub size: Option<(u32, u32)>,
        @[skip] pub scratch: u8,
        pub level: u8,
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Checks that generated Rust parses, where a working rustfmt is on `PATH`.
/// The snapshots are the emitters' unformatted text, so no test depends on
/// rustfmt or on its version; `binding-records.sh` checks the formatted
/// committed files where the pinned toolchain's rustfmt is installed.
fn parses(text: &str) {
    let have = std::process::Command::new("rustfmt")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if have {
        write::rustfmt(text, &root()).unwrap_or_else(|e| panic!("{e}\n{text}"));
    }
}

fn entry(schema: tst_core::binding_schema::RecordSchema, direction: Direction) -> Entry {
    Entry::new(
        "test",
        "tstrans.klv",
        "org/tstrans/klv",
        direction,
        schema.rust_path,
        "",
        schema,
    )
}

/// The pilot plus the fixtures, as one registry.
fn reg() -> Vec<Entry> {
    let mut r = schema::records();
    r.push(entry(ListFixture::BINDING_SCHEMA, Direction::Both));
    r.push(entry(OptFixture::BINDING_SCHEMA, Direction::Both));
    r.push(entry(RichFixture::BINDING_SCHEMA, Direction::Both));
    r
}

fn pilot(reg: &[Entry]) -> &Entry {
    schema::lookup(reg, "WavelengthRecord").expect("pilot registered")
}

fn named<'a>(reg: &'a [Entry], n: &str) -> &'a Entry {
    schema::lookup(reg, n).expect("registered")
}

#[test]
fn python_pair_matches_snapshot() {
    let reg = reg();
    let got = emit_py::record_pair(&reg, pilot(&reg)).unwrap();
    parses(&got);
    assert_eq!(
        got,
        include_str!("tools/print_records/snapshots/wavelength_record.py.rs")
    );
}

#[test]
fn jvm_pair_uses_a_canonical_constructor_and_a_sized_capacity() {
    let reg = reg();
    let raw = emit_jvm_rs::record_pair(&reg, pilot(&reg)).unwrap();
    assert_eq!(raw.matches("ensure_local_capacity(40)").count(), 2, "{raw}"); // 2 * 4 + 32, build and read
    assert!(raw.contains(r#""(JDDLjava/lang/String;)V""#), "{raw}");
    assert!(!raw.contains("from_raw"), "{raw}");
    parses(&raw);
    assert_eq!(
        raw,
        include_str!("tools/print_records/snapshots/wavelength_record.jvm.rs")
    );
}

#[test]
fn dataclass_stub_and_java_record_match_snapshots() {
    let reg = reg();
    let e = pilot(&reg);
    assert_eq!(
        emit_pyi::py_block(&reg, e).unwrap(),
        include_str!("tools/print_records/snapshots/wavelength_record.py")
    );
    assert_eq!(
        emit_pyi::pyi_block(&reg, e).unwrap(),
        include_str!("tools/print_records/snapshots/wavelength_record.pyi")
    );
    assert_eq!(
        emit_java::block(&reg, e).unwrap(),
        include_str!("tools/print_records/snapshots/wavelength_record.java")
    );
}

#[test]
fn capacity_follows_two_per_field_plus_headroom() {
    assert_eq!(emit_jvm_rs::capacity(4), 40);
    assert_eq!(emit_jvm_rs::capacity(0), 32);
    // The hand-written ST 0601 builder reserves 320 for 147 fields.
    assert!(
        emit_jvm_rs::capacity(147) >= 320,
        "{}",
        emit_jvm_rs::capacity(147)
    );
}

#[test]
fn a_vec_of_records_is_built_and_read_inside_local_frames() {
    let reg = reg();
    let raw = emit_jvm_rs::record_pair(&reg, named(&reg, "ListFixture")).unwrap();
    assert!(
        raw.contains("with_local_frame_returning_local::<_, jni::errors::Error>(16"),
        "{raw}"
    );
    assert!(
        raw.contains("with_local_frame::<_, (), jni::errors::Error>(16"),
        "{raw}"
    );
    assert!(
        raw.contains("env.with_local_frame(16, |env| -> jni::errors::Result<_>"),
        "{raw}"
    );
    assert!(raw.contains("\"(JLjava/util/List;)V\""), "{raw}");
    parses(&raw);
    // The item is built inside its own frame, inside the list's frame.
    let want = concat!(
        "for item in r.bands.iter() { ",
        "env.with_local_frame::<_, (), jni::errors::Error>(16, |env| { ",
        "let o1 = env.with_local_frame_returning_local::<_, jni::errors::Error>(16, |env| ",
        "crate::generated::st0601::build_wavelength_record(env, item))?;",
    );
    assert!(raw.contains(want), "{raw}");
}

#[test]
fn optional_fields_cross_as_boxed_values_and_null() {
    let reg = reg();
    let raw = emit_jvm_rs::record_pair(&reg, named(&reg, "OptFixture")).unwrap();
    assert!(
        raw.contains("\"(ILjava/lang/String;Ljava/lang/Long;)V\""),
        "{raw}"
    );
    assert!(raw.contains("None => JObject::null()"), "{raw}");
    assert!(raw.contains("checked_u8"), "{raw}");
    let py = emit_py::record_pair(&reg, named(&reg, "OptFixture")).unwrap();
    assert!(py.contains("if let Some(v) = &r.name"), "{py}");
    parses(&raw);
    parses(&py);
    let block = emit_pyi::py_block(&reg, named(&reg, "OptFixture")).unwrap();
    assert!(block.contains("    name: str | None = None\n"), "{block}");
    let stub = emit_pyi::pyi_block(&reg, named(&reg, "OptFixture")).unwrap();
    assert!(stub.contains("    count: Optional[int] = ...\n"), "{stub}");
}

#[test]
fn a_wide_record_is_built_through_its_builder() {
    let reg = reg();
    let e = named(&reg, "RichFixture");
    let raw = emit_jvm_rs::record_pair(&reg, e).unwrap();
    assert!(raw.contains("org/tstrans/klv/RichFixture$Builder"), "{raw}");
    assert!(raw.contains("\"typeCode\""), "{raw}");
    assert!(
        raw.contains("crate::custom::build_rich_fixture_label"),
        "{raw}"
    );
    assert!(
        !raw.contains("scratch"),
        "a skipped field never crosses\n{raw}"
    );
    parses(&raw);
    // An absent optional leaves its setter uncalled.
    let want = concat!(
        "    if let Some(v) = &r.opt_text { ",
        "let o2 = env.new_string(v)?; ",
        "env.call_method(&b, \"optText\", ",
    );
    assert!(raw.contains(want), "{raw}");
    let (build, read) = raw
        .split_once("pub(crate) fn read_")
        .expect("both functions");
    assert!(!build.contains("JObject::null()"), "{build}");
    // A non_exhaustive record is read by default-and-assign.
    assert!(
        read.contains("let mut r = print_records_emit::RichFixture::default();\n    r.a = a;\n"),
        "{read}"
    );
    let java = emit_java::block(&reg, e).unwrap();
    assert!(java.contains("public static final class Builder"), "{java}");
    assert!(java.contains("int typeCode"), "{java}");
    let py = emit_py::record_pair(&reg, e).unwrap();
    assert!(
        py.contains("crate::custom::rich_fixture_label_to_py"),
        "{py}"
    );
    assert!(
        py.contains("let mut r = "),
        "a non_exhaustive record is default-and-assign\n{py}"
    );
    parses(&py);
}

#[test]
fn a_one_way_record_emits_only_its_direction() {
    let reg = reg();
    let to = entry(WavelengthRecord::BINDING_SCHEMA, Direction::ToHost);
    let from = entry(WavelengthRecord::BINDING_SCHEMA, Direction::FromHost);
    let j = emit_jvm_rs::record_pair(&reg, &to).unwrap();
    assert!(j.contains("fn build_") && !j.contains("fn read_"), "{j}");
    let j = emit_jvm_rs::record_pair(&reg, &from).unwrap();
    assert!(j.contains("fn read_") && !j.contains("fn build_"), "{j}");
    let p = emit_py::record_pair(&reg, &to).unwrap();
    assert!(p.contains("_to_py") && !p.contains("_from_py"), "{p}");
    let p = emit_py::record_pair(&reg, &from).unwrap();
    assert!(p.contains("_from_py") && !p.contains("_to_py"), "{p}");
    // A read-only file needs neither the dict nor the builder imports.
    let f = emit_py::file(
        &[entry(WavelengthRecord::BINDING_SCHEMA, Direction::FromHost)],
        "test",
    )
    .unwrap();
    assert!(!f.contains("PyDict"), "{f}");
}

#[test]
fn an_unmapped_type_is_an_error_naming_it() {
    let err = typemap::map("tst_core::transport::SocketStats", &[]).unwrap_err();
    assert!(err.contains("SocketStats"), "{err}");
    let err = typemap::map("Option<Option<u8>>", &[]).unwrap_err();
    assert!(err.contains("nested Option"), "{err}");
    // Each item of a list crosses as a value; a list of byte strings would
    // reach Python as lists of ints.
    let err = typemap::map("Vec<Vec<u8>>", &[]).unwrap_err();
    assert!(err.contains("Vec<Vec<u8>>"), "{err}");
    // Spellings of the same type the macro can produce.
    for ok in [
        "std::string::String",
        "alloc::string::String",
        "std :: vec :: Vec < u8 >",
        "Option<u64>",
    ] {
        typemap::map(ok, &[]).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn an_unmapped_field_names_its_record_and_field() {
    tst_core::binding_record! {
        pub struct Bad {
            pub stats: std::time::Instant,
        }
    }
    let reg = vec![entry(Bad::BINDING_SCHEMA, Direction::Both)];
    let err = emit_py::record_pair(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("Bad.stats") && err.contains("Instant"),
        "{err}"
    );
}

#[test]
fn a_required_field_after_an_optional_one_takes_a_host_default() {
    tst_core::binding_record! {
        pub struct Misordered {
            pub a: Option<u8>,
            pub b: u8,
            pub c: f64,
            pub d: bool,
            pub e: String,
            pub f: Vec<u8>,
            pub g: Vec<u64>,
            pub h: u8,
            pub i: u8,
        }
    }
    let reg = vec![entry(Misordered::BINDING_SCHEMA, Direction::Both)];
    let py = emit_pyi::py_block(&reg, &reg[0]).unwrap();
    for want in [
        "    a: int | None = None\n",
        "    b: int = 0\n",
        "    c: float = 0.0\n",
        "    d: bool = False\n",
        "    e: str = \"\"\n",
        "    f: bytes = b\"\"\n",
        "    g: tuple[int, ...] = ()\n",
    ] {
        assert!(py.contains(want), "{want}\n{py}");
    }
    let pyi = emit_pyi::pyi_block(&reg, &reg[0]).unwrap();
    assert!(pyi.contains("    b: int = ...\n"), "{pyi}");
    // The Builder starts the non-primitive ones at their host default.
    let java = emit_java::block(&reg, &reg[0]).unwrap();
    for want in [
        "private String e = \"\";",
        "private ByteBuffer f = java.nio.ByteBuffer.allocate(0);",
        "private List<Long> g = java.util.Collections.emptyList();",
        "private int b;",
    ] {
        assert!(java.contains(want), "{want}\n{java}");
    }
    // A required record has no host default to give.
    tst_core::binding_record! {
        pub struct NoDefault {
            pub a: Option<u8>,
            pub band: WavelengthRecord,
        }
    }
    let reg = vec![
        entry(NoDefault::BINDING_SCHEMA, Direction::Both),
        entry(WavelengthRecord::BINDING_SCHEMA, Direction::Both),
    ];
    let err = emit_pyi::py_block(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("NoDefault.band") && err.contains("no host default"),
        "{err}"
    );
}

#[test]
fn doc_text_cannot_close_a_comment_or_a_docstring() {
    let doc = "Ends */ here, has a \\u0041 escape, a \"\"\" and a back\\slash \"";
    let java = emit_java::escape_javadoc(doc, &[]);
    assert!(!java.contains("*/") && !java.contains("\\u"), "{java}");
    assert_eq!(
        emit_java::escape_javadoc("a `min_nm` b", &[]),
        "a {@code min_nm} b"
    );
    let py = emit_pyi::escape_docstring(doc);
    assert!(!py.contains("\"\"\""), "{py}");
    assert!(py.contains("back\\\\slash"), "{py}");
    assert!(py.ends_with("\\\""), "a trailing quote is escaped: {py}");
}

#[test]
fn javadoc_prose_is_html_safe_and_names_java_fields() {
    let rename = [("min_nm", "minNm")];
    assert_eq!(
        emit_java::escape_javadoc("a < b & c > d", &rename),
        "a &lt; b &amp; c &gt; d"
    );
    // No word can start a block tag or an inline tag.
    assert_eq!(
        emit_java::escape_javadoc("@param x {@link y}", &rename),
        "&#64;param x {&#64;link y}"
    );
    // Code spans keep their text; a field name takes its Java spelling.
    assert_eq!(
        emit_java::escape_javadoc("`min_nm` holds `Option<T>`", &rename),
        "{@code minNm} holds {@code Option<T>}"
    );
    // An intra-doc link loses its brackets.
    assert_eq!(
        emit_java::escape_javadoc("see [`Foo`] and [x]", &rename),
        "see {@code Foo} and [x]"
    );
}

#[test]
fn docstrings_use_rst_code_and_wrap_at_79_columns() {
    assert_eq!(emit_pyi::rst_code("a `min_nm` b"), "a ``min_nm`` b");
    assert_eq!(emit_pyi::rst_code("odd ` one"), "odd ` one");
    let one = |s: &str| emit_pyi::docstring_paras(&[s.to_string()]);
    assert_eq!(one("Short."), "    \"\"\"Short.\"\"\"\n");
    let long = "word ".repeat(40);
    let doc = one(long.trim());
    let lines: Vec<&str> = doc.lines().collect();
    assert!(lines.len() > 1, "{doc}");
    assert!(lines.iter().all(|l| l.chars().count() <= 79), "{doc}");
    assert!(lines[0].starts_with("    \"\"\"word"), "{doc}");
    assert!(
        lines[1..].iter().all(|l| l.starts_with("    word")),
        "{doc}"
    );
    assert!(doc.ends_with("word\"\"\"\n"), "{doc}");
    // The pilot's docstring is wrapped too.
    let reg = reg();
    let block = emit_pyi::py_block(&reg, pilot(&reg)).unwrap();
    assert!(block.lines().all(|l| l.chars().count() <= 79), "{block}");
    assert!(block.contains("``min_nm``/``max_nm``"), "{block}");
}

#[test]
fn documented_fields_are_bullets_that_wrap_under_their_text() {
    let long = "word ".repeat(30);
    let doc = emit_pyi::docstring_paras(&[
        "Summary.".to_string(),
        "- ``a`` \u{2014} first.".to_string(),
        format!("- ``b`` \u{2014} {}", long.trim()),
    ]);
    let lines: Vec<&str> = doc.lines().collect();
    assert_eq!(lines[0], "    \"\"\"Summary.");
    assert_eq!(lines[1], "");
    // Bullets sit on adjacent lines; a wrapped one continues two columns in.
    assert_eq!(lines[2], "    - ``a`` \u{2014} first.");
    assert!(lines[3].starts_with("    - ``b`` "), "{doc}");
    assert!(lines[4].starts_with("      word"), "{doc}");
    assert!(lines.iter().all(|l| l.chars().count() <= 79), "{doc}");
}

#[test]
fn paragraphs_split_at_blank_lines() {
    assert_eq!(
        schema::paras(" First line\n second line.\n\n Next paragraph."),
        vec!["First line second line.", "Next paragraph."]
    );
    assert_eq!(schema::paras(" Only one."), vec!["Only one."]);
    assert!(schema::paras("").is_empty());
    // A line of spaces is blank too, and runs of blank lines are one break.
    assert_eq!(schema::paras(" a\n   \n\n b"), vec!["a", "b"]);
}

#[test]
fn a_java_keyword_field_needs_an_override() {
    tst_core::binding_record! {
        pub struct Kw {
            pub class: u8,
        }
    }
    let reg = vec![entry(Kw::BINDING_SCHEMA, Direction::Both)];
    let err = emit_java::block(&reg, &reg[0]).unwrap_err();
    assert!(err.contains("keyword"), "{err}");
}

#[test]
fn naming_helpers() {
    assert_eq!(schema::snake("WavelengthRecord"), "wavelength_record");
    assert_eq!(schema::snake("UasDatalinkLs"), "uas_datalink_ls");
    assert_eq!(schema::camel("min_nm"), "minNm");
    assert_eq!(
        schema::records()[0].rust_path,
        "tst_core::klv::st0601::WavelengthRecord"
    );
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Names of the structs declared through the macro in the library sources.
fn declared_records() -> Vec<String> {
    let mut files = Vec::new();
    for group in ["crates", "bindings"] {
        let Ok(rd) = std::fs::read_dir(root().join(group)) else {
            continue;
        };
        for krate in rd.flatten() {
            walk(&krate.path().join("src"), &mut files);
            // Nested crates such as bindings/c/core.
            if let Ok(inner) = std::fs::read_dir(krate.path()) {
                for i in inner.flatten() {
                    if i.path().join("Cargo.toml").exists() {
                        walk(&i.path().join("src"), &mut files);
                    }
                }
            }
        }
    }
    let mut names = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            if t.starts_with("//") || !t.contains("binding_record!") || !t.trim_end().ends_with('{')
            {
                continue;
            }
            let name = lines[i..]
                .iter()
                .filter(|l| !l.trim_start().starts_with("//"))
                .find_map(|l| l.split("struct ").nth(1))
                .and_then(|r| r.split(|c: char| !c.is_alphanumeric() && c != '_').next())
                .unwrap_or_else(|| panic!("{}: binding_record! without a struct", f.display()));
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

#[test]
fn every_declared_record_is_registered() {
    let declared = declared_records();
    assert!(
        declared.contains(&"WavelengthRecord".to_string()),
        "the walk found nothing: {declared:?}"
    );
    assert!(
        !declared.contains(&"ListFixture".to_string()),
        "test fixtures live under tests/"
    );
    let mut registered: Vec<String> = schema::records()
        .iter()
        .map(|e| e.schema.name.to_string())
        .collect();
    registered.sort();
    assert_eq!(
        declared, registered,
        "a binding_record! declaration is missing from print-records' registry"
    );
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("print-records-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn check_lists_absent_targets_and_write_fills_them() {
    let reg = vec![schema::records().remove(0)];
    let d = scratch("check");
    let py_dir = d.join("bindings/python/python/tstrans");
    std::fs::create_dir_all(&py_dir).unwrap();
    let marked = "x = 1\n# BEGIN GENERATED by print-records (WavelengthRecord); do not edit\nold\n\
                  # END GENERATED by print-records (WavelengthRecord)\ny = 2\n";
    std::fs::write(py_dir.join("klv.py"), marked).unwrap();
    std::fs::write(py_dir.join("klv.pyi"), "no markers here\n").unwrap();

    let t = write::targets(&d, &reg).unwrap();
    let stale: Vec<String> = write::stale(&t)
        .into_iter()
        .map(|(p, why)| format!("{} {why}", p.display()))
        .collect();
    assert_eq!(stale.len(), 5, "{stale:#?}");
    assert!(
        stale
            .iter()
            .any(|s| s.contains("st0601.rs") && s.ends_with("is absent")),
        "{stale:#?}"
    );
    assert!(
        stale
            .iter()
            .any(|s| s.contains("klv.pyi") && s.contains("has no line")),
        "{stale:#?}"
    );
    assert!(
        stale
            .iter()
            .any(|s| s.contains("klv.py ") && s.ends_with("differs from the schema")),
        "{stale:#?}"
    );

    // A block target without its markers cannot be written.
    assert!(write::write_all(&t).is_err());
    std::fs::write(
        py_dir.join("klv.pyi"),
        "# BEGIN GENERATED by print-records (WavelengthRecord); do not edit\n# END GENERATED by print-records (WavelengthRecord)\n",
    )
    .unwrap();
    // The Java record is a block of a hand-owned file; its tail closes the body.
    let java_dir = d.join("bindings/jvm/src/main/java/org/tstrans/klv");
    std::fs::create_dir_all(&java_dir).unwrap();
    std::fs::write(
        java_dir.join("WavelengthRecord.java"),
        "package org.tstrans.klv;\n\n// BEGIN GENERATED by print-records (WavelengthRecord); do not edit\n\
         old\n// END GENERATED by print-records (WavelengthRecord)\n}\n",
    )
    .unwrap();
    write::write_all(&write::targets(&d, &reg).unwrap()).unwrap();
    assert_eq!(write::stale(&write::targets(&d, &reg).unwrap()), Vec::new());
    let java = std::fs::read_to_string(java_dir.join("WavelengthRecord.java")).unwrap();
    assert!(
        java.starts_with("package org.tstrans.klv;\n\n// BEGIN")
            && java.contains("public record WavelengthRecord(")
            && java.ends_with("// END GENERATED by print-records (WavelengthRecord)\n}\n"),
        "{java}"
    );
    let py = std::fs::read_to_string(py_dir.join("klv.py")).unwrap();
    assert!(
        py.starts_with("x = 1\n# BEGIN") && py.ends_with("y = 2\n") && !py.contains("old\n"),
        "{py}"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn check_lists_generated_text_no_record_renders() {
    let reg = schema::records();
    let d = scratch("orphans");
    let header = "// GENERATED by print-records from binding_record! declarations; do not edit.\n";
    let java = d.join("bindings/jvm/src/main/java/org/tstrans/klv");
    std::fs::create_dir_all(&java).unwrap();
    std::fs::write(
        java.join("Gone.java"),
        "// BEGIN GENERATED by print-records (Gone); do not edit\n\
         // END GENERATED by print-records (Gone)\n",
    )
    .unwrap();
    std::fs::write(java.join("Hand.java"), "package x;\n").unwrap();
    let gen_dir = d.join("bindings/python/src/generated");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("old_family.rs"), header).unwrap();
    std::fs::write(gen_dir.join("mod.rs"), "//! hand\n").unwrap();
    let py = d.join("bindings/python/python/tstrans");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::write(
        py.join("klv.py"),
        "# BEGIN GENERATED by print-records (WavelengthRecord); do not edit\n\
         # END GENERATED by print-records (WavelengthRecord)\n\
         # BEGIN GENERATED by print-records (Gone); do not edit\n\
         # END GENERATED by print-records (Gone)\n",
    )
    .unwrap();

    let t = write::targets(&d, &reg).unwrap();
    let found: Vec<String> = write::orphans(&d, &t)
        .into_iter()
        .map(|(p, why)| format!("{} {why}", p.display()))
        .collect();
    assert_eq!(found.len(), 3, "{found:#?}");
    assert!(
        found
            .iter()
            .any(|s| s.contains("Gone.java") && s.contains("(Gone)")),
        "{found:#?}"
    );
    assert!(
        found.iter().any(|s| s.contains("old_family.rs")),
        "{found:#?}"
    );
    assert!(
        found
            .iter()
            .any(|s| s.contains("klv.py") && s.contains("(Gone)")),
        "{found:#?}"
    );
    // The committed tree has none.
    let t = write::targets(&root(), &reg).unwrap();
    assert_eq!(write::orphans(&root(), &t), Vec::new());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_list_of_optionals_is_refused_naming_record_and_field() {
    tst_core::binding_record! {
        pub struct Holes {
            pub marks: Vec<Option<u8>>,
        }
    }
    let reg = vec![entry(Holes::BINDING_SCHEMA, Direction::Both)];
    let err = emit_py::record_pair(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("Holes.marks") && err.contains("Vec<Option<T>>"),
        "{err}"
    );
}

#[test]
fn the_command_line_refuses_a_missing_root_and_an_unknown_flag() {
    let bin = env!("CARGO_BIN_EXE_print-records");
    for args in [&["--root"][..], &["--wirte"][..]] {
        let out = std::process::Command::new(bin).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("usage: print-records"), "{args:?}: {err}");
    }
    let out = std::process::Command::new(bin)
        .args(["--root", root().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("st0601 WavelengthRecord\n"), "{stdout}");
    let n = write::targets(&root(), &schema::records()).unwrap().len();
    assert!(
        stdout.ends_with(&format!(
            "{} records → {n} targets\n",
            schema::records().len()
        )),
        "{stdout}"
    );
}

// ---------------------------------------------------------------------------
// The ST 0601 family: every registered record renders, and the big record's
// declaration is pinned.
// ---------------------------------------------------------------------------

use tst_core::binding_schema::{FieldMark, FieldSpec};

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The code spans of a rendered doc: `{@code …}` in Java, ``…`` in Python.
fn code_spans(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (open, close) in [("{@code ", "}"), ("``", "``")] {
        let mut rest = text;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + open.len()..];
            let Some(j) = after.find(close) else { break };
            out.push(after[..j].to_string());
            rest = &after[j + close.len()..];
        }
    }
    out
}

/// Why a token inside a code span is a Rust name a host reader cannot use:
/// a primitive type, a SCREAMING_CASE constant, or the name of a Rust
/// function. The record's own field names are the host's too.
fn rust_name(tok: &str, fields: &[&str]) -> Option<&'static str> {
    const PRIMITIVES: [&str; 11] = [
        "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64", "usize",
    ];
    const FN_PREFIXES: [&str; 8] = [
        "decode_", "encode_", "parse_", "from_", "to_", "into_", "as_", "try_",
    ];
    if fields.contains(&tok) {
        return None;
    }
    if PRIMITIVES.contains(&tok) {
        return Some("is a Rust type");
    }
    let screaming = tok.contains('_')
        && tok.chars().any(|c| c.is_ascii_uppercase())
        && tok
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if screaming {
        return Some("is a Rust constant");
    }
    if FN_PREFIXES.iter().any(|p| tok.starts_with(p))
        || tok.ends_with("_strict")
        || tok.ends_with("_to_vec")
    {
        return Some("names a Rust function");
    }
    None
}

/// The `*` characters of `text` that are not part of a `**` pair or of a
/// comment delimiter: two on one line read as `*emphasis*`.
fn lone_asterisks(text: &str) -> usize {
    let cs: Vec<char> = text.chars().collect();
    (0..cs.len())
        .filter(|&i| {
            cs[i] == '*'
                && (i == 0 || cs[i - 1] != '*')
                && cs.get(i + 1) != Some(&'*')
                && cs.get(i + 1) != Some(&'/')
        })
        .count()
}

#[test]
fn rust_names_in_a_code_span_are_recognised() {
    let fields = ["decode_mode"];
    assert!(rust_name("u64", &fields).is_some());
    assert!(rust_name("usize", &fields).is_some());
    assert!(rust_name("PACK_TAGS", &fields).is_some());
    assert!(rust_name("decode_strict", &fields).is_some());
    assert!(rust_name("encode_to_vec", &fields).is_some());
    // A field of the record is the host's name too; a spec syntax element
    // and an upper-case word with no underscore are prose.
    assert!(rust_name("decode_mode", &fields).is_none());
    assert!(rust_name("frame_mbs_only_flag", &fields).is_none());
    assert!(rust_name("CAVLC", &fields).is_none());
    assert!(rust_name("matrixSize", &fields).is_none());
    assert_eq!(lone_asterisks("the *used* percentage"), 2);
    assert_eq!(lone_asterisks("a **strong** word, then */"), 0);
}

#[test]
fn no_rust_only_text_reaches_a_host_doc() {
    // Neither a path nor a Rust expression, in prose or in a code span.
    let rust_only = [
        "crate::",
        "::",
        "()?",
        "as_deref",
        "f64::",
        "Vec<",
        "Vec&lt;",
        "Option<",
        "Option&lt;",
        "this crate",
    ];
    let reg = schema::records();
    let mut leaks = Vec::new();
    for e in &reg {
        let n = e.schema.name;
        let java = emit_java::block(&reg, e).unwrap();
        // The Javadoc is the text above the record header; the docstring is
        // the text between the first two triple quotes, or for a native
        // class the doc comments that become its `__doc__`.
        let javadoc: String = java
            .lines()
            .take_while(|l| !l.starts_with("public record"))
            .collect::<Vec<_>>()
            .join("\n");
        let docstring = if e.is_native() {
            emit_pyclass::docs(&reg, e).unwrap()
        } else {
            let py = emit_pyi::py_block(&reg, e).unwrap();
            py.split("\"\"\"").nth(1).unwrap_or("").to_string()
        };
        for (host, doc) in [("Java", &javadoc), ("Python", &docstring)] {
            for bad in rust_only {
                if doc.contains(bad) {
                    let at = doc.find(bad).unwrap();
                    let from = doc[..at].char_indices().rev().nth(30).map_or(0, |(i, _)| i);
                    let to = (at + bad.len() + 30).min(doc.len());
                    let to = (to..=doc.len())
                        .find(|i| doc.is_char_boundary(*i))
                        .unwrap_or(doc.len());
                    leaks.push(format!(
                        "{n} ({host}) `{bad}`: ...{}...",
                        doc[from..to].replace('\n', " ")
                    ));
                }
            }
            let fields: Vec<&str> = e.schema.fields.iter().map(|f| f.name).collect();
            for span in code_spans(doc) {
                for bad in rust_only {
                    if span.contains(bad) {
                        leaks.push(format!("{n} ({host}) code span `{span}` carries `{bad}`"));
                    }
                }
                for tok in span.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                    if let Some(why) = rust_name(tok, &fields) {
                        leaks.push(format!("{n} ({host}) code span `{span}`: `{tok}` {why}"));
                    }
                }
            }
            // Java has no markdown: `*name*` shows its asterisks.
            if host == "Java" {
                for line in doc.lines() {
                    // The comment's own ` * ` margin is not prose.
                    let body = line.trim_start().strip_prefix('*').unwrap_or(line);
                    if lone_asterisks(body) >= 2 {
                        leaks.push(format!("{n} (Java) markdown emphasis: {}", body.trim()));
                    }
                }
            }
        }
    }
    leaks.sort();
    leaks.dedup();
    assert!(
        leaks.is_empty(),
        "Rust-only text in host docs:\n{}",
        leaks.join("\n")
    );
}

#[test]
fn a_doc_ends_for_the_hosts_at_the_rust_only_line() {
    assert_eq!(
        schema::paras(
            " Host text.\n\n More host text.\n <!-- rust-only -->\n Owned variant of [`X`].\n ```\n code\n ```"
        ),
        vec!["Host text.", "More host text."]
    );
    assert!(schema::paras(" <!-- rust-only -->\n only for Rust").is_empty());
}

#[test]
fn rust_paths_and_link_definitions_become_plain_names_in_host_docs() {
    assert_eq!(
        schema::unlink("call `klv::st0806::decode` or `Self::field()` then [`Name`]"),
        "call `decode` or `field` then `Name`"
    );
    assert_eq!(
        schema::unlink("keeps `Option<T>` and `a b::c`"),
        "keeps `Option<T>` and `a b::c`"
    );
    // A reference-style link definition says nothing to a host reader.
    assert_eq!(
        schema::paras(
            " See [`decode`].\n\n [`decode`]: crate::klv::st0903::decode\n [`x`]: crate::x"
        ),
        vec!["See [`decode`]."]
    );
}

#[test]
fn every_registered_record_renders_every_target() {
    let reg = schema::records();
    assert_eq!(reg.len(), 69);
    for e in &reg {
        let n = e.schema.name;
        let py = emit_py::record_pair(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        let jvm = emit_jvm_rs::record_pair(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        parses(&py);
        parses(&jvm);
        if e.is_native() {
            emit_pyclass::pyi_block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        } else {
            emit_pyi::py_block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
            emit_pyi::pyi_block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        }
        let java = emit_java::block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        let host = e.name();
        assert!(
            java.contains(&format!("public record {host}(")),
            "{n}: {java}"
        );
        // The reader reserves the same local references as the builder.
        let ways = usize::from(e.direction.crosses_out()) + usize::from(e.direction.crosses_in());
        assert_eq!(
            jvm.matches("ensure_local_capacity(").count(),
            ways,
            "{n}: {jvm}"
        );
    }
    let targets = write::targets(&root(), &reg).unwrap();
    let mut families: Vec<&str> = reg.iter().map(|e| e.family).collect();
    families.dedup();
    let jvm_families = families
        .iter()
        .filter(|f| {
            reg.iter()
                .any(|e| e.family == **f && e.jvm && e.jvm_builder)
        })
        .count();
    let py_families = families
        .iter()
        .filter(|f| reg.iter().any(|e| e.family == **f && e.python))
        .count();
    let jvm_records = reg.iter().filter(|e| e.jvm).count();
    // One Python converter file per family that has a Python record, one JVM
    // file per family that has a JVM record, the `.py` and `.pyi` blocks per
    // dataclass (the `.pyi` block only for a native class), a Java block
    // per JVM record and the surface manifest's cells.
    let py_blocks: usize = reg
        .iter()
        .filter(|e| e.python)
        .map(|e| if e.is_native() { 1 } else { 2 })
        .sum();
    assert_eq!(
        targets.len(),
        py_families + jvm_families + py_blocks + jvm_records + 1
    );
}

#[test]
fn the_big_record_is_declared_in_struct_order_with_its_residue_marked() {
    let reg = schema::records();
    let e = named(&reg, "UasDatalinkLs");
    let names: Vec<&str> = e.schema.fields.iter().map(|f| f.name).collect();
    assert_eq!(names.len(), 147);
    assert_eq!(
        &names[..5],
        &[
            "universal_label",
            "declared_version",
            "mission_id",
            "platform_tail_number",
            "platform_designation"
        ]
    );
    assert_eq!(
        &names[142..],
        &[
            "sdcc_flps",
            "unknown",
            "field_errors",
            "sentinel_tags",
            "imapb_specials"
        ]
    );
    // The declaration order is the order of the struct in the source.
    // Line endings are normalised: a checkout with CRLF endings would otherwise
    // never match the brace-and-newline pattern below.
    let src = std::fs::read_to_string(root().join("crates/tst-core/src/klv/st0601/model.rs"))
        .unwrap()
        .replace("\r\n", "\n");
    let start = src.find("pub struct UasDatalinkLs {").unwrap();
    let body = &src[start..start + src[start..].find("\n    }\n").unwrap()];
    let declared: Vec<&str> = body
        .lines()
        .filter_map(|l| l.split("pub ").nth(1))
        .filter_map(|r| r.split_once(": ").map(|(n, _)| n))
        .collect();
    assert_eq!(names, declared);

    let custom: Vec<&str> = e
        .schema
        .fields
        .iter()
        .filter(|f| f.mark == FieldMark::Custom)
        .map(|f| f.name)
        .collect();
    assert_eq!(
        custom,
        [
            "universal_label",
            "miis_core_id",
            "platform_status",
            "sensor_control_mode",
            "icing_detected",
            "sensor_fov_name",
            "operational_mode",
            "unknown",
            "field_errors",
            "sentinel_tags",
            "imapb_specials"
        ]
    );
    // The five coded enums carry the `…Code` host name.
    let coded: Vec<_> = e
        .schema
        .fields
        .iter()
        .filter_map(|f| f.java.map(|j| (f.name, j)))
        .collect();
    assert_eq!(
        coded,
        [
            ("platform_status", "platformStatusCode"),
            ("sensor_control_mode", "sensorControlModeCode"),
            ("icing_detected", "icingDetectedCode"),
            ("sensor_fov_name", "sensorFovNameCode"),
            ("operational_mode", "operationalModeCode")
        ]
    );

    // The Java components and the dataclass fields follow the same order.
    let java = emit_java::block(&reg, e).unwrap();
    let header = java.split(") implements KlvSet {").next().unwrap();
    let comps: Vec<&str> = header
        .lines()
        .skip_while(|l| !l.starts_with("public record UasDatalinkLs("))
        .skip(1)
        .map(|l| l.trim().trim_end_matches(',').rsplit(' ').next().unwrap())
        .collect();
    let java_names: Vec<String> = names
        .iter()
        .map(|n| match coded.iter().find(|(r, _)| r == n) {
            Some((_, j)) => j.to_string(),
            None => schema::camel(n),
        })
        .collect();
    assert_eq!(comps, java_names);
    let py = emit_pyi::py_block(&reg, e).unwrap();
    let fields: Vec<&str> = py
        .lines()
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| l.trim().split_once(": ").map(|(n, _)| n))
        .filter(|n| {
            n.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        })
        .collect();
    assert_eq!(fields, names);
}

#[test]
fn the_big_record_reserves_at_least_the_capacity_the_hand_written_one_did() {
    let reg = schema::records();
    let jvm = emit_jvm_rs::record_pair(&reg, named(&reg, "UasDatalinkLs")).unwrap();
    assert!(jvm.contains("ensure_local_capacity(326)"), "2 * 147 + 32");
    assert!(emit_jvm_rs::capacity(147) >= 320);
}

#[test]
fn a_primitive_custom_field_is_passed_to_the_constructor_as_its_primitive() {
    let reg = schema::records();
    let jvm = emit_jvm_rs::record_pair(&reg, named(&reg, "PayloadRecord")).unwrap();
    assert!(
        jvm.contains("crate::custom::build_payload_record_payload_type(env, &r.payload_type)?"),
        "{jvm}"
    );
    assert!(jvm.contains("JValue::Long(payload_type)"), "{jvm}");
    assert!(jvm.contains(r#""(JJLjava/lang/String;)V""#), "{jvm}");
    let java = emit_java::block(&reg, named(&reg, "PayloadRecord")).unwrap();
    assert!(java.contains("long payloadTypeCode"), "{java}");
}

tst_core::binding_record! {
    /// An optional primitive custom field.
    #[derive(Debug, Clone)]
    pub struct PrimitiveOptional {
        @[custom] pub kind: Option<tst_core::klv::st0601::PayloadType>,
    }
}

#[test]
fn an_optional_primitive_custom_field_is_refused() {
    let reg = vec![entry(PrimitiveOptional::BINDING_SCHEMA, Direction::Both)];
    let err = emit_java::block(&reg, &reg[0]).unwrap_err();
    assert!(err.contains("PrimitiveOptional.kind"), "{err}");
    assert!(err.contains("cannot be optional"), "{err}");
}

#[test]
fn a_self_path_in_a_doc_names_the_java_field() {
    let rename = [("min_nm", "minNm")];
    assert_eq!(
        emit_java::escape_javadoc("use `Self::min_nm` or `min_nm`", &rename),
        "use {@code minNm} or {@code minNm}"
    );
}

// ---------------------------------------------------------------------------
// Readiness for the ST 0601 record: public paths, host residue, type map,
// coded enums, whole docs, nested frames, boxing.
// ---------------------------------------------------------------------------

mod hidden {
    tst_core::binding_record! {
        /// Declared in a private module and re-exported by the registry path.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Hidden {
            pub a: u8,
        }
    }
}

#[test]
fn generated_code_names_the_registry_path_not_the_declaring_module() {
    let mut e = entry(hidden::Hidden::BINDING_SCHEMA, Direction::Both);
    assert!(
        e.schema.rust_path.contains("hidden"),
        "{}",
        e.schema.rust_path
    );
    e.rust_path = "print_records_emit::Hidden";
    let reg = vec![e];
    for text in [
        emit_py::record_pair(&reg, &reg[0]).unwrap(),
        emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap(),
    ] {
        assert!(text.contains("&print_records_emit::Hidden"), "{text}");
        assert!(!text.contains("hidden::"), "{text}");
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IcingFixture;

tst_core::binding_record! {
    /// Coded enums cross as their code.
    #[derive(Debug, Clone, PartialEq)]
    pub struct EnumFixture {
        pub id: u8,
        @[custom] @[java("icingCode")] pub icing: Option<IcingFixture>,
    }
}

#[test]
fn a_custom_enum_row_names_its_host_types_and_an_unknown_custom_type_is_refused() {
    // The row is keyed by the type's name; rows exist for the five coded enums.
    let mut s = EnumFixture::BINDING_SCHEMA;
    let fields: Vec<_> = s
        .fields
        .iter()
        .map(|f| {
            let mut f = *f;
            if f.name == "icing" {
                f.ty = "Option<crate::klv::st0601::IcingDetected>";
            }
            f
        })
        .collect();
    s.fields = Box::leak(fields.into_boxed_slice());
    let reg = vec![entry(s, Direction::Both)];
    let py = emit_pyi::py_block(&reg, &reg[0]).unwrap();
    assert!(
        py.contains("    icing: IcingDetected | int | None = None\n"),
        "{py}"
    );
    let pyi = emit_pyi::pyi_block(&reg, &reg[0]).unwrap();
    assert!(
        pyi.contains("    icing: Optional[IcingDetected | int] = ...\n"),
        "{pyi}"
    );
    let java = emit_java::block(&reg, &reg[0]).unwrap();
    assert!(java.contains("Integer icingCode"), "{java}");
    let rs = emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap();
    assert!(
        rs.contains("crate::custom::build_enum_fixture_icing(env, &r.icing)"),
        "{rs}"
    );
    for named in [
        "IcingDetected",
        "OperationalMode",
        "PlatformStatus",
        "SensorControlMode",
        "SensorFovName",
    ] {
        let mut s = EnumFixture::BINDING_SCHEMA;
        let ty: &'static str = leak(&format!("Option<{named}>"));
        let fields: Vec<_> = s
            .fields
            .iter()
            .map(|f| FieldSpec { ty, ..*f })
            .map(|f| {
                if f.name == "id" {
                    FieldSpec { ty: "u8", ..f }
                } else {
                    f
                }
            })
            .collect();
        s.fields = Box::leak(fields.into_boxed_slice());
        let reg = vec![entry(s, Direction::Both)];
        emit_pyi::py_block(&reg, &reg[0]).unwrap_or_else(|e| panic!("{named}: {e}"));
    }
    // Without a row the generator cannot name the host type.
    let reg = vec![entry(EnumFixture::BINDING_SCHEMA, Direction::Both)];
    let err = emit_pyi::py_block(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("EnumFixture.icing") && err.contains("CUSTOM row"),
        "{err}"
    );
}

tst_core::binding_record! {
    /// First paragraph, with a `code` span.
    ///
    /// Second paragraph: a < b & c > d, an @param word and a [`Link`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct DocFixture {
        /// Field text, first paragraph.
        ///
        /// Field text, second paragraph.
        pub id: u8,
    }
}

#[test]
fn the_whole_doc_is_emitted_escaped_and_wrapped() {
    let reg = vec![entry(DocFixture::BINDING_SCHEMA, Direction::Both)];
    let java = emit_java::block(&reg, &reg[0]).unwrap();
    for want in [
        " * First paragraph, with a {@code code} span.\n *\n",
        " * Second paragraph: a &lt; b &amp; c &gt; d, an &#64;param word and a {@code Link}.\n",
        " * @param id Field text, first paragraph.\n *\n *     Field text, second paragraph.\n",
    ] {
        assert!(java.contains(want), "{want}\n{java}");
    }
    let py = emit_pyi::py_block(&reg, &reg[0]).unwrap();
    assert!(
        py.contains("\"\"\"First paragraph, with a ``code`` span.\n\n    Second paragraph:"),
        "{py}"
    );
    assert!(py.contains("a ``Link``."), "{py}");
    // Long text wraps: Javadoc at 100 columns, docstrings at 79.
    let long = "word ".repeat(60);
    let wrapped = emit_pyi::docstring_paras(&[long.trim().to_string(), long.trim().to_string()]);
    assert!(
        wrapped.lines().all(|l| l.chars().count() <= 79),
        "{wrapped}"
    );
    assert_eq!(
        wrapped.lines().filter(|l| l.is_empty()).count(),
        1,
        "{wrapped}"
    );
    let whole = reg_with_doc(&long);
    let java = emit_java::block(&whole, &whole[0]).unwrap();
    assert!(java.lines().all(|l| l.chars().count() <= 100), "{java}");
    assert!(
        java.lines().filter(|l| l.starts_with(" * word")).count() > 1,
        "{java}"
    );
}

/// A record whose doc is `long`.
fn reg_with_doc(long: &str) -> Vec<Entry> {
    let mut s = DocFixture::BINDING_SCHEMA;
    s.doc = leak(long);
    vec![entry(s, Direction::Both)]
}

#[test]
fn nested_records_are_built_in_frames_and_scalars_are_boxed_with_value_of() {
    let reg = reg();
    // A record field and an optional record field, past the constructor limit.
    let rich = emit_jvm_rs::record_pair(&reg, named(&reg, "RichFixture")).unwrap();
    let frame = "env.with_local_frame_returning_local::<_, jni::errors::Error>(16, |env| ";
    assert!(
        rich.contains(&format!(
            "let g = {frame}crate::generated::st0601::build_wavelength_record(env, &r.g))?;"
        )),
        "{rich}"
    );
    assert!(
        rich.contains(&format!(
            "if let Some(v) = &r.opt_band {{ let o1 = {frame}crate::generated::st0601::build_wavelength_record(env, v))?;"
        )) || rich.contains(&format!("{frame}crate::generated::st0601::build_wavelength_record(env, v))?;")),
        "{rich}"
    );
    // An optional scalar on the constructor path is boxed through `valueOf`.
    let opt = emit_jvm_rs::record_pair(&reg, named(&reg, "OptFixture")).unwrap();
    assert!(
        opt.contains(
            "env.call_static_method(\"java/lang/Long\", \"valueOf\", \"(J)Ljava/lang/Long;\", &["
        ),
        "{opt}"
    );
    assert!(!opt.contains("new_object(\"java/lang/Long\""), "{opt}");
    // A Builder sets an optional scalar by its primitive, unboxed.
    assert!(
        rich.contains("\"optCount\", \"(J)Lorg/tstrans/klv/RichFixture$Builder;\""),
        "{rich}"
    );
    let java = emit_java::block(&reg, named(&reg, "RichFixture")).unwrap();
    assert!(java.contains("public Builder optCount(long v)"), "{java}");
    assert!(java.contains("private Long optCount;"), "{java}");
}

#[test]
fn narrow_signed_integers_and_lists_follow_the_hand_written_record() {
    let names = ["WavelengthRecord"];
    // i8 and i16 cross as Java int, range-checked on the way in.
    for (rust, narrow) in [("i8", "checked_i8"), ("i16", "checked_i16")] {
        let ty = typemap::map(rust, &names).unwrap();
        assert_eq!(ty.java(), "int");
        assert_eq!(ty.sig(&[]), "I");
        assert_eq!(
            typemap::map(&format!("Option<{rust}>"), &names)
                .unwrap()
                .java(),
            "Integer"
        );
        let typemap::Ty::Scalar(sc) = ty else {
            panic!()
        };
        assert_eq!(sc.narrow(), Some(narrow));
    }
    // u64 has no narrowing helper: the reader takes the bit pattern of the long.
    let typemap::Ty::Scalar(u64_sc) = typemap::map("u64", &names).unwrap() else {
        panic!()
    };
    assert_eq!(u64_sc.narrow(), None);
    assert_eq!(
        typemap::map("Vec<u8>", &names).unwrap().java(),
        "ByteBuffer"
    );
    assert_eq!(
        typemap::map("Option<Vec<u64>>", &names).unwrap().java(),
        "List<Long>"
    );
    assert_eq!(
        typemap::map("Vec<u32>", &names).unwrap().java(),
        "List<Long>"
    );
}

tst_core::binding_record! {
    /// A class with a hand-written tail.
    #[derive(Debug, Clone, PartialEq)]
    pub struct TailFixture {
        pub a: u8,
        pub b: Option<u8>,
    }
}

#[test]
fn hand_written_text_after_the_end_marker_survives_write_and_check() {
    let reg = vec![entry(TailFixture::BINDING_SCHEMA, Direction::Both)];
    let d = scratch("tail");
    let py_dir = d.join("bindings/python/python/tstrans");
    let java_dir = d.join("bindings/jvm/src/main/java/org/tstrans/klv");
    std::fs::create_dir_all(&py_dir).unwrap();
    std::fs::create_dir_all(&java_dir).unwrap();
    let py_marked = |c: &str| {
        format!(
            "import x\n\n# BEGIN GENERATED by print-records (TailFixture); do not edit\nold\n\
             # END GENERATED by print-records (TailFixture)\n\n    def __post_init__(self) -> None:\n\
             \x20       {c}\n\ny = 2\n"
        )
    };
    std::fs::write(py_dir.join("klv.py"), py_marked("check()")).unwrap();
    std::fs::write(py_dir.join("klv.pyi"), py_marked("...")).unwrap();
    let java_text = "package org.tstrans.klv;\n\nimport java.util.List;\n\n\
        // BEGIN GENERATED by print-records (TailFixture); do not edit\nold\n\
        // END GENERATED by print-records (TailFixture)\n\n    public TailFixture {\n        \
        if (a < 0) throw new IllegalArgumentException();\n    }\n}\n";
    std::fs::write(java_dir.join("TailFixture.java"), java_text).unwrap();

    write::write_all(&write::targets(&d, &reg).unwrap()).unwrap();
    let py = std::fs::read_to_string(py_dir.join("klv.py")).unwrap();
    assert!(
        py.starts_with("import x\n\n# BEGIN") && !py.contains("old\n"),
        "{py}"
    );
    assert!(
        py.contains("    b: int | None = None\n# END GENERATED")
            && py.ends_with("# END GENERATED by print-records (TailFixture)\n\n    def __post_init__(self) -> None:\n        check()\n\ny = 2\n"),
        "{py}"
    );
    let java = std::fs::read_to_string(java_dir.join("TailFixture.java")).unwrap();
    assert!(
        java.contains("public record TailFixture(int a, Integer b) {\n// END GENERATED")
            && java.ends_with(
                "\n    public TailFixture {\n        if (a < 0) throw new IllegalArgumentException();\n    }\n}\n"
            ),
        "{java}"
    );
    // Check compares the block only: an edited tail is not stale, an edited block is.
    assert!(write::stale(&write::targets(&d, &reg).unwrap()).is_empty());
    std::fs::write(py_dir.join("klv.py"), py.replace("check()", "other()")).unwrap();
    std::fs::write(
        java_dir.join("TailFixture.java"),
        java.replace("a < 0", "a < 1"),
    )
    .unwrap();
    let stale = write::stale(&write::targets(&d, &reg).unwrap());
    assert!(
        stale
            .iter()
            .all(|(p, _)| p.extension().is_some_and(|x| x == "rs")),
        "{stale:?}"
    );
    std::fs::write(
        py_dir.join("klv.py"),
        py.replace("b: int | None", "b: str | None"),
    )
    .unwrap();
    assert!(
        write::stale(&write::targets(&d, &reg).unwrap())
            .iter()
            .any(|(p, _)| p.ends_with("klv.py"))
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_declared_python_default_replaces_the_one_the_type_implies() {
    let schema = DefaultFixture::BINDING_SCHEMA;
    assert_eq!(schema.fields[0].py_default, None);
    assert_eq!(schema.fields[1].py_default, Some("\"x\""));
    let reg = vec![entry(schema, Direction::Both)];
    let py = emit_pyi::py_block(&reg, &reg[0]).unwrap();
    assert!(py.contains("    id: int\n"), "{py}");
    assert!(py.contains("    name: str = \"x\"\n"), "{py}");
    assert!(py.contains("    count: int = 5_000_000\n"), "{py}");
    assert!(py.contains("    note: str | None = None\n"), "{py}");
    let pyi = emit_pyi::pyi_block(&reg, &reg[0]).unwrap();
    assert!(pyi.contains("    name: str = ...\n"), "{pyi}");
}

#[test]
fn a_required_string_read_rejects_null_naming_the_field_and_an_optional_one_does_not() {
    let reg = vec![entry(DefaultFixture::BINDING_SCHEMA, Direction::FromHost)];
    let read = emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap();
    parses(&read);
    assert!(
        read.contains("crate::jutil::null_field(env, \"DefaultFixture.name\")"),
        "{read}"
    );
    // `note` is tested for null by its own `Option` branch.
    assert_eq!(read.matches("null_field(").count(), 1, "{read}");
}

#[test]
fn a_python_only_record_has_no_java_or_jvm_target() {
    let mut only = entry(WavelengthRecord::BINDING_SCHEMA, Direction::Both);
    only.family = "pyonly";
    only.jvm = false;
    let reg = vec![only];
    let targets = write::targets(&root(), &reg).unwrap();
    let paths: Vec<String> = targets
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("python/src/generated/pyonly.rs")),
        "{paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.ends_with(".java") || p.contains("jvm/src/generated")),
        "{paths:?}"
    );
    // Its Python text still renders.
    emit_py::file(&reg, "pyonly").unwrap();
}

// ---------------------------------------------------------------------------
// Native Python classes
// ---------------------------------------------------------------------------

use schema::{ExtraKind, PyExtra, PyShape};

fn native(schema: tst_core::binding_schema::RecordSchema) -> Entry {
    let mut e = Entry::new(
        "native_test",
        "tstrans.codec",
        "org/tstrans/codec",
        Direction::ToHost,
        schema.rust_path,
        "",
        schema,
    );
    e.shape = PyShape::Native;
    e
}

const LEAF_EXTRAS: &[PyExtra] = &[
    PyExtra {
        name: "as_float",
        kind: ExtraKind::Method,
        params: &[],
        ret_rs: "f64",
        ret_py: "float",
        doc: " The ratio as a float.",
    },
    PyExtra {
        name: "zero",
        kind: ExtraKind::Static,
        params: &[("den", "u32", "int")],
        ret_rs: "Self",
        ret_py: "LeafFixture",
        doc: "",
    },
];

fn native_reg() -> Vec<Entry> {
    let mut r = reg();
    let mut leaf = native(LeafFixture::BINDING_SCHEMA);
    leaf.py_ctor = true;
    leaf.py_eq = true;
    leaf.py_extras = LEAF_EXTRAS;
    leaf.py_repr = Some(("Leaf({}/{})", &["self.num", "self.den"]));
    let mut big = native(NativeFixture::BINDING_SCHEMA);
    big.host_name = "NativeHost";
    big.py_rename = &[("flag", "enabled")];
    r.push(leaf);
    r.push(big);
    r
}

#[test]
fn a_native_class_is_a_frozen_get_all_struct_of_host_copies() {
    let reg = native_reg();
    let raw = emit_pyclass::file(&reg, "native_test").unwrap();
    parses(&raw);
    // The small value: constructor, comparison, hand-written methods, repr.
    assert!(
        raw.contains(
            r#"#[pyclass(frozen, from_py_object, get_all, eq, name = "LeafFixture", module = "tstrans.codec")]"#
        ),
        "{raw}"
    );
    assert!(
        raw.contains("#[derive(Debug, Clone, Copy, PartialEq, Eq)]"),
        "{raw}"
    );
    assert!(raw.contains("#[pyo3(signature = (num, den))]"), "{raw}");
    assert!(raw.contains("fn new(num: u32, den: u32) -> Self"), "{raw}");
    assert!(
        raw.contains(
            "fn as_float(&self) -> f64 {\n        crate::custom::leaf_fixture_as_float(self)"
        ),
        "{raw}"
    );
    assert!(
        raw.contains("#[staticmethod]\n    fn zero(den: u32) -> Self {\n        crate::custom::leaf_fixture_zero(den)"),
        "{raw}"
    );
    assert!(
        raw.contains("\"Leaf({}/{})\",\n            self.num,\n            self.den"),
        "{raw}"
    );
    // The big one: the host name, host types, no constructor, no Copy.
    assert!(
        raw.contains(
            r#"#[pyclass(frozen, from_py_object, get_all, name = "NativeHost", module = "tstrans.codec")]"#
        ),
        "{raw}"
    );
    assert!(
        raw.contains("#[derive(Debug, Clone)]\npub struct NativeHost {"),
        "{raw}"
    );
    for field in [
        "pub id: u8,",
        "pub name: String,",
        "pub enabled: bool,",
        "pub raw: crate::util::HostBytes,",
        "pub header: crate::util::HostBytes,",
        "pub chroma: crate::codec::ChromaFormatPy,",
        "pub rate: Option<LeafFixture>,",
        "pub count: Option<u32>,",
        "pub leaves: Vec<LeafFixture>,",
        "pub by_id: std::collections::BTreeMap<u8, LeafFixture>,",
        "pub size: (u32, u32),",
    ] {
        // `size` is custom: its host type is the CUSTOM row's, optional.
        if field.starts_with("pub size") {
            assert!(raw.contains("pub size: Option<(u32, u32)>,"), "{raw}");
        } else {
            assert!(raw.contains(field), "{field}\n{raw}");
        }
    }
    assert!(
        !raw.contains("pub scratch"),
        "a skipped field is not held\n{raw}"
    );
    assert!(!raw.contains("fn new(id"), "{raw}");
    assert!(raw.contains("impl From<&"), "{raw}");
    assert!(raw.contains("m.add_class::<LeafFixture>()?;"), "{raw}");
    assert!(raw.contains("m.add_class::<NativeHost>()?;"), "{raw}");
}

#[test]
fn a_native_class_converts_each_shape_of_field_from_the_rust_record() {
    let reg = native_reg();
    let raw = emit_pyclass::file(&reg, "native_test").unwrap();
    for want in [
        "id: r.id,",
        "name: r.name.clone(),",
        "enabled: r.flag,",
        "raw: crate::util::HostBytes(r.raw.to_vec()),",
        "header: crate::util::HostBytes(r.header.to_vec()),",
        "chroma: crate::codec::ChromaFormatPy::from(r.chroma),",
        "rate: r.rate.as_ref().map(LeafFixture::from),",
        "count: r.count,",
        "leaves: r.leaves.iter().map(LeafFixture::from).collect(),",
        "by_id: r.by_id.iter().map(|(k, v)| (*k, LeafFixture::from(v))).collect(),",
        "size: r.size,",
    ] {
        assert!(raw.contains(want), "{want}\n{raw}");
    }
}

#[test]
fn a_native_class_stub_lists_the_constructor_properties_and_extras() {
    let reg = native_reg();
    let leaf = emit_pyclass::pyi_block(&reg, named(&reg, "LeafFixture")).unwrap();
    assert_eq!(
        leaf,
        "@final\nclass LeafFixture:\n\
         \x20   def __new__(cls, num: int, den: int) -> LeafFixture: ...\n\
         \x20   @property\n    def num(self) -> int: ...\n\
         \x20   @property\n    def den(self) -> int: ...\n\
         \x20   def __eq__(self, other: object, /) -> bool: ...\n\
         \x20   def as_float(self) -> float: ...\n\
         \x20   @staticmethod\n    def zero(den: int) -> LeafFixture: ...\n\
         "
    );
    let big = emit_pyclass::pyi_block(&reg, named(&reg, "NativeFixture")).unwrap();
    assert!(big.starts_with("@final\nclass NativeHost:\n"), "{big}");
    for want in [
        "def raw(self) -> bytes: ...",
        "def chroma(self) -> ChromaFormat: ...",
        "def rate(self) -> Optional[LeafFixture]: ...",
        "def count(self) -> Optional[int]: ...",
        "def leaves(self) -> List[LeafFixture]: ...",
        "def by_id(self) -> Dict[int, LeafFixture]: ...",
        "def size(self) -> Optional[Tuple[int, int]]: ...",
    ] {
        assert!(big.contains(want), "{want}\n{big}");
    }
    assert!(!big.contains("scratch"), "{big}");
    assert!(big.contains("def level(self) -> int: ..."), "{big}");
    assert!(!big.contains("__new__"), "{big}");
}

#[test]
fn a_native_record_has_a_stub_block_and_no_dataclass_block() {
    let reg = native_reg();
    let targets = write::targets(&root(), &reg).unwrap();
    let mine: Vec<String> = targets
        .iter()
        .map(|t| t.path.display().to_string())
        .filter(|p| p.contains("NativeHost") || p.contains("native_test"))
        .collect();
    assert!(
        mine.iter()
            .any(|p| p.ends_with("bindings/python/src/generated/native_test.rs")),
        "{mine:?}"
    );
    assert!(
        mine.iter()
            .any(|p| p.ends_with("org/tstrans/codec/NativeHost.java")),
        "{mine:?}"
    );
    let codec_py: Vec<&write::Target> = targets
        .iter()
        .filter(|t| t.path.ends_with("bindings/python/python/tstrans/codec.py"))
        .collect();
    assert!(
        codec_py
            .iter()
            .all(|t| !matches!(&t.kind, write::Kind::Block { begin, .. } if begin.contains("NativeHost") || begin.contains("LeafFixture"))),
        "a native class has no `.py` block"
    );
    let stub = targets
        .iter()
        .find(|t| {
            t.path.ends_with("bindings/python/python/tstrans/codec.pyi")
                && matches!(&t.kind, write::Kind::Block { begin, .. } if begin.contains("(NativeHost)"))
        })
        .expect("a stub block named for the host class");
    assert!(stub.body.starts_with("@final\nclass NativeHost:"));
}

#[test]
fn a_map_and_a_host_type_are_built_in_java_and_refused_the_other_way() {
    let reg = native_reg();
    let e = named(&reg, "NativeFixture");
    let jvm = emit_jvm_rs::record_pair(&reg, e).unwrap();
    parses(&jvm);
    // Twelve crossing components: built through the host-named Builder.
    assert!(
        jvm.contains(r#""org/tstrans/codec/NativeHost$Builder""#),
        "{jvm}"
    );
    assert!(
        jvm.contains("crate::codec::shared::build_chroma_format(env, r.chroma)"),
        "{jvm}"
    );
    assert!(
        jvm.contains("map_err(|()| jni::errors::Error::JavaException)?"),
        "{jvm}"
    );
    assert!(jvm.contains(r#""java/util/HashMap""#), "{jvm}");
    assert!(
        jvm.contains(r#""(Ljava/util/Map;)Lorg/tstrans/codec/NativeHost$Builder;""#),
        "{jvm}"
    );
    assert!(
        jvm.contains("crate::custom::build_native_fixture_size(env, &r.size)"),
        "{jvm}"
    );
    assert!(!jvm.contains("read_native_fixture"), "{jvm}");
    let java = emit_java::block(&reg, e).unwrap();
    assert!(java.contains("Map<Integer, LeafFixture> byId"), "{java}");
    assert!(java.contains("ChromaFormat chroma"), "{java}");
    assert!(java.contains("public record NativeHost("), "{java}");
    // A record that also crosses in has no way to read a map or an enum.
    let mut both = native_reg();
    let i = both
        .iter()
        .position(|x| x.schema.name == "NativeFixture")
        .unwrap();
    both[i].direction = Direction::Both;
    let err = emit_jvm_rs::record_pair(&both, &both[i]).unwrap_err();
    assert!(
        err.contains("NativeFixture.chroma") && err.contains("only crosses to the host"),
        "{err}"
    );
}

tst_core::binding_record! {
    /// A record with an enum the hosts hold as a class.
    #[derive(Debug, Clone, PartialEq)]
    pub struct HostFixture {
        pub chroma: ChromaFormat,
        pub n: u8,
    }
}

#[test]
fn a_dataclass_refuses_a_map_or_a_host_typed_field() {
    let mut reg = reg();
    reg.push(entry(HostFixture::BINDING_SCHEMA, Direction::ToHost));
    let e = named(&reg, "HostFixture");
    let err = emit_py::record_pair(&reg, e).unwrap_err();
    assert!(
        err.contains("HostFixture.chroma") && err.contains("native class only"),
        "{err}"
    );
}

#[test]
fn a_native_struct_holds_the_crossing_fields_and_no_others() {
    let reg = native_reg();
    let raw = emit_pyclass::file(&reg, "native_test").unwrap();
    for (name, held) in [("LeafFixture", 2), ("NativeHost", 12)] {
        // The struct body: between its `pub struct` line and the closing brace.
        let from = raw.find(&format!("pub struct {name} {{")).unwrap();
        let body = &raw[from..from + raw[from..].find("\n}\n").unwrap()];
        assert_eq!(body.matches("\n    pub ").count(), held, "{name}\n{body}");
    }
    // Every held field is read in the conversion, one line each.
    let at = raw
        .find("impl From<&print_records_emit::NativeFixture> for NativeHost")
        .unwrap();
    let conv = raw[at..].split("\n}\n").next().unwrap();
    let assigned = conv
        .lines()
        .filter(|l| l.starts_with("            ") && l.trim_start().contains(": "))
        .count();
    assert_eq!(assigned, 12, "{conv}");
}

#[test]
fn py_rename_must_name_a_field() {
    // `NativeHost` is `NativeFixture` to the hosts: no other record holds it.
    let mut reg = native_reg();
    let mut holder = native(OptFixture::BINDING_SCHEMA);
    holder.family = "other";
    let _ = &mut holder;
    reg.push(holder);
    assert!(emit_pyclass::file(&reg, "other").is_ok());
    let i = reg
        .iter()
        .position(|x| x.schema.name == "NativeFixture")
        .unwrap();
    reg[i].py_rename = &[("nope", "x")];
    let err = emit_pyclass::file(&reg, "native_test").unwrap_err();
    assert!(err.contains("names no field `nope`"), "{err}");
}

#[test]
fn a_record_java_builds_itself_has_a_java_block_and_no_jni_builder() {
    let mut reg = native_reg();
    let i = reg
        .iter()
        .position(|x| x.schema.name == "LeafFixture")
        .unwrap();
    reg[i].jvm_builder = false;
    let jvm = emit_jvm_rs::file(&reg, "native_test").unwrap();
    assert!(!jvm.contains("fn build_leaf_fixture"), "{jvm}");
    assert!(jvm.contains("fn build_native_fixture"), "{jvm}");
    let targets = write::targets(&root(), &reg).unwrap();
    assert!(
        targets
            .iter()
            .any(|t| t.path.ends_with("org/tstrans/codec/LeafFixture.java")),
        "the Java record is still generated"
    );
}

// ---------------------------------------------------------------------------
// Counters: usize, Duration, a record only Java exposes, one host name twice
// ---------------------------------------------------------------------------

tst_core::binding_record! {
    /// A counter snapshot with a count and a duration.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct CounterFixture {
        /// Live subscribers.
        pub peers: usize,
        /// Smoothed round-trip time in whole microseconds.
        pub rtt: std::time::Duration,
        /// Time since the last cut, absent before the first.
        pub age: Option<std::time::Duration>,
    }
}

fn counter_reg() -> Vec<Entry> {
    let mut e = native(CounterFixture::BINDING_SCHEMA);
    e.py_rename = &[("rtt", "rtt_us")];
    vec![e]
}

#[test]
fn a_count_and_a_duration_cross_to_the_host_as_wide_unsigned_values() {
    let reg = counter_reg();
    let raw = emit_pyclass::file(&reg, "native_test").unwrap();
    parses(&raw);
    for want in [
        "pub peers: u64,",
        "pub rtt_us: u64,",
        "pub age: Option<u64>,",
        "peers: r.peers as u64,",
        "rtt_us: u64::try_from(r.rtt.as_micros()).unwrap_or(u64::MAX),",
        "age: r.age.map(|v| u64::try_from(v.as_micros()).unwrap_or(u64::MAX)),",
    ] {
        assert!(raw.contains(want), "{want}\n{raw}");
    }
    let jvm = emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap();
    parses(&jvm);
    // Java reads all three as `long`, an absent duration as `null`.
    assert!(
        jvm.contains("(JJLjava/lang/Long;)V"),
        "a count and a duration are long, an optional one boxed\n{jvm}"
    );
    assert!(jvm.contains("JValue::Long(r.peers as i64)"), "{jvm}");
    assert!(
        jvm.contains("i64::try_from(r.rtt.as_micros()).unwrap_or(i64::MAX)"),
        "{jvm}"
    );
    let java = emit_java::block(&reg, &reg[0]).unwrap();
    assert!(
        java.contains("public record CounterFixture(long peers, long rtt, Long age)"),
        "{java}"
    );
}

#[test]
fn a_count_or_a_duration_only_crosses_to_the_host() {
    let reg = vec![entry(CounterFixture::BINDING_SCHEMA, Direction::Both)];
    let err = emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("CounterFixture.peers") && err.contains("only crosses to the host"),
        "{err}"
    );
}

#[test]
fn a_record_only_java_exposes_has_a_jni_builder_and_a_java_block_and_no_python_target() {
    let mut java_only = native(CounterFixture::BINDING_SCHEMA);
    java_only.python = false;
    let reg = vec![java_only];
    let targets = write::targets(&root(), &reg).unwrap();
    let paths: Vec<String> = targets
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("jvm/src/generated/native_test.rs")),
        "{paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("org/tstrans/codec/CounterFixture.java")),
        "{paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.contains("python/src/generated") || p.ends_with(".pyi")),
        "{paths:?}"
    );
}

#[test]
fn two_records_with_one_host_name_keep_to_their_own_module() {
    let mut tcp = native(CounterFixture::BINDING_SCHEMA);
    tcp.family = "stats_a";
    tcp.py_module = "tstrans.tcp";
    tcp.host_name = "SocketStats";
    tcp.jvm = false;
    let mut udp = native(LeafFixture::BINDING_SCHEMA);
    udp.family = "stats_b";
    udp.py_module = "tstrans.udp";
    udp.host_name = "SocketStats";
    udp.jvm = false;
    let reg = vec![tcp, udp];
    let targets = write::targets(&root(), &reg).unwrap();
    let paths: Vec<String> = targets
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    for want in [
        "python/src/generated/stats_a.rs",
        "python/src/generated/stats_b.rs",
        "python/python/tstrans/tcp.pyi",
        "python/python/tstrans/udp.pyi",
    ] {
        assert!(paths.iter().any(|p| p.ends_with(want)), "{want}: {paths:?}");
    }
    for fam in ["stats_a", "stats_b"] {
        let raw = emit_pyclass::file(&reg, fam).unwrap();
        assert!(raw.contains("pub struct SocketStats {"), "{raw}");
        assert!(raw.contains("name = \"SocketStats\""), "{raw}");
    }
}

#[test]
fn a_host_enum_row_names_its_python_wrapper_and_its_jni_builder() {
    let t = typemap::map("ClockAlignment", &[]).unwrap();
    assert_eq!(t.host_rs(), "crate::rtp::publish::ClockAlignmentPy");
    assert_eq!(t.java(), "ClockAlignment");
    assert_eq!(t.sig(&[]), "Lorg/tstrans/rtp/ClockAlignment;");
    assert_eq!(
        typemap::host_jvm("ClockAlignment"),
        "crate::rtp::publish::build_clock_alignment"
    );
}

#[test]
fn a_record_with_an_unsigned_64_bit_component_says_how_java_reads_it() {
    // `RtpStats` counts in `u64`; a `u32`-only record needs no note.
    let reg = schema::records();
    let with = emit_java::block(&reg, named(&reg, "RtpStats")).unwrap();
    assert!(
        with.contains("bit pattern of a signed {@code long}"),
        "{with}"
    );
    let reg = native_reg();
    let none = emit_java::block(&reg, named(&reg, "LeafFixture")).unwrap();
    assert!(!none.contains("bit pattern"), "{none}");
}

tst_core::binding_record! {
    /// A snapshot that holds a clock alignment.
    #[derive(Debug, Clone, Default)]
    pub struct AlignedFixture {
        /// How the tracks are aligned.
        pub alignment: ClockAlignment,
        /// Steps taken.
        pub steps: u64,
    }
}

#[test]
fn a_host_value_that_can_fail_makes_the_class_construction_fallible() {
    let mut e = native(AlignedFixture::BINDING_SCHEMA);
    e.family = "aligned_test";
    let reg = vec![e];
    let raw = emit_pyclass::file(&reg, "aligned_test").unwrap();
    parses(&raw);
    for want in [
        "impl TryFrom<&",
        "type Error = PyErr;",
        "fn try_from(r: &",
        "alignment: crate::rtp::publish::ClockAlignmentPy::try_from(r.alignment)?,",
        "steps: r.steps,",
        "        })\n    }\n}",
    ] {
        assert!(raw.contains(want), "{want}\n{raw}");
    }
    assert!(!raw.contains("impl From<&"), "{raw}");
    // The Java builder throws on the same value, so both hosts fail alike.
    let jvm = emit_jvm_rs::record_pair(&reg, &reg[0]).unwrap();
    assert!(
        jvm.contains("crate::rtp::publish::build_clock_alignment("),
        "{jvm}"
    );
}

fn manifest_row(item: &str, python: &str, java: &str) -> String {
    format!(
        "[[surface]]\nitem = \"{item}\"\nowning_tests = [\"t.rs\"]\n\
         bindings = [\"c:unaudited\", \"{python}\", \"{java}\", \"swift:deferred\", \"kotlin:deferred\"]\n\n"
    )
}

#[test]
fn a_record_writes_the_python_and_java_cells_of_its_own_row_only() {
    let reg = schema::records();
    let wavelength = reg
        .iter()
        .find(|e| e.schema.name == "WavelengthRecord")
        .expect("registered");
    let item = wavelength.rust_path;
    let text = manifest_row(item, "python:unaudited", "java:unaudited")
        + &manifest_row(
            &format!("{item}::new"),
            "python:unaudited",
            "java:unaudited",
        );
    let out = emit_manifest::apply(&text, &emit_manifest::cells(&reg)).expect("applies");
    let own = manifest_row(
        item,
        "python:tstrans.klv.WavelengthRecord",
        "java:org.tstrans.klv.WavelengthRecord",
    );
    assert!(out.starts_with(&own), "{out}");
    // The row of a method keeps its cells.
    assert!(out.ends_with(&manifest_row(
        &format!("{item}::new"),
        "python:unaudited",
        "java:unaudited"
    )));
}

#[test]
fn a_record_without_a_row_writes_nothing_and_applying_twice_changes_nothing() {
    let reg = schema::records();
    let text = manifest_row(
        "tst_core::other::Thing",
        "python:unaudited",
        "java:unaudited",
    );
    let cells = emit_manifest::cells(&reg);
    let once = emit_manifest::apply(&text, &cells).expect("applies");
    assert_eq!(once, text);
    let own = format!(
        "{}{}",
        text,
        manifest_row(reg[0].rust_path, "python:unaudited", "java:unaudited")
    );
    let first = emit_manifest::apply(&own, &cells).expect("applies");
    assert_ne!(first, own);
    assert_eq!(
        emit_manifest::apply(&first, &cells).expect("applies"),
        first
    );
}

#[test]
fn a_record_the_host_builds_by_hand_leaves_its_hand_cell() {
    let reg = schema::records();
    let core_id = reg
        .iter()
        .find(|e| e.schema.name == "CoreId")
        .expect("registered");
    assert!(!core_id.jvm);
    let cells = emit_manifest::cells(&reg);
    assert!(cells.iter().any(|c| c.item == core_id.rust_path
        && c.prefix == "python"
        && c.text == "python:tstrans.klv.CoreId"));
    assert!(
        !cells
            .iter()
            .any(|c| c.item == core_id.rust_path && c.prefix == "java")
    );
}

#[test]
fn a_row_without_the_column_is_an_error() {
    let reg = schema::records();
    let item = reg[0].rust_path;
    let text = format!(
        "[[surface]]\nitem = \"{item}\"\nbindings = [\"c:unaudited\", \"swift:deferred\"]\n"
    );
    let err = emit_manifest::apply(&text, &emit_manifest::cells(&reg)).expect_err("no column");
    assert!(err.contains("column"), "{err}");
}
