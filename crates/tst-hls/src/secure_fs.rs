//! Output-directory trust check and symlink-free file opens.
//!
//! On Unix the segmenter refuses an output directory the process does not
//! own or that group/others can write, and never follows a symlink when it
//! creates or reads a file there. On other platforms both are no-ops.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::error::HlsError;

/// Create `dir` (private, mode 0700 on Unix) if missing, then verify it can
/// be trusted.
pub(crate) fn prepare_output_dir(dir: &Path) -> Result<(), HlsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(HlsError::Io)?;
        check_trusted(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(HlsError::Io)
    }
}

#[cfg(unix)]
fn check_trusted(dir: &Path) -> Result<(), HlsError> {
    use std::os::unix::fs::MetadataExt;
    let untrusted = |reason: &str| HlsError::UntrustedOutputDir {
        path: dir.display().to_string(),
        reason: reason.to_string(),
    };
    let meta = std::fs::symlink_metadata(dir).map_err(HlsError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(untrusted("it is a symbolic link"));
    }
    if !meta.is_dir() {
        return Err(untrusted("it is not a directory"));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(untrusted("it is not owned by the process user"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(untrusted("it is writable by group or others"));
    }
    Ok(())
}

/// Create-or-truncate for writing; refuses a final symlink on Unix.
pub(crate) fn create_truncate_nofollow(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    nofollow(&mut o);
    o.open(path)
}

/// Exclusive create for writing; refuses a final symlink on Unix.
pub(crate) fn create_new_nofollow(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    nofollow(&mut o);
    o.open(path)
}

/// Read a whole file; refuses a final symlink on Unix.
#[cfg(feature = "serve")]
pub(crate) fn read_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    let mut o = OpenOptions::new();
    o.read(true);
    nofollow(&mut o);
    use std::io::Read;
    let mut buf = Vec::new();
    o.open(path)?.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(unix)]
fn nofollow(o: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    o.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn nofollow(_: &mut OpenOptions) {}
