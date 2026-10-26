use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sanitizer instrumentation for the vendored native builds.
///
/// `TST_NATIVE_SANITIZER=address|thread` (set by the sanitizers workflow's
/// `*-native` jobs) returns the compiler flags that instrument every
/// vendored C object this script builds, so the static libs match the
/// `-Z sanitizer=<x>` instrumentation of the Rust code they link into.
/// Unset/empty returns `None` — that path is byte-identical to a build
/// without this hook. Unknown values fail the build (fail-closed, matching
/// the embedded-gate convention) instead of silently producing
/// uninstrumented libs.
///
/// The SAME variable gates srt-sys and rist-sys — KEEP IN SYNC with the
/// twin helper in srt-sys/build.rs. A downstream artifact enabling both
/// `srt` and `rist` links two static builds of the same vendor/mbedtls
/// sources, collapsed by the linker first-definition-wins; that collapse
/// is sound only while both builds carry identical flags. One shared
/// variable makes a sanitized/unsanitized mix unrepresentable, which is
/// why there is deliberately no per-crate override.
///
/// The instrumented objects resolve their `__asan_*`/`__tsan_*` references
/// against the LLVM runtime that rustc links into the test binary, so
/// builds with this set need `CC=clang CXX=clang++` — gcc pairs
/// `-fsanitize` objects with libgcc's incompatible runtime. Not enforced
/// (clang binary names vary across distros); we warn when `CC` looks wrong.
fn native_sanitizer_cflags() -> Option<String> {
    let value = match env::var("TST_NATIVE_SANITIZER") {
        Err(env::VarError::NotPresent) => return None,
        Err(e) => panic!("TST_NATIVE_SANITIZER is not valid UTF-8: {e}"),
        Ok(v) if v.is_empty() => return None,
        Ok(v) => v,
    };
    match value.as_str() {
        "address" | "thread" => {}
        other => panic!(
            "Unsupported TST_NATIVE_SANITIZER value `{other}` \
             (expected `address` or `thread`)."
        ),
    }
    let cc = env::var("CC").unwrap_or_default();
    let cxx = env::var("CXX").unwrap_or_default();
    if !cc.contains("clang") || !cxx.contains("clang") {
        println!(
            "cargo:warning=rist-sys: TST_NATIVE_SANITIZER={value} but CC={} CXX={} — \
             sanitized native builds need CC=clang CXX=clang++ so the native \
             objects match the LLVM sanitizer runtime rustc links (librist and \
             mbedTLS are pure C, but the twin srt-sys helper builds C++ and \
             the two warnings stay in sync)",
            if cc.is_empty() { "<unset>" } else { &cc },
            if cxx.is_empty() { "<unset>" } else { &cxx }
        );
    }
    // -fno-omit-frame-pointer keeps sanitizer stack traces walkable;
    // -g gives symbolized C frames in reports.
    Some(format!("-fsanitize={value} -fno-omit-frame-pointer -g"))
}

