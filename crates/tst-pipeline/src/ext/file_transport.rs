//! [`FileTransport`] — write-to-file [`Transport`]: the natural capture /
//! debug sink for [`MuxSender`](crate::MuxSender) (both binding consumers
//! and integration ports have independently rebuilt this — ship it once).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use tst_core::transport::{BrokenCause, Transport, TransportError};

/// Appends every chunk verbatim to a file. Post-close sends return
/// [`TransportError::Closed`] per the trait contract. A fatal write error
/// also marks the transport dead: that call returns
/// [`TransportError::Broken`], `is_alive()` becomes `false`, and any
/// subsequent send returns [`TransportError::Closed`] — the same
/// mark-dead-on-write-error contract `tst-tcp`'s `TcpTransport` follows.
///
/// # Finalization
///
/// [`Self::finish`] is the fallible finalization path: it flushes the
/// userspace buffer and surfaces any I/O error. The trait's `close()` and
/// the implicit `Drop` also flush, but BEST-EFFORT only — `Transport::
/// close` returns `()`, so a flush-only failure there (ENOSPC, EIO, a
/// removed device) is silently swallowed. A capture whose tail matters
/// must end with `finish()` (via
/// [`MuxSender::into_inner`](crate::MuxSender::into_inner)), not `close`.
pub struct FileTransport {
    writer: Option<BufWriter<File>>,
    bytes_sent: u64,
}

impl FileTransport {
    /// Create (truncating) `path` and return a transport writing to it.
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            writer: Some(BufWriter::new(File::create(path)?)),
            bytes_sent: 0,
        })
    }

    /// Total bytes ACCEPTED by `send_bytes`. Accepted means written into
    /// the userspace buffer — not necessarily flushed to the OS yet; only
    /// a successful [`Self::finish`] proves every accepted byte reached
    /// the file.
    #[must_use]
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// Flush every buffered byte to the OS and consume the transport,
    /// surfacing any I/O error the infallible `close()`/`Drop` paths
    /// would swallow. Returns the underlying [`File`] so callers can
    /// e.g. `sync_all()` for durability beyond the OS page cache.
    ///
    /// Calling `finish` after `close()` (or after a write error marked
    /// the transport dead) returns `Ok(None)` — there is no writer left
    /// and nothing buffered to lose.
    ///
    /// # Errors
    ///
    /// Any [`std::io::Error`] from flushing the buffered tail (ENOSPC,
    /// EIO, a removed device, ...).
    pub fn finish(mut self) -> std::io::Result<Option<File>> {
        match self.writer.take() {
            Some(w) => Ok(Some(
                w.into_inner()
                    .map_err(std::io::IntoInnerError::into_error)?,
            )),
            None => Ok(None),
        }
    }
}

