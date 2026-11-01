fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_SRT_SOURCE_FIXUPS");
    println!("cargo::rustc-check-cfg=cfg(libsrt_source_fixups)");
    // Set by srt-sys only when it built the vendored libsrt with its source
    // fixups applied; a system libsrt found through pkg-config has none.
    if std::env::var_os("DEP_SRT_SOURCE_FIXUPS").is_some() {
        println!("cargo:rustc-cfg=libsrt_source_fixups");
    }
}
