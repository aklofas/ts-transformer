//! Verifies that the committed header matches what cbindgen would emit
//! against the current source tree, plus the build.rs post-process step
//! that inserts domain-grouping section dividers.
//! Drift indicates a forgotten regenerate-and-commit step.

// The committed header is generated with `srt` + `rtp` on (cbindgen emits
// every transport's items guarded by `#if defined(TST_HAS_*)` regardless of
// build features, but the top-level `#define TST_HAS_SRT 1` / `TST_HAS_RTP 1`
// injected by build.rs only appear when those features are active). All six
// transports are now opt-in, so regenerate with
// `cargo build -p tst-c --features srt,rtp`. The drift assertion only matches
// cbindgen output when exactly `srt` + `rtp` are on (under all-features the
// injected defines would also include udp/tcp/hls/rist = 1). Gate the test to
// that flavor; the per-flavor `tst-c feature matrix` CI jobs cover
// compile-level cfg-leak detection via
// `scripts/check/c/header-conditional-sections.sh`.
#![cfg(all(feature = "srt", feature = "rtp"))]

use std::fs;
use std::path::PathBuf;

#[test]
fn committed_header_matches_cbindgen_output() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let committed = PathBuf::from(manifest_dir).join("include/tstrans.h");
    let expected = fs::read_to_string(&committed).expect("read committed tstrans.h");

    let config = cbindgen::Config::from_file(format!("{manifest_dir}/cbindgen.toml"))
        .expect("read cbindgen.toml");
    // The `#[no_mangle]` entry points live in the embeddable `tst-c-core`
    // rlib (this crate re-exports them via `pub use tst_c_core::*`), so
    // cbindgen scans the core crate — same as build.rs.
    let core_dir = PathBuf::from(manifest_dir).join("core");
    let generated = cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&core_dir)
        .generate()
        .expect("cbindgen generate");
    let mut buf = Vec::new();
    generated.write(&mut buf);
    let generated = String::from_utf8(buf).expect("cbindgen output is utf-8");

    // Mirror the build.rs post-process so this test compares apples to
    // apples (the committed header is cbindgen output + feature-define
    // injection + section dividers).
    let generated = inject_feature_defines(&generated);
    let generated = add_section_dividers(&generated);

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

/// Mirror of `build.rs::inject_feature_defines`. Uses `cfg!(feature = ...)`
/// to detect the same feature set the build was compiled with (the test
/// binary inherits the feature set; CARGO_FEATURE_* env vars only exist
/// in build-script execution, not at test runtime).
fn inject_feature_defines(content: &str) -> String {
    let mut defines = String::new();
    if cfg!(feature = "srt") {
        defines.push_str("#define TST_HAS_SRT 1\n");
    }
    if cfg!(feature = "rtp") {
        defines.push_str("#define TST_HAS_RTP 1\n");
    }
    if defines.is_empty() {
        return content.to_string();
    }
    let needle = "#define TSTRANS_H";
    let insert_pos = content.find(needle).map(|p| {
        p + content[p..]
            .find('\n')
            .map(|n| n + 1)
            .unwrap_or(needle.len())
    });
    match insert_pos {
        Some(pos) => {
            let mut out = String::with_capacity(content.len() + defines.len() + 1);
            out.push_str(&content[..pos]);
            out.push('\n');
            out.push_str(&defines);
            out.push_str(&content[pos..]);
            out
        }
        None => format!("{defines}\n{content}"),
    }
}

/// Mirror of `build.rs::add_section_dividers`. Kept in lock-step with
/// build.rs by convention (if you change one, change the other). Both
/// duplicate the small helper logic rather than depending on a shared
/// module — build.rs runs before the crate compiles, so it can't import
/// from a `tst_c::` path.
///
/// The `strip_prefix(' ')` call below mirrors the leading-space strip
/// in build.rs — cbindgen 0.29.x emits
/// single-line declarations with one leading space; we strip it here
/// so the test compares the same normalised form that build.rs produces.
fn add_section_dividers(original: &str) -> String {
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

    let mut header_bytes = String::new();
    let mut trailer_bytes = String::new();
    let mut chunks: Vec<(&'static str, String)> = Vec::new();
    let mut pending = String::new();
    let lines: Vec<&str> = original.lines().collect();
    let mut saw_first_chunk = false;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let symbol = extract_function_symbol(line);
        if let Some(sym) = symbol {
            if !trailer_bytes.is_empty() {
                header_bytes.push_str(&trailer_bytes);
                trailer_bytes.clear();
            }
            let section = classify_symbol(sym, sections);
            let mut chunk = std::mem::take(&mut pending);
            // Strip one leading space — cbindgen 0.29.x emits single-line
            // declarations with a leading space; build.rs normalises them.
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
            // Keep a feature-guard close attached to its declaration (see
            // build.rs for the rationale — default-OFF features break
            // otherwise). Bare `#endif` only. Lock-step with build.rs.
            if i + 1 < lines.len() && lines[i + 1].trim() == "#endif" {
                i += 1;
                chunk.push_str(lines[i]);
                chunk.push('\n');
            }
            chunks.push((section, chunk));
            saw_first_chunk = true;
        } else if is_chunk_prelude_line(line) {
            pending.push_str(line);
            pending.push('\n');
        } else if line.trim_start().starts_with("#if") && line.contains("TST_HAS_") {
            // Feature-guard open — buffer so it travels with the next
            // declaration chunk (lock-step with build.rs).
            pending.push_str(line);
            pending.push('\n');
        } else {
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
    if !pending.is_empty() {
        if saw_first_chunk {
            trailer_bytes.push_str(&pending);
        } else {
            header_bytes.push_str(&pending);
        }
    }

    let mut out = String::with_capacity(original.len() + 1024);
    out.push_str(&header_bytes);

    let emit_section = |out: &mut String, section: &str, chunks: &[(&'static str, String)]| {
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

    out
}

fn is_chunk_prelude_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("/**")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
        || trimmed.starts_with("//")
        || trimmed.is_empty()
}

fn extract_function_symbol(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('*') || trimmed.starts_with("/*") || trimmed.starts_with("//") {
        return None;
    }
    let paren = line.find('(')?;
    let head = &line[..paren];
    let tst_start = head.rfind("tst_")?;
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
