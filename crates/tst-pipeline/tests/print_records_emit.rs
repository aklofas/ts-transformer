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
    assert!(raw.contains("ensure_local_capacity(40)"), "{raw}"); // 2 * 4 + 32
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
        emit_java::file(&reg, e).unwrap(),
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
        "let o1 = crate::generated::st0601::build_wavelength_record(env, item)?;",
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
    let java = emit_java::file(&reg, e).unwrap();
    assert!(java.contains("public static final class Builder"), "{java}");
    assert!(java.contains("int typeCode"), "{java}");
    assert!(java.contains("import java.util.List;"), "{java}");
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
fn a_required_field_after_an_optional_one_is_refused() {
    tst_core::binding_record! {
        pub struct Misordered {
            pub a: Option<u8>,
            pub b: u8,
        }
    }
    let reg = vec![entry(Misordered::BINDING_SCHEMA, Direction::Both)];
    let err = emit_pyi::py_block(&reg, &reg[0]).unwrap_err();
    assert!(
        err.contains("Misordered.b") && err.contains("optional"),
        "{err}"
    );
}

#[test]
fn doc_text_cannot_close_a_comment_or_a_docstring() {
    let doc = "Ends */ here, has a \\u0041 escape, a \"\"\" and a back\\slash \"";
    let java = emit_java::escape_javadoc(doc);
    assert!(!java.contains("*/") && !java.contains("\\u"), "{java}");
    assert_eq!(
        emit_java::escape_javadoc("a `min_nm` b"),
        "a {@code min_nm} b"
    );
    let py = emit_pyi::escape_docstring(doc);
    assert!(!py.contains("\"\"\""), "{py}");
    assert!(py.contains("back\\\\slash"), "{py}");
    assert!(py.ends_with("\\\""), "a trailing quote is escaped: {py}");
}

#[test]
fn a_java_keyword_field_needs_an_override() {
    tst_core::binding_record! {
        pub struct Kw {
            pub class: u8,
        }
    }
    let reg = vec![entry(Kw::BINDING_SCHEMA, Direction::Both)];
    let err = emit_java::file(&reg, &reg[0]).unwrap_err();
    assert!(err.contains("keyword"), "{err}");
}

#[test]
fn naming_helpers() {
    assert_eq!(schema::snake("WavelengthRecord"), "wavelength_record");
    assert_eq!(schema::snake("UasDatalinkLs"), "uas_datalink_ls");
    assert_eq!(schema::camel("min_nm"), "minNm");
    assert_eq!(
        schema::public_path(&WavelengthRecord::BINDING_SCHEMA),
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
    let reg = schema::records();
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
    write::write_all(&write::targets(&d, &reg).unwrap()).unwrap();
    assert_eq!(write::stale(&write::targets(&d, &reg).unwrap()), Vec::new());
    let py = std::fs::read_to_string(py_dir.join("klv.py")).unwrap();
    assert!(
        py.starts_with("x = 1\n# BEGIN") && py.ends_with("y = 2\n") && !py.contains("old\n"),
        "{py}"
    );
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
    let out = std::process::Command::new(bin).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("WavelengthRecord"));
}
