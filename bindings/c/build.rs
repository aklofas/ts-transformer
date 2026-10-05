//! Runs cbindgen to generate `target/<profile>/include/tstrans.h`.

use std::env;
use std::path::PathBuf;

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

    let config =
        cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).expect("cbindgen.toml");

    // The `#[no_mangle]` entry points now live in the embeddable `tst-c-core`
    // rlib (this leaf crate only re-exports them via `pub use tst_c_core::*`),
    // so cbindgen scans the core crate's source tree. cbindgen.toml stays in
    // this crate.
    let core_dir = PathBuf::from(&crate_dir).join("core");

    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&core_dir)
        .generate()
        .expect("cbindgen generate")
        .write_to_file(&header_path);

    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=core/src");
    // cbindgen scans tst-c-core (.with_crate above) including its Cargo.toml for
    // the cfg/feature gates that drive the [defines] block; Cargo only auto-watches
    // THIS crate's Cargo.toml, so watch the core's explicitly or a feature-only
    // edit would skip header regen on incremental builds.
    println!("cargo:rerun-if-changed=core/Cargo.toml");

    // Post-process 1: inject TST_HAS_SRT / TST_HAS_RTP feature defines into
    // the header immediately after the include guard open, so consumer C code
    // can `#if TST_HAS_SRT` without external compiler flags. cbindgen's
    // [defines] block emits `#if defined(TST_HAS_SRT)` guards around items
    // but does not emit the matching `#define TST_HAS_SRT 1` — we emit it
    // here based on the cargo features active for this build.
    inject_feature_defines(&header_path);

    // Post-process 2: domain-grouping section dividers.
    // Keys on symbol-name prefix, independent of the source-file layout
    // (cbindgen is symbol-based, not file-based, so symbol ordering
    // stays stable across source-tree restructuring).
    add_section_dividers(&header_path);

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

/// Inject `#define TST_HAS_<FEATURE> 1` lines (SRT / RTP / UDP / TCP /
/// HLS / RIST) into the generated header immediately after the
/// `#define TSTRANS_H` include-guard line, one per cargo feature active
/// for this build. cbindgen's `[defines]` block wraps cfg-gated items in
/// `#if defined(TST_HAS_SRT)` guards but does not emit the matching
/// `#define TST_HAS_SRT 1` — this function bridges the gap.
///
/// Without these defines, a consumer who includes `tstrans.h` against a
/// full-featured `libtstrans` would see no SRT symbols (the `#if defined`
/// guards would all be false). With them, the header is self-describing:
/// the defines reflect what the linked library actually exports.
fn inject_feature_defines(header_path: &std::path::Path) {
    // Collect the defines we need to inject based on cargo features active
    // for this build. `CARGO_FEATURE_*` env vars are set by cargo for each
    // active feature (`srt` → `CARGO_FEATURE_SRT`, etc.).
    let mut defines = String::new();
    if std::env::var("CARGO_FEATURE_SRT").is_ok() {
        defines.push_str("#define TST_HAS_SRT 1\n");
    }
    if std::env::var("CARGO_FEATURE_RTP").is_ok() {
        defines.push_str("#define TST_HAS_RTP 1\n");
    }
    // udp / tcp / hls / rist transports (all default-off).
    if std::env::var("CARGO_FEATURE_UDP").is_ok() {
        defines.push_str("#define TST_HAS_UDP 1\n");
    }
    if std::env::var("CARGO_FEATURE_TCP").is_ok() {
        defines.push_str("#define TST_HAS_TCP 1\n");
    }
    if std::env::var("CARGO_FEATURE_HLS").is_ok() {
        defines.push_str("#define TST_HAS_HLS 1\n");
    }
    if std::env::var("CARGO_FEATURE_RIST").is_ok() {
        defines.push_str("#define TST_HAS_RIST 1\n");
    }
    if defines.is_empty() {
        return; // nothing to inject
    }

    let content = std::fs::read_to_string(header_path).expect("read tstrans.h for feature inject");
    // Insert after the `#define TSTRANS_H` include-guard line.
    let needle = "#define TSTRANS_H";
    let insert_pos = content.find(needle).map(|p| {
        p + content[p..]
            .find('\n')
            .map(|n| n + 1)
            .unwrap_or(needle.len())
    });
    let patched = match insert_pos {
        Some(pos) => {
            let mut out = String::with_capacity(content.len() + defines.len() + 1);
            out.push_str(&content[..pos]);
            out.push('\n');
            out.push_str(&defines);
            out.push_str(&content[pos..]);
            out
        }
        None => {
            // Include guard not found — prepend defines at top as fallback.
            format!("{defines}\n{content}")
        }
    };
    std::fs::write(header_path, patched).expect("write tstrans.h with feature defines");
}