/// Build the workspace-shared vendored mbedTLS (`vendor/mbedtls`, 3.6.x) to a
/// private install prefix and return that prefix. Mirrors
/// `srt-sys::build_mbedtls` so librist links the SAME mbedTLS version libsrt
/// does (see the `mbedtls` feature note in Cargo.toml). Only called when the
/// `mbedtls` feature is on + the vendored build path is taken.
fn build_mbedtls(sanitizer: Option<&str>) -> PathBuf {
    let mbedtls_dir = tstrans_mbedtls_src::source_dir();

    if !mbedtls_dir.join("CMakeLists.txt").exists() {
        panic!(
            "Vendored mbedTLS not found at {}. \
             Run `git submodule update --init --recursive` from the workspace root.",
            mbedtls_dir.display()
        );
    }

    // Hide mbedTLS from -Wall sweeps; we don't author this code. Sanitizer
    // flags ride the same define: an explicit CMAKE_C_FLAGS define makes the
    // cmake crate skip its own computed C flags (cflag() would be ignored),
    // so the full flag string must be assembled here.
    let mut c_flags = String::from("-w");
    if let Some(san) = sanitizer {
        c_flags.push(' ');
        c_flags.push_str(san);
    }

    let mut cfg = cmake::Config::new(&mbedtls_dir);
    cfg.define("ENABLE_PROGRAMS", "OFF")
        .define("ENABLE_TESTING", "OFF")
        .define("USE_SHARED_MBEDTLS_LIBRARY", "OFF")
        .define("USE_STATIC_MBEDTLS_LIBRARY", "ON")
        .define("MBEDTLS_FATAL_WARNINGS", "OFF")
        .define("CMAKE_C_FLAGS", &c_flags)
        // Build the static mbedTLS objects position-independent so they link
        // into the downstream cdylib (the tst-py wheel). Implicit on
        // Debian/Ubuntu (gcc defaults to PIE) but NOT on the RHEL/AlmaLinux
        // gcc-toolset used for manylinux wheels — without it the wheel link
        // fails with "relocation R_X86_64_32S ... recompile with -fPIC".
        // Mirrors srt-sys::build_mbedtls. No-op on MSVC.
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON");

    // On windows-msvc, Rust links the dynamic release UCRT (`/MD`) regardless
    // of the cargo profile, but cmake defaults to `/MDd` for a Debug build —
    // mixing the two trips LNK2038 (RuntimeLibrary mismatch). Pin mbedTLS to
    // the dynamic release CRT so it matches Rust and the cl-built librist
    // (which uses `-Db_vscrt=md`). No-op off MSVC.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cfg.define("CMAKE_POLICY_DEFAULT_CMP0091", "NEW")
            .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreadedDLL");
    }

    cfg.build()

    // (A former workaround here staged mbedTLS's private `entropy_poll.h`
    // into the install's public include dir because librist ≤ 0.2.17's
    // `src/crypto/random.c` carried a vestigial `#include
    // <mbedtls/entropy_poll.h>`. librist 0.2.18 removed that include
    // upstream — commit df07717 — so the staging is gone.)
}

