//! Integration tests for the `corpus` module family.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};
use tst_interop::corpus::Sha256Hex;
use tst_interop::fixtures::{AuSizeMode, KlvSet};

#[path = "corpus/annotations.rs"]
mod annotations;
#[path = "corpus/digest.rs"]
mod digest;
#[path = "corpus/inventory.rs"]
mod inventory;
#[path = "corpus/invariants.rs"]
mod invariants;
#[path = "corpus/pace.rs"]
mod pace;
#[path = "corpus/shape.rs"]
mod shape;
#[path = "corpus/subset.rs"]
mod subset;
#[path = "corpus/sweep.rs"]
mod sweep;

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

/// The SHA-256 of `bytes`, as the corpus tool prints it.
pub fn sha_of(bytes: &[u8]) -> Sha256Hex {
    Sha256Hex(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Rewrites every packet of `bytes` so its PCR-bearing packets carry a PCR
/// base offset by `shift` ticks and sit on `new_pid`; non-PCR packets are dropped.
pub fn pcr_only_packets(bytes: &[u8], from_pid: u16, new_pid: u16, shift: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for pkt in bytes.chunks_exact(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pid != from_pid || pkt[3] & 0x20 == 0 || pkt[4] < 7 || pkt[5] & 0x10 == 0 {
            continue;
        }
        let mut p = pkt.to_vec();
        p[1] = (p[1] & 0xE0) | (new_pid >> 8) as u8;
        p[2] = new_pid as u8;
        let base = (u64::from(p[6]) << 25)
            | (u64::from(p[7]) << 17)
            | (u64::from(p[8]) << 9)
            | (u64::from(p[9]) << 1)
            | u64::from(p[10] >> 7);
        let base = (base + shift) & ((1 << 33) - 1);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = (p[10] & 0x7F) | ((base & 1) as u8) << 7;
        out.extend_from_slice(&p);
    }
    out
}