/// Rewrite the cbindgen-generated header in place, inserting domain-
/// grouping section-divider comments before each block of function
/// declarations matching a known prefix.
///
/// Keys on symbol-name prefix (not source-file location), so a source-tree
/// reorg doesn't affect the grouping. Section order in the header follows
/// the table below.
///
/// 7 required domain sections (always emit) + 3 conditional sections
/// (emit only when their bucket is non-empty):
///
/// Required:
/// - INTROSPECTION: version / last-error / clear
/// - MUX SENDER:    tst_mux_sender / tst_managed_mux_sender / tst_mux_config / tst_muxer
/// - TS SENDER:     tst_sender / tst_managed_sender
/// - RAW SENDER:    tst_raw_sender / tst_managed_raw_sender
/// - DEMUX RECEIVER: tst_demux_receiver / tst_managed_demux_receiver / tst_demux_config
/// - TS RECEIVER:   tst_receiver / tst_managed_receiver
/// - RAW RECEIVER:  tst_raw_receiver / tst_managed_raw_receiver
///
/// Conditional (emit only when at least one symbol matched):
/// - KLV:           tst_st0601_ (ST 0601 KLV decode surface)
/// - LIFETIME:      *_close / *_cancel / *_free (catch-all cleanup)
/// - OTHER:         catch-all safety net
fn add_section_dividers(header_path: &std::path::Path) {
    let original = std::fs::read_to_string(header_path).expect("read tstrans.h");

    // (prefix patterns, section name) in match-order. First match wins.
    // Order: INTROSPECTION first (most-specific tst_get_*); domain sections
    // in sender-then-receiver order matching the README crate-graph; LIFETIME
    // is a catch-all suffix matcher; OTHER catches anything unclassified.
    let sections: &[(&[&str], &str)] = &[
        (
            &[
                "tst_get_version_",
                "tst_get_abi_version_",
                "tst_get_last_error",
                "tst_clear_last_error",
            ],
            "INTROSPECTION",
        ),
        (
            &[
                "tst_mux_sender_",
                "tst_managed_mux_sender_",
                "tst_mux_config_",
                "tst_muxer_",
            ],
            "MUX SENDER",
        ),
        (&["tst_sender_", "tst_managed_sender_"], "TS SENDER"),
        (
            &["tst_raw_sender_", "tst_managed_raw_sender_"],
            "RAW SENDER",
        ),
        (
            &[
                "tst_demux_receiver_",
                "tst_managed_demux_receiver_",
                "tst_demux_config_",
                "tst_demuxer_",
            ],
            "DEMUX RECEIVER",
        ),
        (&["tst_receiver_", "tst_managed_receiver_"], "TS RECEIVER"),
        (
            &["tst_raw_receiver_", "tst_managed_raw_receiver_"],
            "RAW RECEIVER",
        ),
        (&["tst_st0601_"], "KLV"),
    ];

    // Emission order. Required sections always emit; KLV, LIFETIME, and
    // OTHER emit only when non-empty.
    const REQUIRED_ORDER: &[&str] = &[
        "INTROSPECTION",
        "MUX SENDER",
        "TS SENDER",
        "RAW SENDER",
        "DEMUX RECEIVER",
        "TS RECEIVER",
        "RAW RECEIVER",
    ];
    const CONDITIONAL_ORDER: &[&str] = &["KLV", "LIFETIME", "OTHER"];

    // Pass 1: chunk the input into (header-bytes, [(section, chunk-bytes)],
    // trailer-bytes). Header = lines before the first function declaration
    // (includes, typedefs, opening `extern "C" {`); trailer = lines after
    // the last function declaration (closing `}`, `#endif`, ABI asserts).
    // A chunk is the doc-comment + attribute block that precedes a function
    // declaration, plus the declaration line(s) up to and including the
    // line that terminates the declaration with `;` (cbindgen wraps long
    // parameter lists across multiple lines; continuation lines must travel
    // with the symbol-bearing line).
    let mut header_bytes = String::new();
    let mut trailer_bytes = String::new();
    let mut chunks: Vec<(&'static str, String)> = Vec::new();
    let mut pending: String = String::new();
    let lines: Vec<&str> = original.lines().collect();
    let mut saw_first_chunk = false;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let symbol = extract_function_symbol(line);
        if let Some(sym) = symbol {
            // Any trailer-collected bytes were a misclassification — pre-decl
            // non-prelude lines belong to header. Move them back.
            if !trailer_bytes.is_empty() {
                header_bytes.push_str(&trailer_bytes);
                trailer_bytes.clear();
            }
            // Flush any buffered doc/attr lines plus this declaration line
            // (and any continuation lines until the `;` terminator) as one
            // chunk classified by `sym`.
            //
            // cbindgen 0.29.x emits single-line declarations with one
            // leading space (e.g. ` int tst_foo(void);`) while multi-line
            // declarations are emitted at column 0. Strip the single
            // leading space here so all declarations start at column 0.
            let section = classify_symbol(sym, sections);
            let mut chunk = std::mem::take(&mut pending);
            chunk.push_str(line.strip_prefix(' ').unwrap_or(line));
            chunk.push('\n');
            // Absorb continuation lines for multi-line declarations.
            while !lines[i].trim_end().ends_with(';') {
                i += 1;
                if i >= lines.len() {
                    break;
                }
                chunk.push_str(lines[i]);
                chunk.push('\n');
            }
            // Keep a feature-guard close (`#endif`) attached to the
            // declaration it guards, so reordering this chunk into its
            // section carries the guard along. Only a bare `#endif`
            // qualifies — the cpp-compat `#endif // __cplusplus` and the
            // ABI-assert `#endif` stay in the prelude/trailer. The matching
            // `#if defined(TST_HAS_*)` open is buffered into `pending` by the
            // branch below, so the chunk is a self-contained guarded unit.
            // Without this, default-OFF feature guards (udp/tcp/hls/rist) get
            // detached during reordering and a stray open guard wraps the
            // whole section block — silently excluding every declaration in a
            // default build (the guard macro is 0). Latent for default-ON
            // srt/rtp (guard evaluates 1); fatal for default-OFF features.
            if i + 1 < lines.len() && lines[i + 1].trim() == "#endif" {
                i += 1;
                chunk.push_str(lines[i]);
                chunk.push('\n');
            }
            chunks.push((section, chunk));
            saw_first_chunk = true;
        } else if is_chunk_prelude_line(line) {
            // Doc-comment, attribute, or blank-line-immediately-before-decl:
            // buffer it; it will travel with the next declaration (or be
            // flushed to header/trailer if no declaration follows).
            pending.push_str(line);
            pending.push('\n');
        } else if line.trim_start().starts_with("#if") && line.contains("TST_HAS_") {
            // Feature-guard open (`#if defined(TST_HAS_X)`): buffer like a
            // prelude so it travels with the next declaration chunk and stays
            // paired with its `#endif` (absorbed above) through section
            // reordering. See the `#endif` comment above for why detaching it
            // breaks default-OFF feature builds.
            pending.push_str(line);
            pending.push('\n');
        } else {
            // Non-prelude, non-decl line. Before the first function chunk
            // these belong to the header (includes, typedefs, opening
            // `extern "C" {`); after the first chunk they belong to the
            // trailer (closing `}`, `#endif`, ABI asserts). The trailer
            // gets reclassified back to header if another function chunk
            // appears later.
            let bucket = if saw_first_chunk {
                &mut trailer_bytes
            } else {
                &mut header_bytes
            };
            if !pending.is_empty() {
                bucket.push_str(&pending);
                pending.clear();
            }
            bucket.push_str(line);
            bucket.push('\n');
        }
        i += 1;
    }
    // Flush any trailing pending (cbindgen's output usually ends in the
    // trailer block, not a doc comment — but be defensive).
    if !pending.is_empty() {
        if saw_first_chunk {
            trailer_bytes.push_str(&pending);
        } else {
            header_bytes.push_str(&pending);
        }
    }

    // Pass 2: emit header verbatim, then sections in order, then trailer.
    let mut out = String::with_capacity(original.len() + 1024);
    out.push_str(&header_bytes);

    let emit_section = |out: &mut String, section: &str, chunks: &[(&str, String)]| {
        let matching: Vec<&String> = chunks
            .iter()
            .filter_map(|(s, c)| if *s == section { Some(c) } else { None })
            .collect();
        if matching.is_empty() {
            return;
        }
        out.push_str(&format!(
            "\n// ─── {} {}\n",
            section,
            "─".repeat(60usize.saturating_sub(section.len() + 6))
        ));
        for chunk in matching {
            out.push_str(chunk);
        }
    };

    for section in REQUIRED_ORDER {
        emit_section(&mut out, section, &chunks);
    }
    for section in CONDITIONAL_ORDER {
        emit_section(&mut out, section, &chunks);
    }

    out.push_str(&trailer_bytes);

    std::fs::write(header_path, out).expect("write tstrans.h with dividers");
}

