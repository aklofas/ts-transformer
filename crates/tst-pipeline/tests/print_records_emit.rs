//! Emitter tests for the `print-records` binding generator: the committed
//! text it produces for the pilot record, the shapes it takes for records the
//! pilot does not cover, and the registry's completeness.

// The bin's modules are compiled whole; the tests reach only some of them.
#![allow(dead_code)]

#[path = "tools/print_records/emit_java.rs"]
mod emit_java;
#[path = "tools/print_records/emit_jvm_rs.rs"]
mod emit_jvm_rs;
#[path = "tools/print_records/emit_py.rs"]
mod emit_py;
#[path = "tools/print_records/emit_pyi.rs"]
mod emit_pyi;
#[path = "tools/print_records/schema.rs"]
mod schema;
#[path = "tools/print_records/typemap.rs"]
mod typemap;
#[path = "tools/print_records/write.rs"]
mod write;

use std::path::{Path, PathBuf};

use schema::{Direction, Entry};
use tst_core::klv::st0601::WavelengthRecord;
use tst_core::klv::universal_label::UniversalLabel;

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
    Entry {
        family: "test",
        py_module: "tstrans.klv",
        java_pkg: "org/tstrans/klv",
        direction,
        rust_path: schema.rust_path,
        java_implements: "",
        schema,
    }
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
            pub stats: std::time::Duration,
        }
    }
    let reg = vec![entry(Bad::BINDING_SCHEMA, Direction::Both)];
    let err = emit_py::record_pair(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("Bad.stats") && err.contains("Duration"),
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
    let header =
        "// GENERATED by print-records from tst-core binding_record! declarations; do not edit.\n";
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
    ];
    let reg = schema::records();
    let mut leaks = Vec::new();
    for e in &reg {
        let n = e.schema.name;
        let java = emit_java::block(&reg, e).unwrap();
        let py = emit_pyi::py_block(&reg, e).unwrap();
        // The Javadoc is the text above the record header; the docstring is
        // the text between the first two triple quotes.
        let javadoc: String = java
            .lines()
            .take_while(|l| !l.starts_with("public record"))
            .collect::<Vec<_>>()
            .join("\n");
        let docstring = py.split("\"\"\"").nth(1).unwrap_or("").to_string();
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
            for span in code_spans(doc) {
                for bad in rust_only {
                    if span.contains(bad) {
                        leaks.push(format!("{n} ({host}) code span `{span}` carries `{bad}`"));
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
    assert_eq!(reg.len(), 16);
    for e in &reg {
        let n = e.schema.name;
        let py = emit_py::record_pair(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        let jvm = emit_jvm_rs::record_pair(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        parses(&py);
        parses(&jvm);
        emit_pyi::py_block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        emit_pyi::pyi_block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        let java = emit_java::block(&reg, e).unwrap_or_else(|m| panic!("{n}: {m}"));
        assert!(java.contains(&format!("public record {n}(")), "{n}: {java}");
        // The reader reserves the same local references as the builder.
        let ways = usize::from(e.direction.crosses_out()) + usize::from(e.direction.crosses_in());
        assert_eq!(
            jvm.matches("ensure_local_capacity(").count(),
            ways,
            "{n}: {jvm}"
        );
    }
    // Two converter files per family, three host texts per record.
    let targets = write::targets(&root(), &reg).unwrap();
    let mut families: Vec<&str> = reg.iter().map(|e| e.family).collect();
    families.dedup();
    assert_eq!(targets.len(), 2 * families.len() + 3 * reg.len());
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
    let src =
        std::fs::read_to_string(root().join("crates/tst-core/src/klv/st0601/model.rs")).unwrap();
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
