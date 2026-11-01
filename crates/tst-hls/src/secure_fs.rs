//! The output directory as a validated handle.
//!
//! On Unix, [`OutputDir::open`] normalises the path lexically, creates the
//! directory 0700 if missing, opens it once with `O_DIRECTORY | O_NOFOLLOW`
//! and runs the owner and mode checks on that open descriptor. Every later
//! operation (create, read, rename, unlink, listing) goes through the
//! descriptor with the `*at` calls and `O_NOFOLLOW`, so renaming the
//! directory's pathname afterwards, or planting a symlink inside, cannot
//! redirect them. The directory is refused when it is a symlink, is not owned
//! by the process user, or is writable by group or others.
//!
//! On other platforms the same API works on the path and no check is made.
//! Names passed to the handle are single path components.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::HlsError;

/// Drop `.` components and redundant trailing separators without resolving
/// anything, so a trailing `/` or `/.` cannot make the final-component
/// symlink check look through a link.
fn normalise(path: &Path) -> PathBuf {
    let p: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if p.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        p
    }
}

fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') || name == ".." || name == "." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output directory entry must be a single path component",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) struct OutputDir {
    dir: File,
}

#[cfg(not(unix))]
pub(crate) struct OutputDir {
    path: PathBuf,
}

#[cfg(unix)]
impl OutputDir {
    /// Create `path` (and missing parents, mode 0700) if needed, open it and
    /// verify the open directory can be trusted.
    pub(crate) fn open(path: &Path) -> Result<Self, HlsError> {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        let path = normalise(path);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .map_err(HlsError::Io)?;
        let untrusted = |reason: &str| HlsError::UntrustedOutputDir {
            path: path.display().to_string(),
            reason: reason.to_string(),
        };
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| HlsError::InvalidConfig("output_dir contains a NUL byte".into()))?;
        // SAFETY: `c` is a valid NUL-terminated path.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                // A final symlink (ELOOP, or ENOTDIR with O_DIRECTORY) or a
                // plain file.
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                    untrusted("it is a symbolic link or not a directory")
                }
                _ => HlsError::Io(e),
            });
        }
        // SAFETY: `fd` was just returned by open and is owned by nobody else.
        let dir = unsafe { File::from_raw_fd(fd) };
        let meta = dir.metadata().map_err(HlsError::Io)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(untrusted("it is not owned by the process user"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(untrusted("it is writable by group or others"));
        }
        Ok(Self { dir })
    }

    fn openat(&self, name: &str, flags: libc::c_int) -> io::Result<File> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        check_name(name)?;
        let c = CString::new(name).map_err(io::Error::other)?;
        // SAFETY: valid directory fd and NUL-terminated component; the mode
        // argument is only read when O_CREAT is set.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is freshly opened and unowned.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Exclusive create for writing.
    pub(crate) fn create_new(&self, name: &str) -> io::Result<File> {
        self.openat(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
    }

    /// Create or truncate for writing.
    pub(crate) fn create_truncate(&self, name: &str) -> io::Result<File> {
        self.openat(name, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)
    }

    /// Read a whole file.
    #[cfg(feature = "serve")]
    pub(crate) fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        self.openat(name, libc::O_RDONLY)?.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Atomically replace `to` with `from`.
    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        check_name(from)?;
        check_name(to)?;
        let (f, t) = (
            CString::new(from).map_err(io::Error::other)?,
            CString::new(to).map_err(io::Error::other)?,
        );
        let fd = self.dir.as_raw_fd();
        // SAFETY: valid directory fd and NUL-terminated components.
        if unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Unlink a file (a symlink is removed itself, never followed).
    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        check_name(name)?;
        let c = CString::new(name).map_err(io::Error::other)?;
        // SAFETY: valid directory fd and NUL-terminated component.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Entry names in the directory.
    fn names(&self) -> io::Result<Vec<String>> {
        use std::ffi::CStr;
        use std::os::fd::AsRawFd;
        // SAFETY: dup of a valid fd; on success fdopendir owns the duplicate.
        let dup = unsafe { libc::fcntl(self.dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` is a directory fd we own.
        let d = unsafe { libc::fdopendir(dup) };
        if d.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so the descriptor is still ours.
            unsafe { libc::close(dup) };
            return Err(e);
        }
        let mut out = Vec::new();
        // SAFETY: `d` is a valid DIR*; entries are read until NULL and the
        // stream is closed exactly once. d_name is NUL-terminated.
        unsafe {
            libc::rewinddir(d);
            loop {
                let ent = libc::readdir(d);
                if ent.is_null() {
                    break;
                }
                let n = CStr::from_ptr((*ent).d_name.as_ptr()).to_string_lossy();
                if n != "." && n != ".." {
                    out.push(n.into_owned());
                }
            }
            libc::closedir(d);
        }
        Ok(out)
    }

    /// Unlink every entry whose name satisfies `stale` (best effort).
    pub(crate) fn remove_matching(&self, stale: impl Fn(&str) -> bool) {
        if let Ok(names) = self.names() {
            for n in names.iter().filter(|n| stale(n)) {
                let _ = self.remove(n);
            }
        }
    }
}

#[cfg(not(unix))]
impl OutputDir {
    pub(crate) fn open(path: &Path) -> Result<Self, HlsError> {
        let path = normalise(path);
        std::fs::create_dir_all(&path).map_err(HlsError::Io)?;
        Ok(Self { path })
    }

    pub(crate) fn create_new(&self, name: &str) -> io::Result<File> {
        check_name(name)?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.join(name))
    }

    pub(crate) fn create_truncate(&self, name: &str) -> io::Result<File> {
        check_name(name)?;
        File::create(self.path.join(name))
    }

    #[cfg(feature = "serve")]
    pub(crate) fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        check_name(name)?;
        std::fs::read(self.path.join(name))
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        check_name(from)?;
        check_name(to)?;
        std::fs::rename(self.path.join(from), self.path.join(to))
    }

    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        check_name(name)?;
        std::fs::remove_file(self.path.join(name))
    }

    pub(crate) fn remove_matching(&self, stale: impl Fn(&str) -> bool) {
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for e in entries.flatten() {
                if stale(&e.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

#[cfg(all(test, unix, feature = "serve"))]
mod tests {
    use super::OutputDir;

    #[test]
    fn read_refuses_a_symlink_and_never_reads_the_target() {
        let dir = std::env::temp_dir().join(format!("secure-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let od = OutputDir::open(&dir).unwrap();
        std::fs::write(dir.join("target"), b"secret").unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).unwrap();
        assert!(od.read("link").is_err());
        assert_eq!(od.read("target").unwrap(), b"secret");
        assert!(od.read("../x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
