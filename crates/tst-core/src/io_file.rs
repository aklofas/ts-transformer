//! File-based ergonomics for the muxer and demuxer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Gated behind the `file` cargo feature (default-on). Embedded users
//! without a filesystem disable via `tst-core = { default-features = false }`.
//!
//! # Error policy for file helpers (cross-binding)
//!
//! File helpers come in three shapes, each with consistent error
//! semantics so callers across Rust + Python + C can reason about
//! failure the same way:
//!
//! 1. **Full-file helpers** ([`demux_file`], [`demux_file_with_config`])
//!    — load the whole file, return [`io::Result<Vec<DemuxEvent>>`]. I/O
//!    errors propagate as the underlying [`io::Error`]; demux errors
//!    (e.g. unrecoverable sync loss, malformed PSI/PES, strict-mode
//!    rejection) are mapped to [`io::ErrorKind::InvalidData`] with the
//!    [`DemuxError`] formatted into the message.
//!
//! 2. **Lossy streaming helpers** ([`DemuxFromFile`]) — iterate
//!    `Item = DemuxEvent`. Read failures and demux-feed failures are
//!    silently coerced to early EOF. Kept for backward compatibility
//!    with existing demos; **new code should prefer
//!    [`TryDemuxFromFile`]**, which surfaces the same errors via
//!    `Iterator<Item = io::Result<DemuxEvent>>`.
//!
//! 3. **Fallible streaming helpers** ([`TryDemuxFromFile`]) — iterate
//!    `Item = io::Result<DemuxEvent>`. On any read or demux-feed
//!    error, the iterator yields `Some(Err(_))` once and then `None`
//!    forever. Trailing events already buffered before the error are
//!    NOT drained — the iterator terminates immediately after the
//!    error so the caller can act on it without confusing it with
//!    later events.
//!
//! The matching Python entry point
//! [`tstrans.io.parse_file`](https://github.com/aklofas/ts-transformer/blob/main/bindings/python/python/tstrans/io.py)
//! is fallible by construction — it raises `tstrans.exceptions.DemuxError`
//! on the same conditions that [`TryDemuxFromFile`] surfaces as `Err`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::error::DemuxError;
use crate::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig};
use crate::mpegts::mux::Muxer;

/// Drain all pending events from a demuxer into `out`.
fn drain_events(demuxer: &mut Demuxer, out: &mut Vec<DemuxEvent>) {
    while let Some(ev) = demuxer.next_event() {
        out.push(ev);
    }
}

/// Read an entire `.ts` file and return all events emitted by the demuxer.
///
/// Convenient for analysis scripts and tests. For large files, prefer
/// [`TryDemuxFromFile`] (streaming, bounded memory, surfaces errors).
///
/// # Errors
///
/// Returns [`std::io::Error`] if the file cannot be read. Demux errors
/// (e.g. unrecoverable sync loss) are mapped to
/// `io::ErrorKind::InvalidData`.
pub fn demux_file(path: impl AsRef<Path>) -> io::Result<Vec<DemuxEvent>> {
    demux_file_with_config(path, DemuxerConfig::default())
}

/// Like [`demux_file`] but with caller-supplied [`DemuxerConfig`].
pub fn demux_file_with_config(
    path: impl AsRef<Path>,
    config: DemuxerConfig,
) -> io::Result<Vec<DemuxEvent>> {
    let bytes = std::fs::read(path)?;
    let mut demuxer = Demuxer::with_config(config);
    let mut out: Vec<DemuxEvent> = Vec::new();
    demuxer
        .feed(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, demux_err_msg(e)))?;
    drain_events(&mut demuxer, &mut out);
    demuxer.flush();
    drain_events(&mut demuxer, &mut out);
    Ok(out)
}

