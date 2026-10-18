use std::path::Path;

use serde::{Deserialize, Serialize};

use super::tap::{APPROX_SLACK, ATTRIBUTION_WINDOW, Class, RECOVERY_BOUND};

/// Format version of the JSONL log. Bump when a field's MEANING changes
/// (adding a field does not need a bump — serde fills the rest from
/// `Default`); readers refuse a version they do not understand.
pub(super) const TAP_VERSION: u32 = 1;

// ============================================================
// Log types
// ============================================================

/// Wrap-aware PCR coordinate of a packet.
///
/// `pcr_base` is the most recent PCR base seen **strictly before** this
/// packet, and `since_pcr` counts packets from the packet that carried
/// that base (so the packet immediately after a PCR packet has
/// `since_pcr == 1`). Anchoring to the *previous* base rather than the
/// packet's own is deliberate: an injection may destroy the very packet it
/// lands on, and a coordinate whose anchor the receiver can never see
/// would have to be resolved approximately. Before the stream's first PCR,
/// `pcr_base` is `None` and `since_pcr` is the packet ordinal itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coord {
    pub pcr_base: Option<u64>,
    pub since_pcr: u64,
}

/// One logged injection — everything a reader needs to judge whether the
/// receiver noticed, without re-deriving anything from the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Injection {
    /// Sender packet ordinal (informational: it does not survive the
    /// network, so [`Coord`] is what attribution actually uses).
    pub ordinal: u64,
    pub coord: Coord,
    pub class: Class,
    /// PID of the packet touched. For [`Class::Garbage`], the PID of the
    /// packet *after* which the bytes were inserted.
    pub pid: u16,
    /// Byte offsets within the 188-byte packet that changed; empty for
    /// [`Class::Garbage`], [`Class::Drop`] and [`Class::Dup`], which
    /// change no byte of the packet itself.
    pub offsets: Vec<usize>,
    /// The original packet.
    pub before: Vec<u8>,
    /// What went on the wire in its place: the mutated packet, the
    /// truncated prefix, the inserted garbage run, or nothing (drop).
    pub after: Vec<u8>,
    /// Whether a conformant receiver is REQUIRED to notice. A duplicated
    /// packet, or a flip in a payload byte no parser inspects, may be
    /// invisible by design — those are logged but never counted against
    /// the library.
    pub detectable: bool,
    /// The packet was a PAT or PMT.
    pub psi: bool,
    /// PUSI was set and the payload started with a PES start code.
    pub pes_start: bool,
}

/// First JSONL line of the log — `{"header":{...}}`. `min_gap`,
/// `attribution_window`, `recovery_bound` and `approx_slack` are the
/// tap's packet geometry AT ITS AU SCALE (see [`ATTRIBUTION_WINDOW`]);
/// the receiver's [`Attribution`](super::Attribution) judges with these, never with the
/// scale-1 constants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogHeader {
    pub tap_version: u32,
    pub seed: u64,
    pub rate_per_10k: u32,
    pub min_gap: u64,
    pub classes: Vec<Class>,
    pub attribution_window: u64,
    pub recovery_bound: u64,
    /// Logs written before the geometry scaled carry no slack field and
    /// were all scale 1, so they read as [`APPROX_SLACK`].
    #[serde(default = "default_approx_slack")]
    pub approx_slack: u64,
}

pub(super) fn default_approx_slack() -> u64 {
    APPROX_SLACK
}

/// One line of the JSONL log. Serde's external tagging renders this as
/// exactly `{"header":{…}}` / `{"injection":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LogLine {
    Header(LogHeader),
    Injection(Injection),
}

/// Read a corruption log file (header + injections) in one shot — the
/// OFFLINE reader, for a log whose sender has already finished. A live
/// receiver reads a log that is still being appended to and must use
/// [`LogTail`] instead.
pub fn read_log(path: &Path) -> Result<(LogHeader, Vec<Injection>), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("corruption log {}: {e}", path.display()))?;
    parse_log(&text).map_err(|e| format!("corruption log {}: {e}", path.display()))
}

/// Parse one non-blank log line. `Ok(None)` for a blank line.
fn parse_line(n: usize, line: &str) -> Result<Option<LogLine>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let parsed: LogLine = serde_json::from_str(line).map_err(|e| format!("line {n}: {e}"))?;
    if let LogLine::Header(h) = &parsed {
        if h.tap_version != TAP_VERSION {
            return Err(format!(
                "line {n}: tap_version {} (this build reads {TAP_VERSION})",
                h.tap_version
            ));
        }
    }
    Ok(Some(parsed))
}

/// Parse a corruption log's text. Fails closed: a line that is neither a
/// header nor an injection, a missing header, or a tap version this build
/// does not understand is an error naming the line.
pub fn parse_log(text: &str) -> Result<(LogHeader, Vec<Injection>), String> {
    let mut header: Option<LogHeader> = None;
    let mut injections = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        match parse_line(n, line)? {
            None => continue,
            Some(LogLine::Header(h)) => {
                if header.is_some() {
                    return Err(format!("line {n}: a second header line"));
                }
                header = Some(h);
            }
            Some(LogLine::Injection(i)) => {
                if header.is_none() {
                    return Err(format!("line {n}: injection before the header line"));
                }
                injections.push(i);
            }
        }
    }
    let header = header.ok_or_else(|| "no header line".to_string())?;
    Ok((header, injections))
}

