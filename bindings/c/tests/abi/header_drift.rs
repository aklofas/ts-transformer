//! Verifies that the committed header matches what cbindgen would emit
//! against the current source tree, plus the build.rs post-process step
//! that inserts domain-grouping section dividers.
//! Drift indicates a forgotten regenerate-and-commit step.

// The committed header is generated with `srt` + `rtp` on. The feature
// defines live in `tstrans_config.h`, not in the header, so the comparison no
// longer depends on them; the test stays gated to that flavor until the
// per-flavor `tst-c feature matrix` CI jobs (compile-level cfg-leak detection
// via `scripts/check/c/header-conditional-sections.sh`) are the only check.
// Regenerate with `cargo build -p tst-c --features srt,rtp`.
#![cfg(all(feature = "srt", feature = "rtp"))]

use std::fs;
use std::path::PathBuf;

#[path = "../../header_post.rs"]
mod header_post;

#[test]
fn committed_header_matches_cbindgen_output() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let committed = PathBuf::from(manifest_dir).join("include/tstrans.h");
    let expected = fs::read_to_string(&committed).expect("read committed tstrans.h");

    // The `#[no_mangle]` entry points live in the embeddable `tst-c-core`
    // rlib (this crate re-exports them via `pub use tst_c_core::*`), so
    // cbindgen scans the core crate - same as build.rs.
    let core_dir = PathBuf::from(manifest_dir).join("core");
    let mut config = cbindgen::Config::from_file(format!("{manifest_dir}/cbindgen.toml"))
        .expect("read cbindgen.toml");
    config
        .export
        .include
        .extend(header_post::mirror_enums(&core_dir.join("src")));
    let generated = cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&core_dir)
        .generate()
        .expect("cbindgen generate");
    let mut buf = Vec::new();
    generated.write(&mut buf);
    let generated = String::from_utf8(buf).expect("cbindgen output is utf-8");

    let generated = header_post::add_section_dividers(&generated);

    // Normalize line endings before comparing: on Windows the committed header
    // is checked out with CRLF (default core.autocrlf, no .gitattributes pins
    // it to LF), while cbindgen emits LF — so a raw compare spuriously reports
    // drift. The drift check is about CONTENT, not EOL, so compare LF-normalized.
    let expected = expected.replace("\r\n", "\n");
    let generated = generated.replace("\r\n", "\n");

    if generated != expected {
        eprintln!(
            "Drift between bindings/c/include/tstrans.h and cbindgen output. \
             Run `cargo build -p tst-c --features srt,rtp && cp target/debug/include/tstrans.h bindings/c/include/tstrans.h` \
             and commit the diff."
        );
        // Compute a small diff hint: first differing line.
        for (i, (g, e)) in generated.lines().zip(expected.lines()).enumerate() {
            if g != e {
                eprintln!("first diff at line {}:", i + 1);
                eprintln!("  expected: {e}");
                eprintln!("  generated: {g}");
                break;
            }
        }
        panic!("header drift");
    }
}

#[test]
fn leading_space_decls_flags_only_indented_top_level_declarations() {
    let header = " int tst_planted(void);\nint tst_ok(void);\n  int member;\n * `tst_doc(void)` in a comment\n";
    assert_eq!(
        header_post::leading_space_decls(header),
        vec![" int tst_planted(void);"]
    );
}

#[test]
fn mirror_enums_finds_every_column_zero_tst_enum() {
    let dir = std::env::temp_dir().join(format!("tst-mirror-enums-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("a.rs"), "#[repr(i32)]\npub enum TstPlanted { A = 0 }\n    pub enum TstIndented {}\npub enum Other {}\n").unwrap();
    std::fs::write(dir.join("nested/b.rs"), "pub enum TstNested {\n}\n").unwrap();
    let found = header_post::mirror_enums(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        found,
        vec!["TstNested".to_string(), "TstPlanted".to_string()]
    );
}

#[test]
fn mirror_enums_sees_every_c_binding_enum() {
    let core_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("core/src");
    assert_eq!(header_post::mirror_enums(&core_src).len(), 24);
}

#[test]
fn config_defines_only_the_active_features() {
    let config = header_post::render_config(&header_post::FEATURES[..2]);
    let defines: Vec<&str> = config
        .lines()
        .filter(|l| l.starts_with("#define TST_HAS_"))
        .collect();
    assert_eq!(
        defines,
        vec!["#define TST_HAS_SRT 1", "#define TST_HAS_RTP 1"]
    );
    assert!(
        config.contains("#ifndef TSTRANS_CONFIG_H")
            && config.ends_with("#endif /* TSTRANS_CONFIG_H */\n")
    );

    // The committed config is the bare flavour: a guard and no feature define.
    let committed = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include/tstrans_config.h"),
    )
    .expect("read committed tstrans_config.h")
    .replace("\r\n", "\n");
    assert!(!committed.contains("#define TST_HAS_"));
    assert!(committed.contains("#ifndef TSTRANS_CONFIG_H"));
}
