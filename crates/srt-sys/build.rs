use std::env;
use std::path::PathBuf;

/// Sanitizer instrumentation for the vendored native builds.
///
/// `TST_NATIVE_SANITIZER=address|thread` (set by the sanitizers workflow's
/// `*-native` jobs) returns the compiler flags that instrument every
/// vendored C/C++ object this script builds, so the static libs match the
/// `-Z sanitizer=<x>` instrumentation of the Rust code they link into.
/// Unset/empty returns `None` — that path is byte-identical to a build
/// without this hook. Unknown values fail the build (fail-closed, matching
/// the embedded-gate convention) instead of silently producing
/// uninstrumented libs.
///
/// The SAME variable gates srt-sys and rist-sys — KEEP IN SYNC with the
/// twin helper in rist-sys/build.rs. A downstream artifact enabling both
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
            "cargo:warning=srt-sys: TST_NATIVE_SANITIZER={value} but CC={} CXX={} — \
             sanitized native builds need CC=clang CXX=clang++ so the C/C++ \
             objects match the LLVM sanitizer runtime rustc links",
            if cc.is_empty() { "<unset>" } else { &cc },
            if cxx.is_empty() { "<unset>" } else { &cxx }
        );
    }
    // -fno-omit-frame-pointer keeps sanitizer stack traces walkable;
    // -g gives symbolized C frames in reports.
    Some(format!("-fsanitize={value} -fno-omit-frame-pointer -g"))
}

/// Apply Apple-iOS cross-compile settings to a cmake config when `target` is
/// an `*-apple-ios` / `*-apple-ios-sim` triple; a no-op on every other target.
///
/// libsrt's own CMakeLists keys its Darwin/iOS handling on
/// `CMAKE_SYSTEM_NAME MATCHES "iOS"` (vendor/srt/CMakeLists.txt), so setting the
/// system name explicitly is required for a correct iOS configure — the `cmake`
/// crate supplies the sysroot/arch for Apple targets but does not reliably set
/// the system name. Setting the sysroot/arch here too is idempotent with what
/// the crate emits (cmake takes the last `-D`), and pins the correct values.
///
/// The whole body is gated on `target.contains("apple-ios")`, so linux / macos
/// / windows builds are completely unaffected. The gate is on the TARGET triple,
/// not the host — Apple cross-compilation only actually works on a macOS host
/// with the Xcode iOS SDK, so we fail early with a clear message if an
/// `apple-ios` target is attempted from a non-Darwin host rather than letting
/// cmake fail obscurely on a missing SDK.
fn apply_apple_ios(cfg: &mut cmake::Config, target: &str) {
    if !target.contains("apple-ios") {
        return;
    }
    let host = env::var("HOST").unwrap_or_default();
    assert!(
        host.contains("apple-darwin"),
        "cross-compiling to {target} needs a macOS host with the Xcode iOS SDK \
         (HOST={host}); Apple cross-compilation cannot run on this platform"
    );
    // Simulator SDK for the `-sim` triples AND for `x86_64-apple-ios` (there is
    // no x86_64 iOS *device*, so that triple is a simulator); device SDK otherwise.
    let sysroot = if target.ends_with("-sim") || target.starts_with("x86_64-apple-ios") {
        "iphonesimulator"
    } else {
        "iphoneos"
    };
    // CMAKE_OSX_ARCHITECTURES uses Apple arch names — map Rust's `aarch64` → `arm64`;
    // `x86_64` is already the Apple name. Derived from the target, not hard-coded,
    // so future non-arm64 iOS slices configure correctly.
    let arch = match env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    };
    cfg.define("CMAKE_SYSTEM_NAME", "iOS")
        .define("CMAKE_OSX_ARCHITECTURES", arch.as_str())
        .define("CMAKE_OSX_SYSROOT", sysroot)
        // A conservative floor; raise via the build script if a consumer needs
        // a newer minimum. Bitcode is intentionally left off (removed from the
        // toolchain in Xcode 14+).
        .define("CMAKE_OSX_DEPLOYMENT_TARGET", "13.0")
        // In cross-compile mode (which CMAKE_SYSTEM_NAME triggers) CMake
        // re-roots find_library/find_path/find_package into the SDK sysroot
        // and, by default, IGNORES CMAKE_PREFIX_PATH — so libsrt's
        // `find_package(MbedTLS)` (CMakeLists.txt:394) can't see the vendored
        // mbedTLS we just built + pointed CMAKE_PREFIX_PATH at, and configure
        // fails "Could NOT find MbedTLS". `...MODE_*=BOTH` makes find_* search
        // the host prefix AND the sysroot, so our private mbedTLS is found.
        .define("CMAKE_FIND_ROOT_PATH_MODE_LIBRARY", "BOTH")
        .define("CMAKE_FIND_ROOT_PATH_MODE_INCLUDE", "BOTH")
        .define("CMAKE_FIND_ROOT_PATH_MODE_PACKAGE", "BOTH");
}