impl Transport for FileTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        let Some(w) = self.writer.as_mut() else {
            return Err(TransportError::Closed);
        };
        if let Err(e) = w.write_all(msg) {
            // Fatal: drop the writer so is_alive() reports dead and any
            // later send takes the closed-writer branch above (returning
            // Closed), matching tst-tcp's mark-dead-on-write-error
            // pattern. Without this, a write failure (ENOSPC/EIO/a
            // removed device) would leave is_alive() reporting true
            // forever, violating the trait's "closed or previously
            // broken" contract.
            self.writer = None;
            return Err(TransportError::Broken {
                msg: format!("file write failed: {e}"),
                errno_code: e.raw_os_error(),
                cause: BrokenCause::Unspecified,
            });
        }
        self.bytes_sent = self.bytes_sent.saturating_add(msg.len() as u64);
        Ok(())
    }

    // MuxSender sizes its drain scratch to max_payload; 7×188 keeps the
    // chunking identical to the wire transports' ecosystem default.
    fn max_payload(&self) -> usize {
        7 * 188
    }

    fn is_alive(&self) -> bool {
        self.writer.is_some()
    }

    fn close(&mut self) {
        // Best-effort by construction: `Transport::close` returns `()`,
        // so a flush failure here is unreportable. `finish()` is the
        // fallible path for captures whose tail matters.
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
    }

    // socket_stats: trait default (None) — a file has no socket.
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn video_only_config() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.pcr_pid(0x1011);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    #[test]
    fn file_transport_captures_muxed_ts_and_rejects_post_close() {
        let path =
            std::env::temp_dir().join(format!("tst-file-transport-{}.ts", std::process::id()));
        let t = FileTransport::create(&path).unwrap();
        let sender = crate::MuxSender::new(t, video_only_config()).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        sender.send_video(&nal, Pts90khz::new(0), true).unwrap();
        let mut t = sender.into_inner();
        // into_inner leaves the transport open (not closed) — pin that.
        assert!(t.is_alive());
        t.close();
        let bytes = std::fs::read(&path).unwrap();
        // Remove the temp file BEFORE asserting — a failed assert used to
        // leak it into the temp dir. Nothing below touches the path
        // (post-close send_bytes never reaches the filesystem).
        let _ = std::fs::remove_file(&path);
        assert!(!bytes.is_empty() && bytes.len() % 188 == 0 && bytes[0] == 0x47);
        // Post-close contract from the Transport trait rustdoc:
        assert!(matches!(
            t.send_bytes(&[0u8; 188]),
            Err(TransportError::Closed)
        ));
    }

    /// A fatal write error must mark the transport dead: `is_alive()` goes
    /// `false` and every later send returns `Closed` (not a repeated
    /// `Broken`, and not `Ok` — the ENOSPC/EIO/removed-device bug this
    /// guards against was `is_alive()` reporting `true` forever).
    ///
    /// Deterministic, portable failure injection: no disk-full or
    /// permission-race trick is both simple and reliable across
    /// Linux/macOS/Windows, so this constructs the transport normally via
    /// `create()` (proving the path/file really exists and is otherwise
    /// healthy) and then swaps in a `BufWriter` over a file handle opened
    /// **read-only** — a write through a read-only handle fails at the OS
    /// level identically on all three platforms. The swap uses ordinary
    /// same-module private-field access (this test module is a descendant
    /// of `file_transport`, not a new public seam).
    ///
    /// The swapped-in `BufWriter` uses a tiny explicit capacity: a write
    /// bigger than the buffer bypasses buffering and goes straight to the
    /// inner file, so the read-only failure actually surfaces here rather
    /// than being silently absorbed into the buffer (as it would be
    /// against `BufWriter`'s much larger default capacity for a 188-byte
    /// TS-packet-sized write).
    #[test]
    fn write_failure_marks_transport_dead() {
        let path = std::env::temp_dir().join(format!(
            "tst-file-transport-write-fail-{}.ts",
            std::process::id()
        ));
        let mut t = FileTransport::create(&path).unwrap();
        assert!(t.is_alive());
        let read_only = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        t.writer = Some(BufWriter::with_capacity(8, read_only));

        let err = t.send_bytes(&[0u8; 188]).unwrap_err();
        let alive_after_failure = t.is_alive();
        let second_send = t.send_bytes(&[0u8; 188]);
        // Remove the temp file BEFORE asserting — a failed assert used to
        // leak it into the temp dir.
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, TransportError::Broken { .. }));
        assert!(
            !alive_after_failure,
            "write failure must mark the transport dead"
        );
        assert!(matches!(second_send, Err(TransportError::Closed)));
    }

    /// A failure that occurs ONLY at final flush — every send accepted,
    /// bytes still in the userspace buffer — must surface through
    /// `finish()`. This is exactly the case the infallible `close()`
    /// swallows (the audit's silent-tail-loss finding): 188 bytes fit
    /// the buffer, so `send_bytes` succeeds; the read-only inner file
    /// rejects the write only when the buffer drains at finish.
    #[test]
    fn finish_surfaces_flush_only_failure() {
        let path = std::env::temp_dir().join(format!(
            "tst-file-transport-flush-fail-{}.ts",
            std::process::id()
        ));
        let mut t = FileTransport::create(&path).unwrap();
        let read_only = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        // Large capacity: the 188-byte send stays buffered (no write-through),
        // so the EBADF surfaces at finish's flush, not at send.
        t.writer = Some(BufWriter::with_capacity(4096, read_only));
        let mut pkt = [0u8; 188];
        pkt[0] = 0x47;
        t.send_bytes(&pkt)
            .expect("small send must buffer successfully");
        assert_eq!(t.bytes_sent(), 188);
        let res = t.finish();
        let _ = std::fs::remove_file(&path);
        assert!(res.is_err(), "flush-only failure must surface, got {res:?}");
    }

    /// The happy path: after a successful `finish()`, every accepted byte
    /// is visible to a plain reader (flush actually drained the buffer).
    #[test]
    fn finish_makes_accepted_bytes_visible() {
        let path = std::env::temp_dir().join(format!(
            "tst-file-transport-finish-{}.ts",
            std::process::id()
        ));
        let mut t = FileTransport::create(&path).unwrap();
        let mut pkt = [0u8; 188];
        pkt[0] = 0x47;
        t.send_bytes(&pkt).unwrap();
        let file = t.finish().expect("finish").expect("live writer");
        drop(file);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len(), 188);
        assert_eq!(bytes[0], 0x47);
    }
}