/// Streaming iterator over demuxer events from a file. Reads the file
/// in 64 KiB chunks, feeding the demuxer incrementally.
///
/// Bounded memory: at most 64 KiB of raw TS bytes are held at a time,
/// plus whatever the demuxer buffers for in-progress PES reassembly.
///
/// # Lossy on errors — prefer [`TryDemuxFromFile`] for new code
///
/// This iterator's `Item` is `DemuxEvent`, so it has no channel to
/// surface read or demux failures: both silently coerce to early
/// EOF. That makes it impossible for a Rust consumer to distinguish
/// a clean end-of-file from a truncated read or a malformed
/// transport stream. It is kept for backward compatibility; new code
/// should use
/// [`TryDemuxFromFile`] instead, which iterates
/// `io::Result<DemuxEvent>` and emits an `Err` on the same
/// conditions. See the module-level docs for the full file-helper
/// error policy.
///
/// ```no_run
/// # #[allow(deprecated)]
/// # {
/// use tst_core::io_file::DemuxFromFile;
///
/// let iter = DemuxFromFile::open("input.ts").unwrap();
/// for event in iter {
///     println!("{:?}", event);
/// }
/// # }
/// ```
#[deprecated(
    since = "0.6.0",
    note = "lossy on read/demux-feed errors (silently coerces to early EOF) \
            — use `TryDemuxFromFile` instead, which surfaces the same \
            failures via `Iterator<Item = io::Result<DemuxEvent>>`; \
            removal no earlier than 1.0"
)]
pub struct DemuxFromFile {
    file: File,
    buf: Box<[u8; 65536]>,
    demuxer: Demuxer,
    pending: std::collections::VecDeque<DemuxEvent>,
    eof: bool,
}

#[allow(deprecated)] // the impl block for a deprecated type warns on every field touch
impl DemuxFromFile {
    /// Open `path` with default [`DemuxerConfig`].
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(path, DemuxerConfig::default())
    }

    /// Open `path` with caller-supplied [`DemuxerConfig`].
    pub fn open_with_config(path: impl AsRef<Path>, config: DemuxerConfig) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            buf: Box::new([0u8; 65536]),
            demuxer: Demuxer::with_config(config),
            pending: std::collections::VecDeque::new(),
            eof: false,
        })
    }
}

#[allow(deprecated)] // the impl block for a deprecated type warns on every field touch
impl Iterator for DemuxFromFile {
    type Item = DemuxEvent;

    fn next(&mut self) -> Option<DemuxEvent> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.eof {
                return None;
            }
            let n = match self.file.read(self.buf.as_mut()) {
                Ok(0) => {
                    // EOF: flush the demuxer and collect any trailing events.
                    self.eof = true;
                    self.demuxer.flush();
                    while let Some(ev) = self.demuxer.next_event() {
                        self.pending.push_back(ev);
                    }
                    continue;
                }
                Ok(n) => n,
                Err(_) => {
                    self.eof = true;
                    return None;
                }
            };
            // Feed this chunk; silently stop on unrecoverable demux errors.
            if self.demuxer.feed(&self.buf[..n]).is_err() {
                self.eof = true;
            }
            while let Some(ev) = self.demuxer.next_event() {
                self.pending.push_back(ev);
            }
        }
    }
}

/// Fallible streaming iterator over demuxer events from a file.
///
/// Like [`DemuxFromFile`] but exposes read failures and demux-feed
/// failures via `Iterator::Item = io::Result<DemuxEvent>` instead of
/// silently coercing them to early EOF. Reads the file in 64 KiB
/// chunks; bounded memory same as the lossy variant.
///
/// # Error reporting contract
///
/// - **Read error** (e.g. truncated mid-PES file, disk error): the
///   iterator emits `Some(Err(io_err))` next, then `None` on every
///   subsequent call. Events that the demuxer had already produced
///   from the successful prefix are returned BEFORE the error (they
///   sit in the internal `pending` queue from prior `next()` loops).
/// - **Demux feed error** (e.g. malformed PSI/PES, strict-mode
///   rejection, sync-buffer exhaustion): the iterator emits
///   `Some(Err(io::Error::new(InvalidData, demux_err)))` and then
///   `None` forever. As with read errors, already-queued events
///   from before the bad chunk are drained first.
/// - **Clean EOF**: matches [`DemuxFromFile`] — the demuxer is
///   flushed, any trailing events are drained as `Ok`, then `None`.
///
/// After the iterator yields an `Err`, it is exhausted; calling
/// `next()` again returns `None`. Callers wanting partial progress
/// after an error must save events as they arrive.
///
/// ```no_run
/// use tst_core::io_file::TryDemuxFromFile;
///
/// let iter = TryDemuxFromFile::open("input.ts").unwrap();
/// for result in iter {
///     match result {
///         Ok(ev) => println!("{:?}", ev),
///         Err(e) => {
///             eprintln!("demux/io error: {e}");
///             break;
///         }
///     }
/// }
/// ```
pub struct TryDemuxFromFile {
    file: File,
    buf: Box<[u8; 65536]>,
    demuxer: Demuxer,
    pending: std::collections::VecDeque<DemuxEvent>,
    /// `Some(_)` while a read- or feed-error is staged. Set when a
    /// read fails or `feed()` returns `Err`; cleared (and `done` set
    /// to `true`) when the staged error is actually emitted to the
    /// caller. Events already queued in `pending` drain BEFORE the
    /// staged error surfaces, so a caller sees the successful prefix
    /// → the error → then `None` forever.
    pending_error: Option<io::Error>,
    /// Iterator is exhausted (clean EOF reached, or an error was
    /// already emitted). Once true, `next()` only drains `pending`
    /// then returns `None`.
    done: bool,
}

