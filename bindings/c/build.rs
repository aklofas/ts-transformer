//! Runs cbindgen to generate `target/<profile>/include/tstrans.h`.

use std::env;
use std::path::PathBuf;

#[path = "header_post.rs"]
mod header_post;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    // The header is emitted into target/<profile>/include/tstrans.h, which is a
    // sibling-of-OUT_DIR layout suitable for downstream consumers to find on
    // their include path. OUT_DIR is .../target/<profile>/build/tst-c-<hash>/out;
    // walk three levels up to get .../target/<profile>.
    let profile_dir = PathBuf::from(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR ancestor walk failed")
        .to_path_buf();
    let include_dir = profile_dir.join("include");
    std::fs::create_dir_all(&include_dir).expect("create include dir");
    let header_path = include_dir.join("tstrans.h");

    // The `#[no_mangle]` entry points live in the embeddable `tst-c-core`
    // rlib (this leaf crate only re-exports them), so cbindgen scans the core
    // crate's source tree. cbindgen.toml stays in this crate.
    let core_dir = PathBuf::from(&crate_dir).join("core");

    let mut config =
        cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).expect("cbindgen.toml");
    // Every `pub enum Tst*` joins `[export] include` here, so an enum no
    // signature references still reaches the header.
    let enums = header_post::mirror_enums(&core_dir.join("src"));
    assert!(!enums.is_empty(), "no `pub enum Tst*` found under core/src");
    config.export.include.extend(enums);

    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&core_dir)
        .generate()
        .expect("cbindgen generate")
        .write_to_file(&header_path);

    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=header_post.rs");
    println!("cargo:rerun-if-changed=core/src");
    // cbindgen scans tst-c-core (.with_crate above) including its Cargo.toml for
    // the cfg/feature gates that drive the [defines] block; Cargo only auto-watches
    // THIS crate's Cargo.toml, so watch the core's explicitly or a feature-only
    // edit would skip header regen on incremental builds.
    println!("cargo:rerun-if-changed=core/Cargo.toml");

    // Post-process: `#define TST_HAS_<F> 1` for each active feature, then the
    // domain-grouping section dividers (symbol-prefix keyed, so a source-tree
    // reorg does not move a declaration).
    let active: Vec<&str> = header_post::FEATURES
        .iter()
        .copied()
        .filter(|f| env::var(format!("CARGO_FEATURE_{f}")).is_ok())
        .collect();
    let rendered = std::fs::read_to_string(&header_path).expect("read tstrans.h");
    let header =
        header_post::add_section_dividers(&header_post::inject_feature_defines(&rendered, &active));
    let indented = header_post::leading_space_decls(&header);
    assert!(
        indented.is_empty(),
        "declarations left indented after post-processing: {indented:?}"
    );
    std::fs::write(&header_path, header).expect("write tstrans.h");

    // ────────────────────────────────────────────────────────────────
    // Symbol hygiene: the libtstrans
    // dynamic export table must contain only tst_*/TST_* symbols (the
    // srt_*/SRT_*/mbedtls_* symbols from the statically-linked libsrt /
    // mbedTLS must NOT leak).
    //
    // Linux does NOT use `-Wl,--exclude-libs=ALL` for this. That flag
    // localizes the symbols of EVERY static archive on the link line —
    // which includes the tst-c-core
    // rlib carrying our own #[no_mangle] tst_* entry points, so it would
    // hide the entire public ABI. It is also
    // redundant: rustc's auto-emitted anonymous version-script for a
    // cdylib already localizes every symbol that is not a crate-graph
    // `pub` `#[no_mangle]` export, so libsrt/mbedTLS symbols are emitted
    // with LOCAL binding (`t`/`b`/`r` in `nm`) and never reach the
    // dynamic export table. The `srt_symbols_not_exported` integration
    // test enforces this invariant (0 global srt_*/SRT_* symbols).
    //
    // macOS still uses -exported_symbols_list (whitelist by symbol-name
    // pattern; the leaf crate's exports.txt). Windows MSVC has no export
    // restriction yet: the first Windows block below only handles the
    // dual-mbedTLS link, and the second documents the missing .def file.
    // ────────────────────────────────────────────────────────────────

    #[cfg(target_os = "linux")]
    {
        // Dual-mbedTLS coexistence. srt-sys and rist-sys each build
        // their OWN static copy of the SAME shared source tree
        // (`crates/mbedtls-src/vendor/mbedtls`, reached via
        // `tstrans_mbedtls_src::source_dir()`) — rist-sys points librist's
        // meson at that build via CMAKE_PREFIX_PATH rather than letting
        // librist bundle its own `contrib/mbedtls`. When BOTH the `srt` and
        // `rist` features are active, those two independent static builds
        // export the same `mbedtls_*` symbols. The default linker errors
        // (`multiple definition of mbedtls_sha256_init`); if force-linked
        // anyway, the two copies' split global state corrupts SRT loopback at
        // runtime (segfault). `--allow-multiple-definition` collapses every
        // `mbedtls_*` reference onto the FIRST definition (srt-sys's build),
        // so the cdylib links AND both libraries share one consistent
        // mbedTLS at runtime — verified: the full `--all-features` test
        // suite (incl. SRT loopback + RIST) passes on all four gating
        // platforms (linux-x86_64/aarch64, macos-arm64, windows-msvc).
        // Scoped to the srt+rist combo so single-transport builds keep strict
        // duplicate-symbol checking. The clean fix is one shared BUILD, not
        // just one shared source tree (crates/rist-sys/Cargo.toml explains
        // the shared tree).
        if std::env::var("CARGO_FEATURE_SRT").is_ok() && std::env::var("CARGO_FEATURE_RIST").is_ok()
        {
            println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
        }
    }

    // MSVC equivalent of the Linux dual-mbedTLS dedup above: srt-sys and
    // rist-sys each build their own static copy of the same shared
    // `crates/mbedtls-src/vendor/mbedtls` source tree, so two byte-identical
    // static copies export duplicate `mbedtls_*` symbols and link.exe errors
    // (LNK2005/LNK1169) without an override. `/FORCE:MULTIPLE` collapses onto
    // the first definition — the same effect `--allow-multiple-definition`
    // gives on Linux, safe because the two copies are the identical 3.6.x.
    #[cfg(target_os = "windows")]
    {
        if std::env::var("CARGO_FEATURE_SRT").is_ok() && std::env::var("CARGO_FEATURE_RIST").is_ok()
        {
            println!("cargo:rustc-link-arg=/FORCE:MULTIPLE");
        }
    }

    #[cfg(target_os = "macos")]
    {
        let crate_dir_path = PathBuf::from(&crate_dir);
        let exports_path = crate_dir_path.join("exports.txt");
        println!("cargo:rerun-if-changed=exports.txt");
        println!(
            "cargo:rustc-link-arg=-Wl,-exported_symbols_list,{}",
            exports_path.display()
        );
    }

    #[cfg(target_os = "windows")]
    {
        // Deferred: Windows MSVC linker uses .def files (/DEF:foo.def) or
        // per-symbol /EXPORT: args to restrict the dynamic export table the
        // way -exported_symbols_list does above for macOS; both are
        // mechanically straightforward but nobody has authored the
        // tst-c.def symbol list yet. This is NOT blocked on Windows
        // CI/hardware — windows-msvc runs the full nextest suite
        // (--all-features, incl. RIST) as a gating platform. When the .def
        // file lands, add:
        //   println!("cargo:rerun-if-changed=tst-c.def");
        //   println!("cargo:rustc-link-arg=/DEF:tst-c.def");
        // For now: compile+link still works (no export-restriction means
        // all symbols remain exported).
    }

    // pkg-config substitution.
    let version = env!("CARGO_PKG_VERSION");
    let template_path = PathBuf::from(&crate_dir).join("tstrans.pc.in");
    let template = std::fs::read_to_string(&template_path).expect("read tstrans.pc.in");
    // C++ runtime + libdl are platform-shaped: macOS's runtime is `-lc++`
    // (no `-lstdc++`, and `-ldl` is a Linux-only libc split — macOS's dl*
    // symbols live in libSystem, linked implicitly). Everything else
    // (`-lpthread`, `-lm`, `-ltstrans`) is common across platforms and
    // stays in the template unsubstituted.
    let platform_libs = if cfg!(target_os = "macos") {
        "-lc++"
    } else {
        "-lstdc++ -ldl"
    };
    let pc = template
        .replace("@VERSION@", version)
        .replace("@PREFIX@", "/usr/local") // tarball install default; consumer can sed it
        .replace("@PLATFORM_LIBS@", platform_libs);
    let pc_path = profile_dir.join("tstrans.pc");
    std::fs::write(&pc_path, pc).expect("write tstrans.pc");
    println!("cargo:rerun-if-changed=tstrans.pc.in");
}
