//! Integration tests for the `corpus` module family.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use tst_interop::fixtures::{AuSizeMode, KlvSet};

#[path = "corpus/inventory.rs"]
mod inventory;
#[path = "corpus/shape.rs"]
mod shape;

/// A fresh, empty directory unique to this test process and `name`.
pub fn temp_dir_named(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tst-interop-corpus-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The bytes of a synthetic capture generated from the named profile.
pub fn gen_profile_bytes(profile: &str, seconds: f64) -> Vec<u8> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "tst-corpus-gen-{profile}-{}-{}.ts",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let p = tst_interop::profiles::by_name(profile).unwrap();
    tst_interop::r#gen::run(p, seconds, &path, KlvSet::Compact, 0, AuSizeMode::Compact).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}
