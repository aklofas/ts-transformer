fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_RIST_SOURCE_FIXUPS");
    println!("cargo::rustc-check-cfg=cfg(librist_source_fixups)");
    // Set by rist-sys only when it built the vendored librist with its source
    // fixups applied; a system librist found through pkg-config has none.
    if std::env::var_os("DEP_RIST_SOURCE_FIXUPS").is_some() {
        println!("cargo:rustc-cfg=librist_source_fixups");
    }
}