/// Generate a meson cross-file for a Linux cross build so librist (+ its
/// bundled C deps) compile for `$TARGET`, not the build host.
///
/// meson — unlike the `cmake` crate srt-sys uses for libsrt/mbedTLS — does not
/// auto-detect the Rust target, so without a cross-file it builds for the host.
/// Returns `None` for native builds (`HOST == TARGET`) and non-Linux targets
/// (Windows wires its MSVC toolchain via env vars in `build_vendored`; macOS
/// wheels build natively). GNU cross binaries are `<triple>-<tool>` on `$PATH`
/// (the manylinux cross image puts them under `/usr/<triple>/bin`), matching
/// the `cc` crate's own cross-naming convention.
fn write_meson_cross_file() -> Option<PathBuf> {
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    if target.is_empty() || host == target {
        return None;
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return None;
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let endian = env::var("CARGO_CFG_TARGET_ENDIAN").unwrap_or_default();
    let content = format!(
        "[binaries]\n\
         c = '{target}-gcc'\n\
         cpp = '{target}-g++'\n\
         ar = '{target}-ar'\n\
         strip = '{target}-strip'\n\
         pkg-config = 'pkg-config'\n\
         cmake = 'cmake'\n\
         \n\
         [host_machine]\n\
         system = 'linux'\n\
         cpu_family = '{arch}'\n\
         cpu = '{arch}'\n\
         endian = '{endian}'\n",
    );
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("meson-cross.ini");
    std::fs::write(&path, content).expect("write meson cross-file");
    // Breadcrumb for cross-build forensics (lands in the build-script stderr
    // capture, `cargo build -vv`), deliberately NOT a `cargo:warning`.
    eprintln!("rist-sys: meson cross-file for {target} (cpu_family={arch})");
    Some(path)
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    // Version-bearing files of the vendored submodules — a submodule bump
    // otherwise leaves the fingerprint untouched and incremental local
    // builds silently reuse the previous librist/mbedTLS static libs.
    // Mirrors srt-sys; see the comment there.
    println!("cargo:rerun-if-changed=vendor/librist/meson.build");
    // The file `SOURCE_FIXUPS` patches: a bump that changes it must re-run the
    // fixup check even when meson.build is untouched.
    println!("cargo:rerun-if-changed=vendor/librist/src/rist.c");
    println!(
        "cargo:rerun-if-changed={}",
        tstrans_mbedtls_src::source_dir()
            .join("CMakeLists.txt")
            .display()
    );
    println!("cargo:rerun-if-env-changed=RIST_NO_PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=RIST_FORCE_VENDORED");
    // Without this, toggling the sanitizer between builds leaves cargo's
    // fingerprint untouched and a cached UNINSTRUMENTED librist.a/libmbed*.a
    // silently survives into a sanitized run — an invisible false-green.
    println!("cargo:rerun-if-env-changed=TST_NATIVE_SANITIZER");

    // Symbol hygiene (hiding librist's static exports from downstream cdylib
    // export tables) is done in each cdylib crate's own build.rs, not here:
    // `cargo:rustc-link-arg-cdylib` only applies to a cdylib target in the same
    // package and rist-sys is rlib-only, so emitting it here only printed a
    // warning (cargo#9562). See bindings/c/build.rs for the effective wiring.

    let force_vendored = env::var_os("RIST_FORCE_VENDORED").is_some();
    let no_pkg_config = env::var_os("RIST_NO_PKG_CONFIG").is_some();
    let feat_mbedtls = env::var_os("CARGO_FEATURE_MBEDTLS").is_some();

    let sanitizer = native_sanitizer_cflags();
    // Fail closed: a sanitizer request must not silently resolve to an
    // uninstrumented system librist through the pkg-config path.
    if sanitizer.is_some() && !(force_vendored || no_pkg_config) {
        panic!(
            "TST_NATIVE_SANITIZER is set but the build may resolve system \
             librist via pkg-config, which cannot be instrumented. Set \
             RIST_FORCE_VENDORED=1 so the sanitized vendored build is used."
        );
    }

    let include_paths: Vec<PathBuf> = if force_vendored || no_pkg_config {
        build_vendored(feat_mbedtls, sanitizer.as_deref())
    } else {
        match pkg_config::Config::new()
            .atleast_version("0.2.10")
            .probe("librist")
        {
            Ok(lib) => lib.include_paths,
            Err(_) => build_vendored(feat_mbedtls, sanitizer.as_deref()),
        }
    };

    // ===== Generate bindings =====
    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .ctypes_prefix("libc")
        .derive_debug(true)
        .derive_default(true)
        .derive_copy(true)
        .layout_tests(false)
        // Allowlist librist symbols + macros.
        .allowlist_function("rist_.*")
        .allowlist_function("librist_.*")
        .allowlist_type("rist_.*")
        .allowlist_var("RIST_.*")
        .allowlist_var("LIBRIST_.*");

    // librist's logging.h exposes `FILE *log_stream` inside
    // `rist_logging_settings`, so bindgen needs a `FILE` type in scope.
    // Substitute libc's FILE on Unix; let bindgen emit an opaque FILE on
    // Windows (libc doesn't export FILE under windows-msvc).
    let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    if target_family == "unix" {
        builder = builder
            .blocklist_type("FILE")
            .blocklist_type("__sFILE")
            .blocklist_type("fpos_t")
            .raw_line("use libc::FILE;");
    }

    for inc in &include_paths {
        builder = builder.clang_arg(format!("-I{}", inc.display()));
    }

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    builder
        .generate()
        .expect("Failed to generate librist bindings")
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Failed to write bindings.rs to OUT_DIR");
}

/// A source correction applied to the staged copy of `vendor/librist`, so the
/// submodule stays pinned at the upstream commit and untouched.
struct SourceFixup {
    /// Named in the panic when the fixup stops matching.
    name: &'static str,
    /// Path relative to the librist source root.
    file: &'static str,
    old: &'static str,
    new: &'static str,
}

const SOURCE_FIXUPS: &[SourceFixup] = &[SourceFixup {
    // `rist_receiver_peer_create` (Simple profile) dereferences the RTCP peer
    // it creates on port + 1 one statement before its null check, so a
    // failed RTCP bind (port + 1 already held) crashes the process. Moving
    // the assignment after the check makes that failure return -1.
    name: "rtcp-peer-null-check",
    file: "src/rist.c",
    old: "\t\tp_rtcp = rist_receiver_peer_insert_local(ctx, config);\n\
          \t\tp_rtcp->peer_ssrc = p->peer_ssrc;\n\
          \t\tif (!p_rtcp)\n\
          \t\t{\n\
          \t\t\tudpsocket_close(p->sd);\n\
          \t\t\tfree(p);\n\
          \t\t\treturn -1;\n\
          \t\t}\n",
    new: "\t\tp_rtcp = rist_receiver_peer_insert_local(ctx, config);\n\
          \t\tif (!p_rtcp)\n\
          \t\t{\n\
          \t\t\tudpsocket_close(p->sd);\n\
          \t\t\tfree(p);\n\
          \t\t\treturn -1;\n\
          \t\t}\n\
          \t\tp_rtcp->peer_ssrc = p->peer_ssrc;\n",
}];

/// Paths under `vendor/librist` that are not staged. KEEP IN SYNC with the
/// `vendor/librist/…` entries of Cargo.toml's `exclude` list (prefix
/// stripped), so a build from the git checkout compiles exactly the tree a
/// build from the published crate does. `.git` is a file (submodule) or
/// directory that the package never carries either.
const STAGE_SKIP: &[&str] = &[
    ".git",
    "docs",
    "test",
    "tools",
    "subprojects",
    ".gitlab-ci.yml",
    "contrib/mbedtls/include",
    "contrib/mbedtls/library",
    "contrib/mbedtls/LICENSE",
    "contrib/mbedtls/Makefile",
    "contrib/mbedtls/refresh_mbedtls_source.sh",
];

/// Copies the vendored tree to `staged`, skipping [`STAGE_SKIP`] and the
/// [`SOURCE_FIXUPS`] targets (which [`apply_source_fixups`] writes). A file is
/// written only when it is missing or its bytes differ, so a re-run keeps
/// mtimes and ninja rebuilds nothing. Nothing is ever deleted from `staged`.
fn stage_vendored_source(vendor_dir: &Path, staged: &Path) {
    fn copy_dir(src_root: &Path, src: &Path, dst_root: &Path) {
        let entries = std::fs::read_dir(src)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", src.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("Failed to read {}: {e}", src.display()));
            let path = entry.path();
            let rel = path.strip_prefix(src_root).unwrap();
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if STAGE_SKIP.contains(&rel_str.as_str())
                || SOURCE_FIXUPS.iter().any(|f| f.file == rel_str)
            {
                continue;
            }
            let dst = dst_root.join(rel);
            let file_type = entry
                .file_type()
                .unwrap_or_else(|e| panic!("Failed to stat {}: {e}", path.display()));
            if file_type.is_dir() {
                std::fs::create_dir_all(&dst)
                    .unwrap_or_else(|e| panic!("Failed to create {}: {e}", dst.display()));
                copy_dir(src_root, &path, dst_root);
            } else {
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                write_if_changed(&dst, &bytes);
            }
        }
    }
    std::fs::create_dir_all(staged)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", staged.display()));
    copy_dir(vendor_dir, vendor_dir, staged);
}

