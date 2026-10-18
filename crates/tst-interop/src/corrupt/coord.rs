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
    carry: Vec<u8>,
    /// 1-based number of the next line, for error messages that name the
    /// same line the offline parser would.
    next_line: usize,
    /// Injections kept by [`LogTail::skip_history`], returned by the next
    /// [`LogTail::poll`] before anything appended later.
    held: Vec<Injection>,
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

#[cfg(test)]
mod tests {
    use super::super::tap::tests::*;
    use super::*;
    use crate::corrupt::*;

    /// A restarted receiver re-attaches to a log the sender has been
    /// writing for hours. The tail must stream it: memory bounded by the
    /// longest line, not the archive (the previous whole-file carry with a
    /// front drain per line was quadratic and held the archive in RAM).
    #[test]
    fn log_tail_streams_an_archive_with_bounded_carry() {
        let b = baseline_bytes(60.0, "tail-capacity");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let mut archive = format!("{header}\n");
        for _ in 0..4096 {
            archive.push_str(first);
            archive.push('\n');
        }
        let path = std::env::temp_dir().join(format!(
            "tst-interop-tail-capacity-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &archive).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let history = tail.poll().unwrap();
        let capacity = tail.carry.capacity();
        assert!(tail.carry.is_empty());
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(history.len(), 4096);
        assert!(
            capacity < first.len() * 2 + 2,
            "carry must hold at most one line, got capacity {capacity} for a {}-byte archive",
            archive.len()
        );
    }

    /// The sender appends; a poll can land mid-line. The partial line is
    /// held, never parsed, and comes back exactly once when terminated.
    #[test]
    fn partial_last_line_is_held_until_terminated() {
        use std::io::Write;
        let b = baseline_bytes(60.0, "tail-partial");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let path = std::env::temp_dir().join(format!(
            "tst-interop-tail-partial-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (head, rest) = first.split_at(first.len() / 2);
        std::fs::write(&path, format!("{header}\n{first}\n{head}")).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1, "one complete line");
        assert_eq!(tail.poll().unwrap().len(), 0, "the partial line is held");
        assert!(!tail.carry.is_empty(), "held bytes live in carry");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{rest}").unwrap();
        drop(f);
        let got = tail.poll().unwrap();
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(got.len(), 1, "the completed line arrives exactly once");
    }

    /// Archived logs (every soak before the geometry scaled) have no
    /// `approx_slack`; they were all scale 1, so they must read as 128.
    #[test]
    fn an_archived_header_without_approx_slack_reads_as_scale_one() {
        let json = r#"{"header":{"tap_version":1,"seed":12,"rate_per_10k":5,"min_gap":1000,"classes":["body_flip"],"attribution_window":500,"recovery_bound":600}}"#;
        let LogLine::Header(h) = serde_json::from_str::<LogLine>(json).unwrap() else {
            panic!("not a header");
        };
        assert_eq!(h.approx_slack, APPROX_SLACK);
    }

    #[test]
    fn read_log_round_trips_and_the_parser_fails_closed() {
        let b = baseline_bytes(6.0, "rt");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-rt-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, &text).unwrap();
        let from_file = read_log(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let from_text = parse_log(&text).unwrap();
        assert_eq!(from_file.1, from_text.1);
        assert!(!from_file.1.is_empty());

        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let injection = lines.next().expect("at least one injection");
        // Every rejection names the offending line.
        for (bad, want) in [
            (format!("{header}\n{header}"), "line 2"),
            (injection.to_string(), "line 1"),
            (format!("{header}\nnot json"), "line 2"),
            (
                header.replace("\"tap_version\":1", "\"tap_version\":2"),
                "line 1",
            ),
        ] {
            let e = parse_log(&bad).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
        assert!(parse_log("").unwrap_err().contains("no header"));
    }

    /// `LogTail` must read a file that is STILL GROWING: only complete
    /// lines, never the same injection twice, and a half-written line
    /// held back until the rest of it lands.
    #[test]
    fn log_tail_reads_a_growing_file_one_complete_line_at_a_time() {
        use std::io::Write as _;

        // 60s (~3600 packets) so `min_gap`'s 1000-packet floor still
        // leaves several injections to hand out one at a time.
        let b = baseline_bytes(60.0, "tail");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap().to_string();
        let injections: Vec<String> = lines.map(str::to_string).collect();
        assert!(injections.len() >= 2, "need two injections to tail");

        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-tail-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{header}").unwrap();
        f.flush().unwrap();

        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.header().tap_version, TAP_VERSION);
        assert!(tail.poll().unwrap().is_empty(), "nothing appended yet");

        writeln!(f, "{}", injections[0]).unwrap();
        f.flush().unwrap();
        let first = tail.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert!(tail.poll().unwrap().is_empty(), "no injection twice");

        // A torn line: the sender is mid-write. Nothing is returned until
        // the newline lands, and then the whole line parses.
        let (head, rest) = injections[1].split_at(injections[1].len() / 2);
        write!(f, "{head}").unwrap();
        f.flush().unwrap();
        assert!(tail.poll().unwrap().is_empty(), "a torn line is held back");
        writeln!(f, "{rest}").unwrap();
        f.flush().unwrap();
        let second = tail.poll().unwrap();
        assert_eq!(second.len(), 1);
        assert!(
            second[0].ordinal > first[0].ordinal,
            "injections come back in log order"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `LogTail::open` fails closed on a file with no complete header
    /// line yet — the caller's cue to wait and retry, not to proceed with
    /// an empty judgement.
    #[test]
    fn log_tail_open_refuses_a_headerless_file() {
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-tail-empty-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, b"").unwrap();
        let e = LogTail::open(&path).unwrap_err();
        assert!(e.contains("no header line yet"), "{e}");
        // A header line without its newline is not a line yet either.
        std::fs::write(&path, b"{\"header\":{").unwrap();
        assert!(LogTail::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    fn archive_of(n: usize, tag: &str) -> (std::path::PathBuf, usize) {
        let b = baseline_bytes(60.0, tag);
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let mut archive = format!("{header}\n");
        for _ in 0..n {
            archive.push_str(first);
            archive.push('\n');
        }
        let path = std::env::temp_dir().join(format!(
            "tst-interop-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &archive).unwrap();
        (path, first.len())
    }

    /// A receiver attaching to a log that already holds hours of injections
    /// (a hold's restarted leg) keeps only the newest `keep_last`: older
    /// anchors are ones it never listened for, and the oldest would sit
    /// past the 33-bit PCR half-range and block resolution outright.
    #[test]
    fn skip_history_keeps_the_newest_injections() {
        let (path, _) = archive_of(5000, "skip-history");
        let mut tail = LogTail::open(&path).unwrap();
        let skipped = tail.skip_history(4096).unwrap();
        let kept = tail.poll().unwrap();
        assert_eq!(
            tail.poll().unwrap().len(),
            0,
            "nothing left after the kept tail"
        );
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(skipped, 904);
        assert_eq!(kept.len(), 4096);
    }

    /// Fewer injections than the bound (every first receiver, which attaches
    /// to a log the sender has barely started): nothing is skipped and the
    /// order is kept.
    #[test]
    fn skip_history_keeps_everything_under_the_bound() {
        let (path, _) = archive_of(10, "skip-small");
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.skip_history(4096).unwrap(), 0);
        let kept = tail.poll().unwrap();
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(kept.len(), 10);
    }

    /// `keep_last = 0` is a real input (a judgement to keep no history at
    /// all), and the bound has to hold there too: every injection counts
    /// as skipped and `held` never grows past empty.
    #[test]
    fn skip_history_zero_keeps_nothing() {
        let (path, _) = archive_of(10, "skip-zero");
        let mut tail = LogTail::open(&path).unwrap();
        let skipped = tail.skip_history(0).unwrap();
        assert!(tail.held.is_empty(), "keep_last=0 must hold nothing");
        assert_eq!(tail.poll().unwrap().len(), 0, "nothing left to return");
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(skipped, 10);
    }
}

/// Property tests on the packet coordinate and the 33-bit PCR-base circle
/// it lives on. `Coord` itself is plain data; its ordering and resolution
/// rules live in [`Attribution`] (`ticks_ahead`, `on_pcr`, `append`), so
/// the properties drive those with generated coordinates.
#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::super::attribution::{Attribution, ticks_ahead};
    use super::super::tap::{APPROX_SLACK, ATTRIBUTION_WINDOW, Class, RECOVERY_BOUND};
    use super::super::{MAX_APPROX_TICKS, MAX_FUTURE_TICKS};
    use super::{Coord, Injection, LogHeader};

    const MASK: u64 = (1 << 33) - 1;
    const HALF: u64 = 1 << 32;

    /// A base origin: uniform over the circle half the time, otherwise
    /// within `span` ticks BELOW the wrap so a run starting there crosses
    /// 2^33.
    fn origin(span: u64) -> impl Strategy<Value = u64> {
        prop_oneof![0..=MASK, (MASK + 1 - span)..=MASK]
    }

    fn hdr() -> LogHeader {
        LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        }
    }

    fn inj(base: u64, since: u64) -> Injection {
        Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: Some(base),
                since_pcr: since,
            },
            class: Class::Header,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        }
    }

    proptest! {
        /// (a) The wrapping distance orders any two bases: equal bases are
        /// distance 0, the two directions sum to the full circle, and
        /// apart from the exact antipode one direction is "ahead".
        #[test]
        fn wrapping_distance_is_a_total_order_on_bases(a in 0..=MASK, b in 0..=MASK) {
            let ab = ticks_ahead(a, b);
            let ba = ticks_ahead(b, a);
            prop_assert_eq!(ab == 0, a == b);
            prop_assert_eq!((ab + ba) & MASK, 0);
            if a != b && ab != HALF {
                prop_assert!((ab < HALF) != (ba < HALF), "exactly one of {} / {} is ahead", ab, ba);
            }
        }

        /// (a) Along a run of coordinates whose bases advance within a
        /// half-range, the engine's wrapping comparison agrees with an
        /// independent reference: the plain unwrapped position
        /// `(offset from origin, since_pcr)` compared as integers. Half of
        /// the origins sit just below 2^33, so most such runs cross the
        /// wrap.
        #[test]
        fn coordinate_order_is_lexicographic_across_a_wrap(
            origin in origin(10_000_000),
            steps in proptest::collection::vec((0u64..2_000_000, 0u64..600), 2..24),
        ) {
            let mut off = 0u64;
            let mut run = Vec::new(); // (unwrapped offset, Coord)
            for (step, since) in steps {
                off += step;
                run.push((off, Coord {
                    pcr_base: Some((origin + off) % (MASK + 1)),
                    since_pcr: since,
                }));
            }
            for (oi, ci) in &run {
                for (oj, cj) in &run {
                    let (bi, bj) = (ci.pcr_base.unwrap(), cj.pcr_base.unwrap());
                    let reference = (oi, ci.since_pcr).cmp(&(oj, cj.since_pcr));
                    // The engine's reading of the same two coordinates.
                    let base_cmp = if bi == bj {
                        std::cmp::Ordering::Equal
                    } else if ticks_ahead(bj, bi) < HALF {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                    prop_assert_eq!(base_cmp.then(ci.since_pcr.cmp(&cj.since_pcr)), reference);
                    if oj >= oi {
                        prop_assert_eq!(ticks_ahead(bj, bi), oj - oi);
                    }
                }
            }
        }

        /// (b) An injection whose anchor base is in the PCR history lands
        /// at `ord(anchor) + since_pcr`, exactly: whether it was known
        /// before the PCRs arrived or appended afterwards (the history
        /// replay). Half of the origins sit just below 2^33, so the
        /// history regularly straddles the wrap.
        #[test]
        fn resolve_returns_the_logged_packet_index(
            origin in origin(40_000),
            steps in proptest::collection::vec((1u64..5_000, 0u64..400, 1u64..2_000), 1..30),
            appended_late in any::<bool>(),
        ) {
            let mut bases = Vec::new(); // (base, receiver ordinal)
            let mut injs = Vec::new();
            let (mut base, mut at) = (origin, 10_000u64);
            for (dt, since, dat) in &steps {
                base = (base + dt) & MASK;
                at += dat;
                bases.push((base, at));
                injs.push(inj(base, *since));
            }
            let mut a = if appended_late {
                Attribution::strict(Vec::new(), &hdr())
            } else {
                Attribution::strict(injs.clone(), &hdr())
            };
            for (b, at) in &bases {
                a.on_pcr(*b, *at);
            }
            if appended_late {
                a.append(injs.clone());
            }
            for (k, (_, at)) in bases.iter().enumerate() {
                let (resolved, approx, stranded) = a.placement(k);
                prop_assert_eq!(resolved, Some(at + steps[k].1));
                prop_assert!(!approx && !stranded);
            }
        }

        /// (b) A late append whose anchor base has LEFT the 64-entry
        /// history (more than `MAX_APPROX_TICKS` older than the oldest
        /// retained base) is stranded, never placed.
        #[test]
        fn a_late_append_past_the_history_is_stranded(
            origin in origin(2_000_000),
            since in 0u64..400,
        ) {
            let mut a = Attribution::strict(Vec::new(), &hdr());
            let first = origin;
            for k in 0..70u64 {
                a.on_pcr((origin + k * 20_000) & MASK, 10_000 + k * 100);
            }
            a.append(vec![inj(first, since)]);
            let (resolved, _, stranded) = a.placement(0);
            prop_assert_eq!((resolved, stranded), (None, true));
        }

        /// (c) The half-range rule, for a receiver at any base `r` and a
        /// logged base `d` ticks BEHIND it on the circle (wrapping
        /// distance): exact at 0, approximate within `MAX_APPROX_TICKS`,
        /// stranded beyond. A base AHEAD by at most `MAX_FUTURE_TICKS` is
        /// still to come (unplaced, not stranded); further ahead means its
        /// age wrapped the counter, so it is stranded. Both distances are
        /// drawn in strata so the approximate tier and every boundary are
        /// reached on each run.
        #[test]
        fn half_range_rule_holds_across_a_wrap(
            r in prop_oneof![0..=MASK, 0..100_000u64, (MASK - 100_000)..=MASK],
            behind in prop_oneof![
                Just(0u64),
                1..=MAX_APPROX_TICKS,
                Just(MAX_APPROX_TICKS),
                Just(MAX_APPROX_TICKS + 1),
                (MAX_APPROX_TICKS + 1)..HALF,
                Just(HALF - 1),
            ],
            ahead in prop_oneof![
                1..=MAX_FUTURE_TICKS,
                Just(MAX_FUTURE_TICKS),
                Just(MAX_FUTURE_TICKS + 1),
                (MAX_FUTURE_TICKS + 1)..=HALF,
                Just(HALF),
            ],
            since in 0u64..600,
        ) {
            // behind
            let b = r.wrapping_sub(behind) & MASK;
            let mut a = Attribution::strict(vec![inj(b, since)], &hdr());
            a.on_pcr(r, 50_000);
            let (resolved, approx, stranded) = a.placement(0);
            if behind == 0 {
                prop_assert_eq!((resolved, approx, stranded), (Some(50_000 + since), false, false));
            } else if behind <= MAX_APPROX_TICKS {
                prop_assert_eq!((resolved, approx, stranded), (Some(50_000 + since), true, false));
            } else {
                prop_assert_eq!((resolved, stranded), (None, true));
            }
            // ahead
            let b = (r + ahead) & MASK;
            let mut a = Attribution::strict(vec![inj(b, since)], &hdr());
            a.on_pcr(r, 50_000);
            let (resolved, _, stranded) = a.placement(0);
            prop_assert_eq!(resolved, None);
            if ahead == HALF {
                // The antipode reads as "behind by 2^32": history, stranded.
                prop_assert!(stranded);
            } else {
                prop_assert_eq!(stranded, ahead > MAX_FUTURE_TICKS);
            }
        }
    }
}
