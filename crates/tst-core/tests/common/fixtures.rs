//! Fixture paths and external-tool probes shared across test binaries.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `tests/fixtures/<rel>` under the crate root.
pub fn fixture_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

/// `tests/fixtures/audio/<name>` under the crate root.
pub fn audio_fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join(name)
}

/// The bytes of `tests/fixtures/subtitles/<name>`; panics naming the path
/// when the file cannot be read.
pub fn subtitle_fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subtitles")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {}", path.display(), e))
}

/// `ffprobe -version` runs and exits 0.
pub fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
