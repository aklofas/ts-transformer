//! Output-directory trust and symlink-free file I/O (Unix enforcement).

#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use tst_core::publisher::Publisher;
use tst_hls::{HlsConfig, HlsError, HlsPublisher};

fn scratch(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hls-trust-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn private_dir(parent: &Path, name: &str) -> PathBuf {
    let d = parent.join(name);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn config(dir: &Path) -> HlsConfig {
    let mut c = HlsConfig::default();
    c.bind = "127.0.0.1:0".parse().unwrap();
    c.output_dir = dir.to_path_buf();
    c
}

fn ts_packets(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; 188 * n];
    for p in v.chunks_mut(188) {
        p[0] = 0x47;
    }
    v
}

#[test]
fn world_writable_output_dir_is_refused() {
    let root = scratch("mode");
    let d = root.join("out");
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
    let err = HlsPublisher::with_config(config(&d))
        .err()
        .expect("refused");
    assert!(matches!(err, HlsError::UntrustedOutputDir { .. }), "{err}");
}

#[test]
fn symlinked_output_dir_is_refused() {
    let root = scratch("link");
    let real = private_dir(&root, "real");
    let link = root.join("link");
    symlink(&real, &link).unwrap();
    let err = HlsPublisher::with_config(config(&link))
        .err()
        .expect("refused");
    assert!(matches!(err, HlsError::UntrustedOutputDir { .. }), "{err}");
}

#[test]
fn planted_symlinks_are_not_followed() {
    let root = scratch("plant");
    let d = private_dir(&root, "out");
    let victim = root.join("victim");
    std::fs::write(&victim, b"untouched").unwrap();
    // Planted before construction: the stale-file cleanup removes the link
    // itself and the segment is a fresh regular file.
    symlink(&victim, d.join("segment_00000.ts")).unwrap();
    let mut p = HlsPublisher::with_config(config(&d)).unwrap();
    p.push_ts(&ts_packets(1)).unwrap();
    let meta = std::fs::symlink_metadata(d.join("segment_00000.ts")).unwrap();
    assert!(meta.file_type().is_file());
    // Planted after construction: the playlist write must fail, not follow.
    symlink(&victim, d.join("playlist.m3u8.tmp")).unwrap();
    assert!(p.cut_segment().is_err());
    assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
}

#[test]
fn private_output_dir_works_end_to_end() {
    let root = scratch("ok");
    let d = root.join("fresh"); // created by the segmenter
    let mut p = HlsPublisher::with_config(config(&d)).unwrap();
    p.push_ts(&ts_packets(2)).unwrap();
    p.cut_segment().unwrap();
    assert!(d.join("playlist.m3u8").is_file());
    assert_eq!(
        std::fs::metadata(&d).unwrap().permissions().mode() & 0o077,
        0
    );
}