/// Writes each [`SOURCE_FIXUPS`] target into the staged tree with its fixup
/// applied. The text is read from the pristine `vendor_dir`, never from the
/// staged copy, so a re-run sees the upstream bytes and writes nothing new.
/// Panics unless each fixup's `old` text occurs exactly once, so a vendor
/// bump that changes the patched code fails the build instead of silently
/// shipping the bug or a double patch.
fn apply_source_fixups(vendor_dir: &Path, staged: &Path) {
    for fixup in SOURCE_FIXUPS {
        let src = vendor_dir.join(fixup.file);
        let text = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", src.display()));
        match text.matches(fixup.old).count() {
            1 => write_if_changed(
                &staged.join(fixup.file),
                text.replacen(fixup.old, fixup.new, 1).as_bytes(),
            ),
            0 if text.contains(fixup.new) => panic!(
                "rist-sys source fixup `{}` no longer matches {}: upstream now carries \
                 this fix: delete the fixup from SOURCE_FIXUPS in build.rs",
                fixup.name, fixup.file
            ),
            n => panic!(
                "rist-sys source fixup `{}` matched {n} times in {} (expected exactly \
                 once): refresh the fixup text against the bumped source",
                fixup.name, fixup.file
            ),
        }
    }
}

/// Writes `bytes` to `path` only when the file is missing or differs.
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return;
    }
    std::fs::write(path, bytes)
        .unwrap_or_else(|e| panic!("Failed to write {}: {e}", path.display()));
}

