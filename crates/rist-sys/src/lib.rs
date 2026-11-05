//! Raw FFI bindings to VideoLAN librist 0.2.x.
//!
//! This is a `-sys` crate: every type and function maps directly to a librist
//! symbol exposed via `RIST_API`. The safe wrapper lives in `tst-rist`.
//!
//! ## Build modes
//!
//! - **system mode** (default attempt): `pkg-config librist >= 0.2.20` → link
//!   against distro-installed librist. Currently rare on common Linux distros
//!   (Debian ships `librist4` runtime-only without a `.pc` file), so this path
//!   typically falls through to vendored.
//! - **vendored mode**: forced via `RIST_FORCE_VENDORED=1` (or when pkg-config
//!   fails) → compile `vendor/librist` via meson + ninja, link statically.
//! - **encryption**: enabled via the `mbedtls` cargo feature (default-on) —
//!   builds librist with `builtin_mbedtls=false`, linking the shared vendored
//!   mbedTLS 3.6.x from `tstrans-mbedtls-src` instead of librist's own bundled
//!   (older) contrib/mbedtls copy.
//!
//! ## Build prerequisites (vendored mode)
//!
//! - `meson` ≥ 0.51 (Debian: `apt install meson`; macOS: `brew install meson`)
//! - `ninja` (Debian: `apt install ninja-build`; macOS: `brew install ninja`)
//!
//! Cold builds take 1-3 min; warm rebuilds are seconds.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(rustdoc::broken_intra_doc_links)] // bindgen-emitted doc comments contain C function refs

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