/// Incremental reader for a corruption log that is STILL BEING WRITTEN.
///
/// A live receiver and the sender it is judging run at the same time, so
/// the log the receiver must read is a file the sender appends to for the
/// whole run. Reading it once at startup — which is all a receiver could
/// do with [`read_log`] — yields the header and whatever handful of
/// injections happened to be on disk at that instant; every injection
/// after that would surface as an unexplained event and fail the run.
///
/// So the receiver polls this instead. Each [`LogTail::poll`] returns the
/// injections appended since the previous call, in order, and a line the
/// sender has only half-written is held back until the rest of it lands
/// (the tap flushes per line, so a torn line is a narrow window, not an
/// error). Feed what comes back to [`Attribution::append`](super::Attribution::append).
///
/// Waiting for the file to EXIST is the caller's job: the sender creates
/// it, and which process starts first is the caller's arrangement to
/// make, not something a reader can paper over by blocking.
#[derive(Debug)]
pub struct LogTail {
    /// Buffered reader over the log file: each `poll` reads whatever the
    /// writer has appended since, one `read_until('\n')` per line, so a
    /// multi-hour archive a restarted receiver attaches to costs one
    /// linear pass and no archive-sized buffer.
    reader: std::io::BufReader<std::fs::File>,
    path: std::path::PathBuf,
    header: LogHeader,
    /// Bytes of the trailing line the writer has not terminated yet
    /// (`read_until` appends to it; it is taken whole once the newline
    /// arrives). Never holds more than one line.
    pub(super) carry: Vec<u8>,
    /// 1-based number of the next line, for error messages that name the
    /// same line the offline parser would.
    next_line: usize,
    /// Injections kept by [`LogTail::skip_history`], returned by the next
    /// [`LogTail::poll`] before anything appended later.
    pub(super) held: Vec<Injection>,
}

impl LogTail {
    /// Open `path` and consume its header line, leaving everything after
    /// it for the first [`LogTail::poll`]. Fails if the file cannot be
    /// read, if its first line is not a header this build understands, or
    /// if no complete line has been written yet (the sender created the
    /// file microseconds ago — the caller retries).
    pub fn open(path: &Path) -> Result<LogTail, String> {
        let name = || format!("corruption log {}", path.display());
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", name()))?;
        let mut tail = LogTail {
            reader: std::io::BufReader::with_capacity(64 * 1024, file),
            path: path.to_path_buf(),
            // Replaced below; a placeholder keeps `next_complete_line` usable.
            header: LogHeader {
                tap_version: TAP_VERSION,
                seed: 0,
                rate_per_10k: 0,
                min_gap: 0,
                classes: Vec::new(),
                attribution_window: ATTRIBUTION_WINDOW,
                recovery_bound: RECOVERY_BOUND,
                approx_slack: APPROX_SLACK,
            },
            carry: Vec::new(),
            next_line: 1,
            held: Vec::new(),
        };
        match tail.next_complete_line()? {
            Some(LogLine::Header(h)) => {
                tail.header = h;
                Ok(tail)
            }
            Some(LogLine::Injection(_)) => {
                Err(format!("{}: injection before the header line", name()))
            }
            None => Err(format!("{}: no header line yet", name())),
        }
    }

    /// The header the log declared — the attribution window and recovery
    /// bound an [`Attribution`](super::Attribution) must judge by.
    #[must_use]
    pub fn header(&self) -> &LogHeader {
        &self.header
    }

    /// Injections appended since the previous call, in log order.
    pub fn poll(&mut self) -> Result<Vec<Injection>, String> {
        let mut out = std::mem::take(&mut self.held);
        while let Some(line) = self.next_complete_line()? {
            match line {
                LogLine::Injection(i) => out.push(i),
                LogLine::Header(_) => {
                    return Err(format!(
                        "corruption log {}: a second header line",
                        self.path.display()
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Read every injection already in the log and keep only the newest
    /// `keep_last` for the next `poll` (which returns them first, before
    /// anything appended later). Returns how many older ones were skipped.
    /// Linear in the file; memory bounded by `keep_last`.
    pub fn skip_history(&mut self, keep_last: usize) -> Result<u64, String> {
        let mut kept: std::collections::VecDeque<Injection> =
            std::collections::VecDeque::with_capacity(keep_last);
        let mut skipped: u64 = 0;
        while let Some(line) = self.next_complete_line()? {
            match line {
                LogLine::Injection(i) => {
                    kept.push_back(i);
                    while kept.len() > keep_last {
                        kept.pop_front();
                        skipped += 1;
                    }
                }
                LogLine::Header(_) => {
                    return Err(format!(
                        "corruption log {}: a second header line",
                        self.path.display()
                    ));
                }
            }
        }
        self.held = kept.into_iter().collect();
        Ok(skipped)
    }

    /// The next NEWLINE-TERMINATED line, parsed; `None` at the current end
    /// of the file. A trailing unterminated line stays in `carry` for a
    /// later poll (the writer is mid-line).
    fn next_complete_line(&mut self) -> Result<Option<LogLine>, String> {
        use std::io::BufRead;
        loop {
            match self.reader.read_until(b'\n', &mut self.carry) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(format!("corruption log {}: {e}", self.path.display()));
                }
            }
            if self.carry.last() != Some(&b'\n') {
                // End of file inside a line: keep the bytes, report nothing.
                return Ok(None);
            }
            let raw = std::mem::take(&mut self.carry);
            let n = self.next_line;
            self.next_line += 1;
            let text = std::str::from_utf8(&raw[..raw.len() - 1])
                .map_err(|e| format!("corruption log {}: line {n}: {e}", self.path.display()))?;
            if let Some(parsed) = parse_line(n, text)
                .map_err(|e| format!("corruption log {}: {e}", self.path.display()))?
            {
                return Ok(Some(parsed));
            }
        }
    }
}