/// Build librist from `vendor/librist` via meson + ninja.
///
/// Unlike srt-sys (which uses cmake), librist is a meson project. We invoke
/// `meson setup` + `meson compile` via `std::process::Command`. Both `meson`
/// and `ninja` must be on `$PATH` (Debian: `apt install meson ninja-build`).
///
/// meson builds a staged copy under `OUT_DIR` (see [`SOURCE_FIXUPS`]), never
/// the submodule itself.
fn build_vendored(want_mbedtls: bool, sanitizer: Option<&str>) -> Vec<PathBuf> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor/librist");

    if !vendor_dir.join("meson.build").exists() {
        panic!(
            "Vendored librist not found at {}. \
             Run `git submodule update --init --recursive` from the workspace root.",
            vendor_dir.display()
        );
    }

    require_tool("meson");
    require_tool("ninja");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let build_dir = out_dir.join("librist-build");
    let source_dir = out_dir.join("librist-src");
    stage_vendored_source(&vendor_dir, &source_dir);
    apply_source_fixups(&vendor_dir, &source_dir);

    // ===== meson setup =====
    //
    // Args:
    //   --default-library=static  static lib only
    //   --buildtype=release       optimized build
    //   -Dbuilt_tools=false       skip CLI tools (faster build)
    //   -Dtest=false              skip test suite
    //   -Dbuiltin_cjson=true      bundle contrib/cjson (avoids system cjson dep)
    //   -Dbuiltin_lz4=true        bundle contrib/lz4 (librist ≥ 0.2.17 needs
    //                             LZ4 for Advanced Profile payload compression;
    //                             explicit builtin keeps the build free of a
    //                             system liblz4 dep on every platform. Must be
    //                             explicit: our -Dfallback_builtin=false turns
    //                             the would-be automatic builtin fallback into
    //                             a hard "lz4.h not found" meson error)
    //   -Dbuiltin_mbedtls=<bool>  bundle contrib/mbedtls when mbedtls feature is on
    //   -Duse_mbedtls=<bool>      enable/disable encryption entirely
    let mut args: Vec<String> = vec![
        "setup".into(),
        build_dir.to_string_lossy().into_owned(),
        source_dir.to_string_lossy().into_owned(),
        "--default-library=static".into(),
        "--buildtype=release".into(),
        "-Dbuilt_tools=false".into(),
        "-Dtest=false".into(),
        "-Dbuiltin_cjson=true".into(),
        "-Dbuiltin_lz4=true".into(),
    ];
    let mut meson_envs: Vec<(String, String)> = Vec::new();

    // Sanitize the librist objects when requested. meson's free-form
    // compile-args options shlex-split their value, so the multi-flag
    // string rides a single -Dc_args=. librist is pure C (no cpp_args
    // needed); c_link_args keeps meson's compiler sanity/feature probes
    // linking consistently with the instrumented objects. On a cached
    // build dir the `--reconfigure` path below picks up the changed
    // option values and ninja rebuilds every affected object.
    if let Some(san) = sanitizer {
        args.push(format!("-Dc_args={san}"));
        args.push(format!("-Dc_link_args={san}"));
    }

    // On Windows the Rust target is `x86_64-pc-windows-msvc`, so librist (and
    // its bundled contrib/mbedtls + cJSON) MUST be compiled with MSVC `cl` —
    // not the mingw `gcc` that meson would otherwise pick up from Strawberry
    // Perl on PATH. A mingw-built `librist.a` references mingw/winpthreads
    // runtime symbols (`__mingw_printf`, `___chkstk_ms`, `pthread_once`, ...)
    // that the MSVC linker can't resolve (LNK1120).
    //
    // We locate `cl` and its environment (INCLUDE/LIB/PATH) via the same MSVC
    // registry discovery the `cmake` crate uses for srt-sys, and inject it into
    // ONLY the meson/ninja subprocesses (see `run_cmd`). This deliberately does
    // NOT pollute the whole process env (the earlier `ilammy/msvc-dev-cmd`
    // approach did, which made bindgen's libclang pick up MSVC's bundled
    // clang headers and fail parsing `__m64` in `mmintrin.h`). With `cl`, MSVC's
    // `<time.h>` has no `clock_gettime`, so librist's `contrib/time-shim.c`
    // shim compiles cleanly (no redefinition) and its Windows-native threading
    // path needs no winpthreads — hence we do NOT enable `have_mingw_pthreads`.
    // `-Db_vscrt=md` matches Rust's dynamic UCRT (Rust links `/MD` even in
    // debug) so the CRTs don't clash at link time.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let target = env::var("TARGET").unwrap_or_default();
        let cl = cc::windows_registry::find_tool(&target, "cl.exe").unwrap_or_else(|| {
            panic!(
                "Could not locate MSVC `cl.exe` for target `{target}`. \
                 librist's meson build needs the MSVC toolchain on windows-msvc; \
                 ensure Visual Studio Build Tools (VC++) are installed."
            )
        });
        // Tool::get_envs() yields the INCLUDE/LIB/PATH (and friends) the MSVC
        // compiler needs; forward each into the meson subprocess env. For PATH
        // we APPEND the discovered MSVC dirs to the inherited PATH rather than
        // replacing it, so meson + ninja (installed on the original PATH) stay
        // findable by the `meson compile` step.
        for (k, v) in cl.get_envs() {
            let key = k.to_string_lossy().into_owned();
            let mut val = v.to_string_lossy().into_owned();
            if key.eq_ignore_ascii_case("path")
                && let Some(existing) = env::var_os(&key)
            {
                val.push(';');
                val.push_str(&existing.to_string_lossy());
            }
            meson_envs.push((key, val));
        }
        // Point meson at the discovered cl.exe explicitly so it doesn't fall
        // back to the mingw gcc that is also on PATH.
        meson_envs.push(("CC".to_string(), cl.path().to_string_lossy().into_owned()));
        meson_envs.push(("CXX".to_string(), cl.path().to_string_lossy().into_owned()));
        args.push("-Db_vscrt=md".into());
    }
    // On a Linux cross build (e.g. the aarch64 manylinux wheel, x86_64 host →
    // aarch64 target) meson must be handed a cross-file or it builds librist
    // for the HOST — producing x86_64 objects that fail the aarch64 link with
    // "File in wrong format" (EM: 62). srt-sys's mbedTLS uses the `cmake` crate,
    // which auto-cross-compiles from $TARGET; meson does not, hence this.
    if let Some(cross_file) = write_meson_cross_file() {
        args.push("--cross-file".into());
        args.push(cross_file.to_string_lossy().into_owned());
    }
    // When encryption is wanted, build the SHARED workspace vendor/mbedtls
    // (3.6.x) and point librist's meson at it so it links that `mbedcrypto`
    // (`-Dbuiltin_mbedtls=false`) instead of its bundled contrib/mbedtls — see
    // the `mbedtls` feature note in Cargo.toml.
    //
    // librist 0.2.18's `contrib/mbedtls/meson.build` resolves the external
    // mbedTLS via the CMAKE method FIRST (`dependency('MbedTLS', method:
    // 'cmake', modules: ['MbedTLS::mbedcrypto'])`), then a bare
    // `cc.find_library('mbedcrypto')`. PKG_CONFIG_PATH alone is NOT consulted
    // by the cmake method, so we export CMAKE_PREFIX_PATH pointing at our
    // mbedTLS install (which ships `lib/cmake/MbedTLS/MbedTLSConfig.cmake`).
    // Without this, both lookups miss and librist SILENTLY falls back to its
    // bundled contrib/mbedtls (2.28.x) headers — which, being < 3.0, skip the
    // `srp.c` `#include <mbedtls/compat-2.x.h>` shim and emit calls to the
    // removed 2.x `mbedtls_sha256_ret`/`*_starts_ret`/`*_update_ret`/
    // `*_finish_ret` symbols, breaking the link against our 3.6.x `mbedcrypto`.
    // `-Dfallback_builtin=false` turns that silent fallback into a hard meson
    // error (it sets librist's internal `required_library = true`), so a future
    // detection regression fails loudly instead of mis-linking.
    let mbedtls_prefix: Option<PathBuf> = if want_mbedtls {
        let prefix = build_mbedtls(sanitizer);
        let pc_dir = prefix.join("lib").join("pkgconfig");
        let pkg_path = match env::var("PKG_CONFIG_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}:{}", pc_dir.display(), existing)
            }
            _ => pc_dir.to_string_lossy().into_owned(),
        };
        meson_envs.push(("PKG_CONFIG_PATH".to_string(), pkg_path));
        let cmake_path = match env::var("CMAKE_PREFIX_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}:{}", prefix.display(), existing)
            }
            _ => prefix.to_string_lossy().into_owned(),
        };
        meson_envs.push(("CMAKE_PREFIX_PATH".to_string(), cmake_path));
        args.push("-Dbuiltin_mbedtls=false".into());
        args.push("-Dfallback_builtin=false".into());
        args.push("-Duse_mbedtls=true".into());
        Some(prefix)
    } else {
        args.push("-Duse_mbedtls=false".into());
        None
    };

    // If a previous build exists, reconfigure it instead of erroring. A
    // reconfigure can FAIL when the existing dir was configured against a
    // different vendored librist version — a cargo `target/` that predates a
    // submodule bump, either local-incremental or restored from the CI cargo
    // cache (whose key hashes Cargo.lock, which a submodule bump doesn't
    // touch). meson's cached state doesn't survive the source jump, so
    // self-heal: wipe the stale build dir and set up fresh. (First bitten by
    // the librist 0.2.16 -> 0.2.18 bump, locally and on the CI cache.)
    let reconfiguring = build_dir.join("build.ninja").exists();
    if reconfiguring {
        args[0] = "setup".into();
        args.insert(1, "--reconfigure".into());
    }

    if !try_run_cmd("meson", &args, &source_dir, &meson_envs) {
        if !reconfiguring {
            panic!(
                "Command `meson {}` failed (cwd={})",
                args.join(" "),
                source_dir.display()
            );
        }
        println!(
            "cargo:warning=rist-sys: meson reconfigure failed on a stale build dir \
             (likely a pre-bump cargo cache); wiping {} and retrying fresh",
            build_dir.display()
        );
        std::fs::remove_dir_all(&build_dir).unwrap_or_else(|e| {
            panic!(
                "Failed to remove stale meson build dir {}: {e}",
                build_dir.display()
            )
        });
        args.remove(1); // drop --reconfigure
        run_cmd("meson", &args, &source_dir, &meson_envs);
    }

    // ===== meson compile =====
    run_cmd(
        "meson",
        &[
            "compile".to_string(),
            "-C".to_string(),
            build_dir.to_string_lossy().into_owned(),
        ],
        &source_dir,
        &meson_envs,
    );

    // librist's meson outputs `librist.a` directly in builddir (with
    // --default-library=static). Headers stay in the staged source tree under
    // `include/`.
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.to_string_lossy()
    );
    println!("cargo:rustc-link-lib=static=rist");

    // With `-Dbuiltin_mbedtls=false`, librist.a does NOT bundle mbedTLS — its
    // `mbedtls_*` references resolve against the shared vendor/mbedtls 3.6.x
    // static libs we built above. Link them after librist.a (link order:
    // mbedtls -> mbedx509 -> mbedcrypto, matching srt-sys). The downstream
    // cdylib's `-Wl,--allow-multiple-definition` collapses these with libsrt's
    // identical 3.6.x copy.
    if let Some(prefix) = &mbedtls_prefix {
        // cmake's GNUInstallDirs puts the static libs in `<prefix>/lib` on
        // Debian/Ubuntu but `<prefix>/lib64` on RHEL/AlmaLinux (manylinux
        // wheel builds). Emit both search paths so the link succeeds
        // regardless of distro — a missing dir is silently ignored by the
        // linker. (Without lib64, manylinux builds failed with "could not
        // find native static library `mbedtls`".)
        println!(
            "cargo:rustc-link-search=native={}",
            prefix.join("lib").to_string_lossy()
        );
        println!(
            "cargo:rustc-link-search=native={}",
            prefix.join("lib64").to_string_lossy()
        );
        println!("cargo:rustc-link-lib=static=mbedtls");
        println!("cargo:rustc-link-lib=static=mbedx509");
        println!("cargo:rustc-link-lib=static=mbedcrypto");
    }

    // Platform link needs. librist is pure C; no C++ stdlib link required.
    // (The mbedTLS link line, when encryption is enabled, is emitted above —
    // the shared vendor/mbedtls static libs, since builtin_mbedtls=false.)
    if cfg!(target_os = "linux") {
        // Some librist sources call pthread fns; the link line needs -pthread.
        println!("cargo:rustc-link-lib=dylib=pthread");
    }

    // librist's `src/network.c` calls the Windows IP Helper API
    // (`GetAdaptersInfo`, in iphlpapi.dll) and Winsock (`ws2_32.dll`) — the
    // same system libs librist's own meson.build lists for Windows. Our static
    // link line doesn't inherit those, so emit them here or the cdylib link
    // fails with `unresolved external symbol __imp_GetAdaptersInfo` (LNK1120).
    // bcrypt covers mbedTLS's BCryptGenRandom entropy when encryption is on
    // (mirrors srt-sys).
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=dylib=iphlpapi");
        println!("cargo:rustc-link-lib=dylib=ws2_32");
        if want_mbedtls {
            println!("cargo:rustc-link-lib=dylib=bcrypt");
        }
    }

    // librist's build also generates `librist_config.h` (and vcs_version.h)
    // into <build_dir>/include/librist/. peer.h `#include`s this generated
    // header via a relative include (`#include "librist_config.h"`), so the
    // include path must reach the librist/ subdirectory directly.
    vec![
        source_dir.join("include"),
        build_dir.join("include").join("librist"),
        build_dir.join("include"),
    ]
}