impl TryDemuxFromFile {
    /// Open `path` with default [`DemuxerConfig`].
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(path, DemuxerConfig::default())
    }

    /// Open `path` with caller-supplied [`DemuxerConfig`].
    pub fn open_with_config(path: impl AsRef<Path>, config: DemuxerConfig) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            buf: Box::new([0u8; 65536]),
            demuxer: Demuxer::with_config(config),
            pending: std::collections::VecDeque::new(),
            pending_error: None,
            done: false,
        })
    }
}

impl Iterator for TryDemuxFromFile {
    type Item = io::Result<DemuxEvent>;

    fn next(&mut self) -> Option<io::Result<DemuxEvent>> {
        loop {
            // Drain any already-queued events first so the caller
            // sees the full successful prefix before an error.
            if let Some(ev) = self.pending.pop_front() {
                return Some(Ok(ev));
            }
            // After the pending queue is empty, emit a staged error
            // exactly once, then terminate.
            if let Some(err) = self.pending_error.take() {
                self.done = true;
                return Some(Err(err));
            }
            if self.done {
                return None;
            }
            let n = match self.file.read(self.buf.as_mut()) {
                Ok(0) => {
                    // Clean EOF: flush the demuxer + queue any trailing
                    // events; subsequent loop iteration drains them
                    // before returning None.
                    self.done = true;
                    self.demuxer.flush();
                    while let Some(ev) = self.demuxer.next_event() {
                        self.pending.push_back(ev);
                    }
                    continue;
                }
                Ok(n) => n,
                Err(io_err) => {
                    // Stage the I/O error; loop back so any events
                    // already queued from prior chunks are returned
                    // first, then the error, then None.
                    self.pending_error = Some(io_err);
                    continue;
                }
            };
            match self.demuxer.feed(&self.buf[..n]) {
                Ok(()) => {
                    while let Some(ev) = self.demuxer.next_event() {
                        self.pending.push_back(ev);
                    }
                }
                Err(demux_err) => {
                    // Even on a feed error, the demuxer may have
                    // produced events from the prefix of `buf[..n]`
                    // that parsed cleanly. Drain those first so the
                    // caller sees them before the error.
                    while let Some(ev) = self.demuxer.next_event() {
                        self.pending.push_back(ev);
                    }
                    self.pending_error = Some(io::Error::new(
                        io::ErrorKind::InvalidData,
                        demux_err_msg(demux_err),
                    ));
                }
            }
        }
    }
}

/// Open `path` with caller-supplied [`DemuxerConfig`] and return a
/// fallible streaming iterator. File-open failure surfaces as
/// `Err(io::Error)` on the outer `Result`; later read/demux errors
/// surface as `Some(Err(_))` on the inner iterator (see
/// [`TryDemuxFromFile`] for the full contract).
pub fn try_demux_from_file_with_config(
    path: impl AsRef<Path>,
    config: DemuxerConfig,
) -> io::Result<TryDemuxFromFile> {
    TryDemuxFromFile::open_with_config(path, config)
}

/// Drain a [`Muxer`] to a file. Calls [`Muxer::pull`] repeatedly until
/// the muxer has no more output.
///
/// The muxer must already have content pushed via `push_video` / `push_klv`
/// etc. before calling this. This function does NOT advance the muxer clock
/// or force a PSI flush — the caller is responsible for ensuring the muxer
/// is at the desired boundary before draining.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the file cannot be created or written.
pub fn write_mux_to_file(mux: &mut Muxer, path: impl AsRef<Path>) -> io::Result<()> {
    let mut file = File::create(path)?;
    let mut buf = [0u8; 65536];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    Ok(())
}

fn demux_err_msg(e: DemuxError) -> String {
    format!("demux error: {e}")
}