/// Build the vendored mbedTLS to a private install prefix.
/// Returns the install prefix path.
///
/// Only called when the `mbedtls` cargo feature is enabled.
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
        // Build the static mbedTLS objects position-independent so they can be
        // linked into the downstream cdylib (the tst-py wheel / libtstrans.so).
        // Debian/Ubuntu gcc defaults to PIE so this was implicit there, but the
        // RHEL/AlmaLinux gcc-toolset (manylinux wheel builds) does not — without
        // this the wheel link fails: "relocation R_X86_64_32S ... can not be
        // used when making a shared object; recompile with -fPIC". No-op on MSVC.
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON");
    apply_apple_ios(&mut cfg, &env::var("TARGET").unwrap_or_default());
    cfg.build()
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    // Version-bearing files of the vendored submodules. A submodule bump
    // otherwise leaves cargo's fingerprint untouched, so an incremental
    // local build silently reuses the PREVIOUS libsrt/mbedTLS static libs
    // (CI never sees this — fresh checkouts always rebuild). Tracking the
    // two top-level files (each carries the version) is enough to catch
    // every pin change without recursively statting the whole submodule.
    println!("cargo:rerun-if-changed=vendor/srt/CMakeLists.txt");
    println!(
        "cargo:rerun-if-changed={}",
        tstrans_mbedtls_src::source_dir()
            .join("CMakeLists.txt")
            .display()
    );
    println!("cargo:rerun-if-env-changed=SRT_NO_PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=SRT_FORCE_VENDORED");
    // Without this, toggling the sanitizer between builds leaves cargo's
    // fingerprint untouched and a cached UNINSTRUMENTED libsrt.a/libmbed*.a
    // silently survives into a sanitized run — an invisible false-green.
    println!("cargo:rerun-if-env-changed=TST_NATIVE_SANITIZER");

    // Symbol hygiene for downstream cdylib consumers is wired
    // in each cdylib crate's OWN build.rs, not here: a `cargo:rustc-link-arg-cdylib`
    // directive applies only to a cdylib target in the SAME package, and srt-sys
    // is rlib-only — so emitting it here would do nothing except print a per-build
    // warning ("does not contain a cdylib target", rust-lang/cargo#9562). The
    // effective `-Wl,--exclude-libs=ALL` (Linux) lives in bindings/c/build.rs
    // (alongside the macOS exported_symbols_list path); the "no srt_*/SRT_*
    // symbol leak in libtstrans.so" CI ratchet verifies it works.

    let force_vendored = env::var_os("SRT_FORCE_VENDORED").is_some();
    let no_pkg_config = env::var_os("SRT_NO_PKG_CONFIG").is_some();
    let want_mbedtls = env::var_os("CARGO_FEATURE_MBEDTLS").is_some();

    let sanitizer = native_sanitizer_cflags();
    // Fail closed: a sanitizer request must not silently resolve to an
    // uninstrumented system libsrt through the pkg-config path.
    if sanitizer.is_some() && !(force_vendored || no_pkg_config) {
        panic!(
            "TST_NATIVE_SANITIZER is set but the build may resolve system \
             libsrt via pkg-config, which cannot be instrumented. Set \
             SRT_FORCE_VENDORED=1 so the sanitized vendored build is used."
        );
    }

    let mbedtls_prefix: Option<PathBuf> = if want_mbedtls {
        Some(build_mbedtls(sanitizer.as_deref()))
    } else {
        None
    };

    let include_paths: Vec<PathBuf> = if force_vendored || no_pkg_config {
        build_vendored(mbedtls_prefix.as_ref(), sanitizer.as_deref())
    } else {
        match pkg_config::Config::new()
            .atleast_version("1.5.0")
            .probe("srt")
        {
            Ok(lib) => lib.include_paths,
            Err(_) => build_vendored(mbedtls_prefix.as_ref(), sanitizer.as_deref()),
        }
    };

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .allowlist_function("srt_.*")
        .allowlist_type("SRT_.*|SRTSOCKET|UDPSOCKET|CBytePerfMon")
        .allowlist_var("SRT_.*")
        .ctypes_prefix("libc")
        .derive_debug(true)
        .derive_default(true)
        .derive_copy(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    // The libc crate exposes `sockaddr` / `sockaddr_storage` on Unix
    // targets, so we blocklist bindgen's auto-generated copies and
    // substitute libc's — this lets consumer crates reuse the same
    // sockaddr types as Rust's std::net layer.
    //
    // On Windows (`*-pc-windows-msvc`), libc does NOT export these
    // symbols at the crate root (Win32 uses its own `SOCKADDR` types
    // from <ws2def.h>), so the substitution would generate an
    // unresolved-import compile error. Let bindgen emit its own
    // copies on Windows — consumers cast through raw pointers in
    // either case, so the type identity doesn't matter end-to-end.
    let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    if target_family == "unix" {
        builder = builder
            .blocklist_type("sockaddr.*")
            .raw_line("use libc::{sockaddr, sockaddr_storage};");
    }

    for path in &include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    builder
        .generate()
        .expect("Failed to generate bindings against libsrt headers")
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Failed to write bindings.rs to OUT_DIR");
}