fn require_tool(name: &str) {
    if which(name).is_none() {
        panic!(
            "Required build tool `{name}` not found on $PATH. \
             librist requires meson + ninja for the vendored build path. \
             Debian/Ubuntu: `sudo apt install meson ninja-build`. \
             macOS: `brew install meson ninja`. \
             Windows: `choco install meson ninja`."
        );
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    // On Windows an executable carries an extension (meson.exe, ninja.exe, and
    // meson is often a .cmd/.bat shim), so the bare `name` is rarely a file on
    // disk. Probe `name` plus each PATHEXT extension. On Unix the candidate
    // list is just the bare name, preserving the original behaviour.
    let mut names = vec![name.to_string()];
    if cfg!(windows) {
        let pathext = env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.BAT;.CMD".to_string());
        for ext in pathext.split(';').filter(|e| !e.is_empty()) {
            names.push(format!("{name}{ext}"));
        }
    }
    for dir in env::split_paths(&path) {
        for candidate_name in &names {
            let candidate = dir.join(candidate_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn run_cmd(prog: &str, args: &[String], cwd: &Path, envs: &[(String, String)]) {
    if !try_run_cmd(prog, args, cwd, envs) {
        panic!(
            "Command `{prog} {}` failed (cwd={})",
            args.join(" "),
            cwd.display()
        );
    }
}

/// Like [`run_cmd`] but reports failure instead of panicking (spawn errors —
/// tool not found at all — still panic).
fn try_run_cmd(prog: &str, args: &[String], cwd: &Path, envs: &[(String, String)]) -> bool {
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.status()
        .unwrap_or_else(|e| panic!("Failed to spawn `{prog}`: {e}"))
        .success()
}