/// Returns true if a line should be buffered as part of the doc/attribute
/// prelude immediately preceding a function declaration. Doxy block lines
/// (`/**`, ` *`, ` */`), single-line comments (`//`), and blank lines all
/// qualify. The chunker flushes pending prelude on either a recognized
/// declaration (-> attach to that chunk) or a non-prelude/non-decl line
/// (-> attach to header).
fn is_chunk_prelude_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("/**")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
        || trimmed.starts_with("//")
        || trimmed.is_empty()
}

/// Extract a leading `tst_<word>` symbol from a function-declaration line.
/// Returns None for lines that don't look like a function declaration.
///
/// Filters out doc-comment continuation lines (those starting with ` *` or
/// `/*`) — cbindgen emits doxygen comments that contain backtick-wrapped
/// references like `` `tst_get_last_error()` ``, which would otherwise
/// false-match here and cause spurious section dividers to land inside
/// comment blocks.
fn extract_function_symbol(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('*') || trimmed.starts_with("/*") || trimmed.starts_with("//") {
        return None;
    }
    let paren = line.find('(')?;
    let head = &line[..paren];
    let tst_start = head.rfind("tst_")?;
    // Reject if `tst_` is preceded by a backtick (inline-code in a doc
    // comment) or any other non-whitespace, non-start character that
    // signals this isn't a declaration prefix.
    if tst_start > 0 {
        let prev = head.as_bytes()[tst_start - 1];
        if !(prev == b' ' || prev == b'\t' || prev == b'*') {
            return None;
        }
    }
    let symbol = &head[tst_start..];
    if symbol
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        Some(symbol)
    } else {
        None
    }
}

/// Classify a `tst_*` symbol into one of the 7 required + 2 conditional
/// sections. Match order = first match wins. LIFETIME and OTHER are tried
/// only if no domain section matches.
fn classify_symbol(symbol: &str, sections: &[(&[&str], &str)]) -> &'static str {
    for (prefixes, name) in sections {
        for prefix in *prefixes {
            if symbol.starts_with(prefix) {
                return match *name {
                    "INTROSPECTION" => "INTROSPECTION",
                    "MUX SENDER" => "MUX SENDER",
                    "TS SENDER" => "TS SENDER",
                    "RAW SENDER" => "RAW SENDER",
                    "DEMUX RECEIVER" => "DEMUX RECEIVER",
                    "TS RECEIVER" => "TS RECEIVER",
                    "RAW RECEIVER" => "RAW RECEIVER",
                    "KLV" => "KLV",
                    _ => "OTHER",
                };
            }
        }
    }
    if symbol.ends_with("_close") || symbol.ends_with("_cancel") || symbol.ends_with("_free") {
        "LIFETIME"
    } else {
        "OTHER"
    }
}