/// Build the vendored libsrt source via cmake.
///
/// If `mbedtls_prefix` is `Some`, libsrt is configured with
/// `ENABLE_ENCRYPTION=ON` + `USE_ENCLIB=mbedtls` and uses the prefix
/// to locate mbedTLS via `find_package(MbedTLS)`.
///
/// Otherwise, libsrt is built with `ENABLE_ENCRYPTION=OFF`.
fn build_vendored(mbedtls_prefix: Option<&PathBuf>, sanitizer: Option<&str>) -> Vec<PathBuf> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor/srt");

    if !vendor_dir.join("CMakeLists.txt").exists() {
        panic!(
            "Vendored libsrt not found at {}. \
             Run `git submodule update --init --recursive` from the workspace root.",
            vendor_dir.display()
        );
    }

    let mut cfg = cmake::Config::new(&vendor_dir);
    cfg.define("ENABLE_APPS", "OFF")
        .define("ENABLE_SHARED", "OFF")
        .define("ENABLE_STATIC", "ON")
        .define("ENABLE_UNITTESTS", "OFF")
        // Heavy logging is ON by default in Debug builds and causes a C++
        // static-initialization-order fiasco: the CUDTException constructor
        // (called from a static CThreadError object) tries to log via aclog
        // before aclog and srt_logger_config are initialized.  Disabling it
        // avoids the SIGSEGV at process startup when libsrt is statically
        // linked.
        .define("ENABLE_HEAVY_LOGGING", "OFF");

    // MSVC requires explicit /EHsc to enable C++ exception unwind
    // semantics; gcc/clang have it on by default. libsrt's sources use
    // try/catch, so without this every catch site errors C4530 and
    // libsrt fails to build under windows-msvc.
    let target = env::var("TARGET").unwrap_or_default();
    if target.contains("msvc") {
        cfg.cxxflag("/EHsc");
    }

    // Sanitize the libsrt objects when requested. libsrt is C++ with C
    // entry shims — both languages need the flag. cflag()/cxxflag() append
    // to the C flags the cmake crate computes itself; that works here
    // because (unlike the mbedTLS config above) this config does not
    // define CMAKE_C(XX)_FLAGS explicitly.
    if let Some(san) = sanitizer {
        cfg.cflag(san);
        cfg.cxxflag(san);
    }

    match mbedtls_prefix {
        Some(prefix) => {
            cfg.define("ENABLE_ENCRYPTION", "ON")
                .define("USE_ENCLIB", "mbedtls")
                .define("CMAKE_PREFIX_PATH", prefix);
        }
        None => {
            cfg.define("ENABLE_ENCRYPTION", "OFF");
        }
    }

    // iOS cross-compile (macOS host only) — libsrt's CMakeLists needs
    // CMAKE_SYSTEM_NAME=iOS to take its Darwin path. No-op off apple-ios.
    apply_apple_ios(&mut cfg, &target);

    let dst = cfg.build();

    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-search=native={}/lib64", dst.display());

    // libsrt's CMakeLists names the static library `srt_static.lib` on
    // MSVC to avoid colliding with the shared-library import lib (also
    // named `srt.lib`); on every other platform it's `libsrt.a` /
    // `libsrt.so` (rustc strips the lib prefix when linking by name).
    // See vendor/srt/CMakeLists.txt L1169-1181.
    let srt_lib_name = if target.contains("msvc") {
        "srt_static"
    } else {
        "srt"
    };
    println!("cargo:rustc-link-lib=static={srt_lib_name}");

    // Link the mbedTLS static libs (libsrt's pkg-config file references them
    // but our static link line doesn't go through pkg-config).
    if let Some(prefix) = mbedtls_prefix {
        println!("cargo:rustc-link-search=native={}/lib", prefix.display());
        println!("cargo:rustc-link-search=native={}/lib64", prefix.display());
        // Order matters: libsrt -> libmbedtls -> libmbedx509 -> libmbedcrypto.
        println!("cargo:rustc-link-lib=static=mbedtls");
        println!("cargo:rustc-link-lib=static=mbedx509");
        println!("cargo:rustc-link-lib=static=mbedcrypto");

        // mbedTLS on Windows uses BCryptGenRandom from bcrypt.dll for
        // entropy collection (mbedtls_platform_entropy_poll calls into
        // it). On Linux it uses /dev/urandom and no extra link is
        // needed; on Windows we need bcrypt.lib (system import lib
        // shipped with the Windows SDK).
        if target.contains("windows") {
            println!("cargo:rustc-link-lib=dylib=bcrypt");
        }
    }

    // libsrt is C++ internally; link the C++ stdlib explicitly.
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=stdc++");
    } else if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=c++");
    }

    vec![dst.join("include")]
}
