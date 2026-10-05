fn main() {
    pyo3_build_config::add_extension_module_link_args();

    // Dual-mbedTLS coexistence. srt-sys and rist-sys each build
    // their OWN static copy of the SAME shared source tree
    // (`crates/mbedtls-src/vendor/mbedtls`, reached via
    // `tstrans_mbedtls_src::source_dir()`) — rist-sys points librist's meson
    // at that build via CMAKE_PREFIX_PATH rather than letting librist bundle
    // its own `contrib/mbedtls`. When BOTH the `srt` and `rist` features are
    // active, those two independent static builds export the same
    // `mbedtls_*` symbols, so the extension module links two copies with
    // duplicate symbols. On Linux the default linker errors (`multiple
    // definition of mbedtls_sha256_init`). `-Wl,--allow-multiple-definition`
    // collapses every reference onto the first definition (srt-sys's
    // build), so the extension links AND both libraries share one
    // consistent mbedTLS at runtime (same fix + rationale as
    // bindings/c/build.rs; the clean fix — one shared BUILD, not just one
    // shared source tree — is the rist-sys v2 follow-up). Scoped to the
    // srt+rist combo so single-transport builds keep strict
    // duplicate-symbol checking.
    #[cfg(target_os = "linux")]
    {
        if std::env::var("CARGO_FEATURE_SRT").is_ok() && std::env::var("CARGO_FEATURE_RIST").is_ok()
        {
            println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
        }
    }

    // MSVC equivalent of the Linux block above: srt-sys and rist-sys each
    // build their own static copy of the same shared
    // `crates/mbedtls-src/vendor/mbedtls` source tree, so two byte-identical
    // static copies export duplicate `mbedtls_*` symbols and link.exe errors
    // (LNK2005/LNK1169) without an override. `/FORCE:MULTIPLE` collapses onto
    // the first definition, the same outcome `--allow-multiple-definition`
    // gives on Linux. Safe only because both copies are the identical 3.6.x
    // version.
    #[cfg(target_os = "windows")]
    {
        if std::env::var("CARGO_FEATURE_SRT").is_ok() && std::env::var("CARGO_FEATURE_RIST").is_ok()
        {
            println!("cargo:rustc-link-arg=/FORCE:MULTIPLE");
        }
    }
}
