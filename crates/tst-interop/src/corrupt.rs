//! Seeded sender-side TS corruption tap (spec §4.1) and the receiver-side
//! attribution engine that judges what the corruption did (spec §4.3).
//!
//! The two halves are deliberately split by a FILE, not by a shared
//! in-memory object: [`Corrupter`] wraps the send-side [`Transport`] and
//! writes one JSONL line per injection; [`Attribution`] reads that log
//! back and matches it against the error events a receiver actually
//! surfaced. Nothing is carried across in process memory, so the same
//! judgement can be made live (`recv`), offline (`verify` over a capture),
//! or days later from an archived soak directory — and a receiver can
//! never "know" about an injection except through the same evidence a
//! human would read.
//!
//! # Why a packet-coordinate instead of a byte offset
//!
//! Sender byte offsets are useless at the receiver: the transport loses,
//! duplicates and reorders, and the corruption itself changes the wire
//! length (truncation, inserted garbage). Every injection is therefore
//! logged at a [`Coord`] — the most recent PCR base seen *before* the
//! packet, plus the number of packets since that PCR packet. PCR bases are
//! carried in the stream itself, so both ends can name the same instant
//! without a shared clock, and a receiver resolves a coordinate the moment
//! it sees that base (or, if the injection destroyed the packet carrying
//! it, the first base after it — see [`Attribution::on_pcr`]).
//!
//! Determinism is the other invariant: given the same seed, the same
//! config and the same input bytes, the tap emits byte-identical wire
//! output and a byte-identical log. Nothing here reads the clock.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tst_core::transport::{SocketStats, Transport, TransportCancel, TransportError};

use crate::impair::XorShift64;
use crate::rawts::{Reader, classify_packet};

const PKT: usize = 188;

/// Mixed into the run seed so the tap's PRNG stream is independent of every
/// other seeded component that shares the same run seed (the impairment
/// proxy's `Engine`, the generator's AU sizes). Without a per-component
/// salt, two components seeded identically would draw the same numbers and
/// their "independent" decisions would correlate.
pub const CORRUPT_SALT: u64 = 0xC0FF_EE00_0BAD_F00D;

/// How many receiver packets after a resolved injection an error event may
/// surface and still be blamed on it (spec §4.3). Wide enough to cover a
/// demuxer that only notices at the next PES/section boundary, narrow
/// enough that an unrelated event a second later is not silently excused.
pub const ATTRIBUTION_WINDOW: u64 = 500;

/// How many receiver packets after a resolved injection the stream must
/// have produced media again for the injection to count as recovered-from
/// (spec §4.3).
pub const RECOVERY_BOUND: u64 = 600;

/// Default `min_gap` — the floor on the packet distance between two
/// injections. Keeping injections farther apart than
/// [`ATTRIBUTION_WINDOW`] + [`RECOVERY_BOUND`] is what makes attribution
/// unambiguous: at most one injection's window can ever contain a given
/// event.
pub const DEFAULT_MIN_GAP: u64 = 1000;

/// Extra attribution slack granted to an injection whose PCR base the
/// receiver never saw, so it resolved against the *next* base instead. The
/// error is then bounded by one PCR interval; 128 packets is comfortably
/// more than the ≤100 ms interval H.222.0 §2.4.2.2 allows at any bitrate
/// this harness generates.
const APPROX_SLACK: u64 = 128;

/// How far ahead of an injection's own PCR base a later base may be and
/// still resolve it approximately — four 100 ms PCR intervals in 90 kHz
/// ticks (H.222.0 §2.4.2.2 caps the interval at 100 ms).
///
/// Without this bound a reconnect outage silently corrupts the verdict:
/// the sender keeps logging through it while the receiver sees nothing, so
/// the first PCR after the outage would resolve every injection logged
/// during it to that one receiver ordinal — and all but one would then be
/// reported `undetected` against a receiver that never had the chance to
/// see them. Beyond the bound an injection stays unresolved instead, which
/// means it is never judged.
pub const MAX_APPROX_TICKS: u64 = 4 * 9000;

/// Format version of the JSONL log. Bump when a field's MEANING changes
/// (adding a field does not need a bump — serde fills the rest from
/// `Default`); readers refuse a version they do not understand.
const TAP_VERSION: u32 = 1;

/// Cap on each of [`AttributionReport`]'s human-readable sample lists
/// (`unexplained_events`, `undetected`, `unrecovered`). Every one of them
/// has an uncapped counter beside it, so a badly-broken run is still
/// counted in full — only the quoted examples are bounded, and a
/// multi-day run cannot turn a report into a gigabyte of strings.
///
/// **A cap on a list must never become a cap on a JUDGEMENT.** The
/// verdicts read the counters, never `len()`; see
/// [`AttributionReport::unexplained_total`].
pub const MAX_SAMPLES: usize = 64;

/// Injections retired below [`Attribution`]'s `lo` cursor before their
/// accounting is folded into the running counters and their per-injection
/// state dropped. Batched rather than one at a time so the retained
/// prefix is removed with one memmove per batch instead of per injection;
/// the batch size is the entire bound on how much of the log the engine
/// holds once a run is under way.
pub const PRUNE_BATCH: usize = 1024;

// ============================================================
// Classes, config, parsing
// ============================================================

/// One corruption class — what the tap does to a packet it selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Flip 1-4 bytes of the packet payload.
    BodyFlip,
    /// Corrupt the 4-byte TS header (sync byte, PID, continuity counter,
    /// or adaptation-field length).
    Header,
    /// Emit only the first 40..=187 bytes of the packet.
    Truncate,
    /// Emit the packet, then a run of 1..=300 non-sync bytes after it.
    Garbage,
    /// Emit nothing for this packet.
    Drop,
    /// Emit the packet twice.
    Dup,
    /// Flip one byte inside a PAT/PMT section body, never its CRC.
    PsiFlip,
}

/// Relative selection weights. Body flips are the most common real-world
/// corruption (a bit error anywhere in the 184-byte payload is ~46× more
/// likely than one in the 4-byte header), and header damage is the next
/// most common; the structural classes are rarer but individually much
/// more disruptive, so they are kept at equal small weights rather than
/// scaled by probability.
const WEIGHTS: [(Class, u32); 7] = [
    (Class::BodyFlip, 30),
    (Class::Header, 20),
    (Class::Truncate, 10),
    (Class::Garbage, 10),
    (Class::Drop, 10),
    (Class::Dup, 10),
    (Class::PsiFlip, 10),
];

impl Class {
    /// Every class, in selection-weight order. The default class set.
    pub const ALL: [Class; 7] = [
        Class::BodyFlip,
        Class::Header,
        Class::Truncate,
        Class::Garbage,
        Class::Drop,
        Class::Dup,
        Class::PsiFlip,
    ];

    /// Inverse of [`Class::name`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Class> {
        Class::ALL.iter().copied().find(|c| c.name() == s)
    }

    /// Snake-case wire name — the spelling used on the command line, in
    /// the JSONL log, and in [`CorruptionStats::per_class`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Class::BodyFlip => "body_flip",
            Class::Header => "header",
            Class::Truncate => "truncate",
            Class::Garbage => "garbage",
            Class::Drop => "drop",
            Class::Dup => "dup",
            Class::PsiFlip => "psi_flip",
        }
    }
}

/// Tap configuration. `seed` is supplied by the caller (the run seed), not
/// parsed from the spec string, so every seeded component of a run shares
/// one number.
#[derive(Clone, Debug, PartialEq)]
pub struct CorruptConfig {
    /// Per-10 000 chance that an eligible packet is corrupted. `10_000`
    /// means "every eligible packet", which combined with `min_gap` is how
    /// the unit tests force exactly one injection.
    pub rate_per_10k: u32,
    /// Floor on the packet distance between consecutive injections.
    pub min_gap: u64,
    /// Classes to draw from; weights are re-normalised over this subset.
    pub classes: Vec<Class>,
    /// Run seed (salted with [`CORRUPT_SALT`] before use).
    pub seed: u64,
}

impl CorruptConfig {
    /// Reject configurations whose evidence could not be judged.
    ///
    /// The `min_gap` floor is the load-bearing one: with two injections
    /// closer together than one attribution window plus one recovery
    /// bound, an error event could legitimately belong to either, and the
    /// report would be guessing. Rather than produce an ambiguous verdict
    /// the tap refuses to run.
    ///
    /// `min_gap` also funds the window's BACKWARD half. An attribution
    /// window is `[r - backward_reach, r + ATTRIBUTION_WINDOW]`, and
    /// `Attribution::backward_reach` clamps its own extension to
    /// `min_gap - (ATTRIBUTION_WINDOW + APPROX_SLACK) - 1` so consecutive
    /// windows stay disjoint whatever `since_pcr` turns out to be — the
    /// `- 1` because both window edges are inclusive. At this floor that
    /// budget is 1000 - 628 - 1 = 371 packets, which is why the floor
    /// cannot be lowered without narrowing the backward half too.
    pub fn validate(&self) -> Result<(), String> {
        if self.rate_per_10k == 0 {
            return Err(
                "--corrupt: rate=0 means no corruption at all; omit --corrupt instead".into(),
            );
        }
        if self.rate_per_10k > 10_000 {
            return Err(format!(
                "--corrupt: rate={} is out of range (1..=10000 per ten thousand packets)",
                self.rate_per_10k
            ));
        }
        if self.min_gap < 2 * ATTRIBUTION_WINDOW || self.min_gap <= RECOVERY_BOUND {
            return Err(format!(
                "--corrupt: min_gap={} is too small; it must be >= {} (2 * attribution window) \
                 and > {} (recovery bound) so at most one injection can explain any event",
                self.min_gap,
                2 * ATTRIBUTION_WINDOW,
                RECOVERY_BOUND
            ));
        }
        if self.classes.is_empty() {
            return Err("--corrupt: classes= listed no classes".into());
        }
        Ok(())
    }
}

/// Parse `rate=PER_10K[,min_gap=PKTS][,classes=a+b+c]`.
///
/// Classes are `+`-separated, not comma-separated: the spec string itself
/// is split on commas, so a comma inside a value would be unparseable.
/// Unknown keys, a missing `rate=`, and any out-of-range value are all
/// errors — a typo must not silently degrade into "no corruption", which
/// would make a whole soak run's evidence vacuous.
pub fn parse_corrupt(s: &str, seed: u64) -> Result<CorruptConfig, String> {
    let mut rate = None;
    let mut min_gap = DEFAULT_MIN_GAP;
    let mut classes = Class::ALL.to_vec();
    for part in s.split(',') {
        let (key, val) = part.split_once('=').ok_or_else(|| {
            format!("--corrupt: `{part}` is not key=value (rate=N,min_gap=N,classes=a+b)")
        })?;
        match key {
            "rate" => {
                rate = Some(
                    val.parse::<u32>()
                        .map_err(|e| format!("--corrupt: rate={val}: {e}"))?,
                );
            }
            "min_gap" => {
                min_gap = val
                    .parse::<u64>()
                    .map_err(|e| format!("--corrupt: min_gap={val}: {e}"))?;
            }
            "classes" => {
                classes = val
                    .split('+')
                    .map(|c| {
                        Class::parse(c).ok_or_else(|| {
                            format!(
                                "--corrupt: unknown class `{c}` (one of: {})",
                                Class::ALL
                                    .iter()
                                    .map(|c| c.name())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
            }
            _ => return Err(format!("--corrupt: unknown key `{key}`")),
        }
    }
    let cfg = CorruptConfig {
        rate_per_10k: rate.ok_or_else(|| "--corrupt: rate= is required".to_string())?,
        min_gap,
        classes,
        seed,
    };
    cfg.validate()?;
    Ok(cfg)
}

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

/// First JSONL line of the log — `{"header":{...}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogHeader {
    pub tap_version: u32,
    pub seed: u64,
    pub rate_per_10k: u32,
    pub min_gap: u64,
    pub classes: Vec<Class>,
    pub attribution_window: u64,
    pub recovery_bound: u64,
}

/// Counters for the whole tap run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CorruptionStats {
    pub packets_seen: u64,
    pub injections: u64,
    pub detectable: u64,
    pub per_class: BTreeMap<String, u64>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Packets the tap could not classify and therefore passed through
    /// untouched. Always `0` for this harness's own muxer output; a
    /// non-zero value means something upstream already emitted a
    /// malformed packet, which would invalidate the run's evidence.
    pub passthrough_unclassified: u64,
}

/// One line of the JSONL log. Serde's external tagging renders this as
/// exactly `{"header":{…}}` / `{"injection":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LogLine {
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
/// error). Feed what comes back to [`Attribution::append`].
///
/// Waiting for the file to EXIST is the caller's job: the sender creates
/// it, and which process starts first is the caller's arrangement to
/// make, not something a reader can paper over by blocking.
#[derive(Debug)]
pub struct LogTail {
    file: std::fs::File,
    path: std::path::PathBuf,
    header: LogHeader,
    /// Bytes read but not yet terminated by a newline.
    carry: Vec<u8>,
    /// 1-based number of the next line, for error messages that name the
    /// same line the offline parser would.
    next_line: usize,
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
            file,
            path: path.to_path_buf(),
            // Replaced below; a placeholder keeps `read_more` usable.
            header: LogHeader {
                tap_version: TAP_VERSION,
                seed: 0,
                rate_per_10k: 0,
                min_gap: 0,
                classes: Vec::new(),
                attribution_window: ATTRIBUTION_WINDOW,
                recovery_bound: RECOVERY_BOUND,
            },
            carry: Vec::new(),
            next_line: 1,
        };
        tail.read_more()?;
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
    /// bound an [`Attribution`] must judge by.
    #[must_use]
    pub fn header(&self) -> &LogHeader {
        &self.header
    }

    /// Injections appended since the previous call, in log order.
    pub fn poll(&mut self) -> Result<Vec<Injection>, String> {
        self.read_more()?;
        let mut out = Vec::new();
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

    /// Append everything readable right now to `carry`.
    fn read_more(&mut self) -> Result<(), String> {
        use std::io::Read;
        let mut buf = [0u8; 64 * 1024];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => self.carry.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(format!("corruption log {}: {e}", self.path.display()));
                }
            }
        }
    }

    /// Take the next NEWLINE-TERMINATED line out of `carry`, parsed. A
    /// trailing unterminated line stays put for a later poll.
    fn next_complete_line(&mut self) -> Result<Option<LogLine>, String> {
        while let Some(nl) = self.carry.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.carry.drain(..=nl).collect();
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
        Ok(None)
    }
}

// ============================================================
// The tap
// ============================================================

/// What one mutation produced.
struct Mutation {
    /// Bytes that replace the original packet on the wire (empty for a
    /// drop, two packets for a dup, packet+garbage for a garbage run).
    wire: Vec<u8>,
    /// Bytes recorded as [`Injection::after`].
    after: Vec<u8>,
    offsets: Vec<usize>,
    detectable: bool,
}

/// Sender-side corruption tap: a [`Transport`] adapter that mutates whole
/// 188-byte TS packets on their way to the real transport and logs exactly
/// what it did.
///
/// # Input invariant
///
/// `send_bytes` expects a whole number of 188-byte packets — which is what
/// `MuxSender` always produces. A non-multiple tail is passed through
/// untouched (and trips a `debug_assert`), so a mis-framed caller degrades
/// to "no corruption" rather than to garbage.
///
/// One push may leave as more than one: the tap can grow a message
/// (duplication, garbage insertion) past the inner transport's
/// `max_payload`, so the output is split into several `send_bytes` calls
/// of at most that size, cut on packet boundaries wherever the content
/// still has them.
///
/// # Error contract
///
/// An error from the inner transport propagates unchanged, but
/// [`TransportError::Backpressure`]'s "the bytes were not consumed, retry
/// the same slice" contract does NOT survive the tap: by the time a later
/// slice is refused, earlier slices of the same push are already on the
/// wire and the tap's PRNG has advanced. Callers of a corrupted sender
/// must treat any send error as fatal to the run — which is what the soak
/// harness does.
pub struct Corrupter<T: Transport> {
    inner: T,
    cfg: CorruptConfig,
    rng: XorShift64,
    /// Fed the ORIGINAL bytes of every packet, in non-resync mode, purely
    /// to learn the PAT's PMT PIDs. It never sees a mutated byte, so it
    /// cannot be knocked out by the corruption it is helping to place.
    reader: Reader,
    /// Sender packet ordinal of the next packet.
    ordinal: u64,
    /// Most recent PCR base seen strictly before the next packet.
    base: Option<u64>,
    /// Packets since the packet that carried `base` (or since the start of
    /// the stream while `base` is `None`).
    since: u64,
    last_injection: Option<u64>,
    /// A [`Class::PsiFlip`] drawn on a packet that was not PAT/PMT, held
    /// until one arrives. Re-drawing a different class instead would bias
    /// the mix away from `psi_flip` by exactly the fraction of non-PSI
    /// packets — i.e. almost all of them.
    pending_psi: Option<Class>,
    log: Box<dyn Write + Send>,
    stats: Arc<Mutex<CorruptionStats>>,
}

impl<T: Transport> Corrupter<T> {
    /// Wrap `inner`, writing the log header line immediately so a log file
    /// is self-describing even if the sender dies before the first
    /// injection.
    pub fn new(inner: T, cfg: CorruptConfig, log: Box<dyn Write + Send>) -> Result<Self, String> {
        cfg.validate()?;
        let mut me = Corrupter {
            inner,
            rng: XorShift64::new(cfg.seed ^ CORRUPT_SALT),
            reader: Reader::new(),
            ordinal: 0,
            base: None,
            since: 0,
            last_injection: None,
            pending_psi: None,
            log,
            stats: Arc::new(Mutex::new(CorruptionStats::default())),
            cfg,
        };
        let header = LogHeader {
            tap_version: TAP_VERSION,
            seed: me.cfg.seed,
            rate_per_10k: me.cfg.rate_per_10k,
            min_gap: me.cfg.min_gap,
            classes: me.cfg.classes.clone(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
        };
        me.write_line(&LogLine::Header(header))
            .map_err(|e| format!("writing the corruption log header: {e}"))?;
        Ok(me)
    }

    /// Snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> CorruptionStats {
        self.stats.lock().expect("corruption stats mutex").clone()
    }

    /// The live counters. Kept up to date as packets flow, so a caller that
    /// no longer owns the tap (it was moved into a `MuxSender` that has
    /// since been dropped) can still read the final numbers.
    #[must_use]
    pub fn stats_handle(&self) -> Arc<Mutex<CorruptionStats>> {
        Arc::clone(&self.stats)
    }

    /// Unwrap the inner transport.
    ///
    /// There is deliberately no `Drop` impl to flush anything here: every
    /// log line is flushed as it is written and the stats live behind the
    /// `Arc`, so a tap that is dropped (or leaked, or killed mid-run)
    /// leaves complete evidence either way.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn write_line(&mut self, line: &LogLine) -> std::io::Result<()> {
        let json = serde_json::to_string(line).expect("log line serializes");
        self.log.write_all(json.as_bytes())?;
        self.log.write_all(b"\n")?;
        // Flushed per line, not per run: a soak sender can be killed at
        // any moment and the log must still explain every injection that
        // reached the wire.
        self.log.flush()
    }

    fn is_psi_pid(&self, pid: u16) -> bool {
        pid == 0 || self.reader.is_pmt_pid(pid)
    }

    /// Weighted draw over the configured subset (weights re-normalise).
    fn pick_class(&mut self) -> Class {
        let eligible: Vec<(Class, u32)> = WEIGHTS
            .iter()
            .copied()
            .filter(|(c, _)| self.cfg.classes.contains(c))
            .collect();
        let total: u32 = eligible.iter().map(|(_, w)| w).sum();
        let mut x = (self.rng.next_u64() % u64::from(total)) as u32;
        for &(c, w) in &eligible {
            if x < w {
                return c;
            }
            x -= w;
        }
        eligible
            .last()
            .expect("validate() guarantees a non-empty class set")
            .0
    }

    /// Apply `class` to `pkt`. `None` means "not applicable to this
    /// packet" — only [`Class::PsiFlip`] can say that, and only when the
    /// PSI packet turns out not to carry a single complete section; the
    /// caller then keeps the class pending. No PRNG draw happens on that
    /// path, so the decision stays deterministic.
    fn mutate(
        &mut self,
        class: Class,
        pkt: &[u8; PKT],
        info: &crate::rawts::PacketInfo,
    ) -> Option<Mutation> {
        let mut p = *pkt;
        match class {
            Class::BodyFlip => {
                // Payload bytes only; a packet with no payload still has
                // adaptation-field bytes past the 4-byte header worth
                // flipping.
                //
                // On a packet that STARTS a PES, the flip also skips the
                // PES header. A flip there is undetectable by contract —
                // tst-core's PES parser accepts `stream_id`,
                // `PES_packet_length`, `header_data_length` and 33 of the
                // 40 PTS bits as-is (see `sensitive_span`) — yet it
                // silently rewrites a PTS or a stream_id, and the WIRE
                // ORACLES read those same bytes structurally: a PTS the
                // parser shrugs at makes `pts_wrap_unexpected` fire, and a
                // rewritten stream_id makes `av1_carriage_wire` fire. The
                // run would then fail for damage the tap itself declared
                // nobody has to notice. Corrupting the picture is what
                // "body" means; corrupting the timing is a different
                // experiment, and not one this harness can judge.
                //
                // On a PSI packet the flip skips the section HEADER for
                // the mirror-image reason. tst-core's `parse_pat` /
                // `parse_pmt` check `table_id` and `section_length`
                // BEFORE the CRC, and `psi_topology.rs`'s
                // `Err(_) => return` arm drops every one of those
                // rejections silently — only `CrcMismatch` and
                // `MultiSectionUnsupported` become `NonConformant`
                // events. So a flip that lands on the pointer field, the
                // table_id or either section_length byte takes the whole
                // section out of play, and any OTHER byte the same
                // injection flipped can never be observed either.
                //
                // Measured, not theorised: the 1-hour re-smoke on main
                // `6f0fbeb7` failed `corruption_detected` on exactly this
                // shape — injection ordinal 2938545, a `body_flip` on the
                // PMT PID with offsets [5, 14, 118, 161], where 5 IS the
                // table_id (0x02 -> 0xd5). Offset 14 sits inside the
                // CRC'd span and made the injection `detectable`, but the
                // receiver had already dropped the section on the
                // table_id and nothing could surface. The tap was
                // over-claiming, so the tap is what changes.
                let lo = if !info.has_payload {
                    4
                } else if let Some(body) =
                    psi_section_body_start(pkt, info, self.is_psi_pid(info.pid))
                {
                    body
                } else if pes_start(pkt, info) {
                    // §2.4.3.7: 3-byte start code, stream_id,
                    // PES_packet_length(2), two flag bytes, then
                    // header_data_length and that many optional bytes.
                    // The elementary-stream payload starts after all of
                    // it.
                    match pkt.get(info.payload_off + 8) {
                        Some(&hdr_len) => {
                            let es = info.payload_off + 9 + usize::from(hdr_len);
                            // A header that fills the packet leaves no ES
                            // payload to flip. Fall back to the last four
                            // bytes rather than skipping the injection:
                            // the draw below must consume the same number
                            // of PRNG values on every path or the whole
                            // stream of decisions would depend on packet
                            // shape.
                            if es < PKT { es } else { PKT - 4 }
                        }
                        None => PKT - 4,
                    }
                } else {
                    info.payload_off.max(4)
                };
                let n = 1 + (self.rng.next_u64() % 4) as usize;
                let mut offsets: Vec<usize> = Vec::with_capacity(n);
                for _ in 0..n {
                    // Both draws happen even when the offset repeats, so
                    // the PRNG stream does not depend on the collision.
                    let o = lo + (self.rng.next_u64() % (PKT - lo) as u64) as usize;
                    let x = 1 + (self.rng.next_u64() % 255) as u8;
                    if offsets.contains(&o) {
                        continue;
                    }
                    offsets.push(o);
                    p[o] ^= x;
                }
                offsets.sort_unstable();
                // Only a flip that lands under a CRC can be called
                // detectable. A PAT/PMT packet is ~90% 0xFF stuffing,
                // and everything outside a PSI section is media bytes or
                // loosely-validated PES header fields; a flip there
                // corrupts the picture but breaks no syntax anything
                // checks, and claiming otherwise would make a conformant
                // receiver fail the run.
                let detectable = sensitive_span(pkt, info, self.is_psi_pid(info.pid))
                    .is_some_and(|(lo, hi)| offsets.iter().any(|&o| (lo..hi).contains(&o)));
                Some(Mutation {
                    wire: p.to_vec(),
                    after: p.to_vec(),
                    offsets,
                    detectable,
                })
            }
            Class::Header => {
                let psi = self.is_psi_pid(info.pid);
                let mut offsets = Vec::new();
                let mut kind = None;
                for _ in 0..8 {
                    let mut k = self.rng.next_u64() % 4;
                    // Sub-kind 3 rewrites the adaptation_field_length,
                    // which only exists when there IS an adaptation field.
                    if k == 3 && info.afc & 0x2 == 0 {
                        k = 2;
                    }
                    // Sub-kinds 1 and 2 are both noticed as a continuity
                    // jump on the packet's own PID — 2 by rewriting the
                    // counter, 1 by moving the packet off the PID so the
                    // counter appears to skip one — and §2.4.3.3 advances
                    // the counter only on packets that CARRY PAYLOAD. On
                    // an adaptation-field-only packet (this harness's
                    // muxer emits PCR-only catch-up packets — see
                    // `mux/scheduling.rs`) neither leaves a trace, so both
                    // would be undetectable by construction. Re-roll the
                    // SUB-KIND — never the class, which is already
                    // committed by the weighted draw.
                    if matches!(k, 1 | 2) && !info.has_payload {
                        continue;
                    }
                    kind = Some(k);
                    break;
                }
                // On a PAT/PMT packet only the sync byte is used. The other
                // three sub-kinds are INVISIBLE there, which would make the
                // tap manufacture failures against a conformant receiver:
                //
                // - A continuity_counter jump is only ever reported for a
                //   resolved elementary stream (`tst_core`'s
                //   `mpegts::demux::sync_ingress` looks the PID up and
                //   drops the event when it finds nothing), and a PSI PID
                //   is not one. Measured: a CC jump on PID 0 produces no
                //   demux event at all.
                // - Rewriting the PID moves the packet off the PSI PID
                //   entirely, so the section simply never arrives — the
                //   next repetition of the table covers for it, and a
                //   receiver has nothing to report.
                // - An adaptation_field_length overrun is only reachable
                //   when the packet HAS an adaptation field, which this
                //   harness's PSI packets do not.
                //
                // A destroyed sync byte, by contrast, is caught
                // deterministically by the raw reader as a resync. The
                // sub-kind is still DRAWN above on this path, so the PRNG
                // stream does not depend on which PID the draw landed on;
                // only the arm taken changes. On media PIDs all four
                // sub-kinds stay in play.
                let kind = if psi { 0 } else { kind.unwrap_or(0) };
                // A sync-byte flip is valid on every packet, so it is also
                // the fallback if the re-rolls kept landing on CC.
                match kind {
                    0 => {
                        let x = 1 + (self.rng.next_u64() % 255) as u8;
                        p[0] = 0x47 ^ x;
                        offsets.push(0);
                    }
                    1 => {
                        // 0x1FFE: unassigned, and deliberately not 0x1FFF
                        // (the null PID), which a demuxer would silently
                        // discard instead of reporting.
                        p[1] = (p[1] & 0xE0) | 0x1F;
                        p[2] = 0xFE;
                        offsets.extend([1, 2]);
                    }
                    2 => {
                        // +2..+7 — never +1 (which would look correct) and
                        // never +0 (a legal duplicate).
                        let cc = (info.cc + 2 + (self.rng.next_u64() % 6) as u8) & 0x0F;
                        p[3] = (p[3] & 0xF0) | cc;
                        offsets.push(3);
                    }
                    _ => {
                        // 184..187: 5 + af_len then overruns the 188-byte
                        // packet, which every conformant parser must reject.
                        p[4] = 184 + (self.rng.next_u64() % 4) as u8;
                        offsets.push(4);
                    }
                }
                Some(Mutation {
                    wire: p.to_vec(),
                    after: p.to_vec(),
                    offsets,
                    detectable: true,
                })
            }
            Class::Truncate => {
                // 40 bytes minimum so the header and a little payload
                // survive: a receiver must resync, not merely see a short
                // read it could mistake for the end of the stream.
                let n = 40 + (self.rng.next_u64() % 148) as usize;
                let wire = p[..n].to_vec();
                Some(Mutation {
                    after: wire.clone(),
                    wire,
                    offsets: Vec::new(),
                    detectable: true,
                })
            }
            Class::Garbage => {
                let n = 1 + (self.rng.next_u64() % 300) as usize;
                let mut g = Vec::with_capacity(n);
                for _ in 0..n {
                    let b = ((self.rng.next_u64() % 255) as u8).wrapping_add(1);
                    // Never 0x47: inserted garbage must not fake a packet
                    // start, or the receiver would resync onto it and the
                    // damage would be a different class than the one
                    // logged.
                    g.push(if b == 0x47 { 0x48 } else { b });
                }
                let mut wire = p.to_vec();
                wire.extend_from_slice(&g);
                Some(Mutation {
                    wire,
                    after: g,
                    offsets: Vec::new(),
                    detectable: true,
                })
            }
            Class::Drop => Some(Mutation {
                wire: Vec::new(),
                after: Vec::new(),
                offsets: Vec::new(),
                // A dropped packet is noticed as a continuity jump and
                // nothing else (see `expects`), so it is only detectable
                // where a continuity jump is actually reportable — the
                // same two conditions the `Header` class already applies
                // to its continuity-counter sub-kind:
                //
                // - On a MEDIA PID. A jump is only ever reported for a
                //   resolved elementary stream, and a lost PAT/PMT
                //   repetition is covered by the next one, so a conformant
                //   receiver has nothing to say about either.
                // - On a packet WITH PAYLOAD. §2.4.3.3 advances the
                //   counter only on packets that carry payload, so
                //   dropping an adaptation-field-only packet (this
                //   harness's muxer emits PCR-only catch-up packets — see
                //   `mux/scheduling.rs`) leaves the counter sequence
                //   intact and there is no jump to report.
                detectable: !self.is_psi_pid(info.pid) && info.has_payload,
            }),
            Class::Dup => {
                let mut wire = p.to_vec();
                wire.extend_from_slice(&p);
                Some(Mutation {
                    wire,
                    after: p.to_vec(),
                    offsets: Vec::new(),
                    // A repeated packet with the same continuity counter is
                    // LEGAL (§2.4.3.3 allows one duplicate); a receiver that
                    // says nothing is conformant.
                    detectable: false,
                })
            }
            Class::PsiFlip => {
                let (lo, hi) = psi_body_range(&p, info)?;
                let o = lo + (self.rng.next_u64() % (hi - lo) as u64) as usize;
                let x = 1 + (self.rng.next_u64() % 255) as u8;
                p[o] ^= x;
                Some(Mutation {
                    wire: p.to_vec(),
                    after: p.to_vec(),
                    offsets: vec![o],
                    // The CRC is left intact on purpose, so the section
                    // fails its checksum — a silent CRC "fix" would make
                    // the corruption invisible and the evidence worthless.
                    detectable: true,
                })
            }
        }
    }

    /// Mutate a whole push and return the bytes to put on the wire.
    fn transform(&mut self, msg: &[u8]) -> Vec<u8> {
        debug_assert!(
            msg.len() % PKT == 0,
            "corruption tap expects whole TS packets, got {} bytes",
            msg.len()
        );
        let mut out = Vec::with_capacity(msg.len() + 512);
        // Cloned so the guard borrows the Arc, not `self` — the loop below
        // needs `&mut self` for the PRNG, the reader and the log.
        let stats_arc = Arc::clone(&self.stats);
        let mut stats = stats_arc.lock().expect("corruption stats mutex");
        let mut packets = msg.chunks_exact(PKT);
        for chunk in packets.by_ref() {
            let pkt: [u8; PKT] = chunk.try_into().expect("chunks_exact(188)");
            stats.packets_seen += 1;
            let ordinal = self.ordinal;
            self.ordinal += 1;

            let Ok(info) = classify_packet(&pkt) else {
                stats.passthrough_unclassified += 1;
                out.extend_from_slice(&pkt);
                continue;
            };
            // PAT/PMT packets only: the PAT->PMT map is all this reader
            // is for, and feeding it the whole stream would accumulate
            // every PCR and PTS of a 72-hour soak in its summary. The
            // reader only ever sees pristine bytes; a feed error is
            // impossible for this harness's own muxer output, but if one
            // happened the failing packet would stay in its carry and
            // poison every later feed, so clear it.
            if self.is_psi_pid(info.pid) && self.reader.feed(&pkt).is_err() {
                self.reader.resync();
            }
            let coord = Coord {
                pcr_base: self.base,
                since_pcr: self.since,
            };
            match info.pcr_base {
                Some(b) => {
                    self.base = Some(b);
                    self.since = 1;
                }
                None => self.since += 1,
            }

            let mut class = None;
            if self.pending_psi.is_some() {
                if self.is_psi_pid(info.pid) {
                    class = self.pending_psi.take();
                }
            } else if self
                .last_injection
                .is_none_or(|l| ordinal - l >= self.cfg.min_gap)
                && self.rng.next_u64() % 10_000 < u64::from(self.cfg.rate_per_10k)
            {
                let picked = self.pick_class();
                if picked == Class::PsiFlip && !self.is_psi_pid(info.pid) {
                    self.pending_psi = Some(picked);
                } else {
                    class = Some(picked);
                }
            }

            let Some(class) = class else {
                out.extend_from_slice(&pkt);
                continue;
            };
            let Some(m) = self.mutate(class, &pkt, &info) else {
                // PSI packet without a usable single section — wait for
                // the next one.
                self.pending_psi = Some(class);
                out.extend_from_slice(&pkt);
                continue;
            };
            out.extend_from_slice(&m.wire);
            let injection = Injection {
                ordinal,
                coord,
                class,
                pid: info.pid,
                offsets: m.offsets,
                before: pkt.to_vec(),
                after: m.after,
                detectable: m.detectable,
                psi: self.is_psi_pid(info.pid),
                pes_start: pes_start(&pkt, &info),
            };
            stats.injections += 1;
            if injection.detectable {
                stats.detectable += 1;
            }
            *stats.per_class.entry(class.name().to_string()).or_insert(0) += 1;
            self.last_injection = Some(ordinal);
            if let Err(e) = self.write_line(&LogLine::Injection(injection)) {
                // Losing a line cannot be repaired here, and must not stop
                // the run. It fails LOUD rather than silent: the receiver
                // will report the resulting event as unexplained, which is
                // a FAIL verdict.
                tracing::error!("corruption log write failed at packet {ordinal}: {e}");
            }
        }
        out.extend_from_slice(packets.remainder());
        stats.bytes_in += msg.len() as u64;
        stats.bytes_out += out.len() as u64;
        out
    }

    fn emit(&mut self, out: &[u8]) -> Result<(), TransportError> {
        if out.is_empty() {
            return Ok(());
        }
        let max = self.inner.max_payload();
        // Cut on a packet boundary where the budget allows one, so a
        // normal push stays one push and only a grown one splits.
        let cut = if max >= PKT {
            (max / PKT) * PKT
        } else {
            max.max(1)
        };
        for slice in out.chunks(cut) {
            self.inner.send_bytes(slice)?;
        }
        Ok(())
    }
}

/// PUSI set and the payload starting with a PES start code (§2.4.3.7).
fn pes_start(p: &[u8; PKT], info: &crate::rawts::PacketInfo) -> bool {
    info.pusi && info.has_payload && p[info.payload_off..].starts_with(&[0, 0, 1])
}

/// The byte range of a packet whose corruption a conformant receiver MUST
/// notice — the only range of a transport stream a receiver is guaranteed
/// to checksum.
///
/// That is a PSI section's BODY and CRC32, and nothing else. Everything
/// after the section is stuffing (a PAT in this harness's own multiplex is
/// 17 bytes of section and 167 bytes of 0xFF), and everything before the
/// body — the pointer field and the 3-byte section header — is read to
/// FIND the section, so damaging it makes a receiver discard the section
/// before it ever reaches the CRC:
///
/// - A flipped `table_id` is a `TableIdMismatch`, and a flipped
///   `section_length` a `SectionTooLong`/`SectionTooShort`/`Truncated` (or
///   an assembler still waiting for continuation bytes that never come).
///   `tst_core`'s `mpegts::demux::psi` raises all of those BEFORE the CRC
///   check, and `psi_topology`'s `handle_pat_section`/`handle_pmt_section`
///   surface only `CrcMismatch` as an event — every other parse error is
///   dropped, which is the ordinary behaviour of a PSI filter skipping a
///   table it does not recognise.
/// - A flipped pointer field redirects the section start into this
///   harness's 0xFF stuffing, where `table_id == 0xFF` takes the same
///   silent path.
///
/// Measured, not assumed: a flip at the PAT's `table_id` and one at its
/// `section_length` each produce zero demux events, while a flip one byte
/// later — the first body byte — produces `PsiChecksumMismatch`. Claiming
/// the header detectable would fail a conformant receiver.
///
/// This is [`psi_body_range`] plus the CRC: that function deliberately
/// stops short of the CRC so a [`Class::PsiFlip`] leaves the checksum
/// intact and the section genuinely fails it; here the CRC bytes belong in
/// the span, because flipping one of them also fails the checksum.
///
/// A PES header deliberately does NOT count, even though it is "syntax":
/// tst-core's PES parser (`mpegts/demux/pes.rs`) validates only the start
/// code, the '10' marker bits, `PTS_DTS_flags` and the PTS prefix/marker
/// bits — `stream_id`, `PES_packet_length`, `header_data_length` and 33 of
/// the 40 PTS bits are all accepted as-is. Roughly half the header is
/// therefore silently tolerated, and claiming it detectable would
/// manufacture failures against a conformant receiver. Under-claiming
/// costs nothing: an event inside the attribution window is still
/// attributed regardless of `detectable`.
///
/// `psi` cannot be derived from the packet alone (a PMT PID is learned
/// from the PAT), so the caller supplies it. `None` means "nothing here is
/// required to be noticed", which is the honest answer for nearly every
/// packet in a transport stream.
fn sensitive_span(
    p: &[u8; PKT],
    info: &crate::rawts::PacketInfo,
    psi: bool,
) -> Option<(usize, usize)> {
    if !psi || !info.has_payload || !info.pusi {
        return None;
    }
    let sec = info.payload_off + 1 + usize::from(*p.get(info.payload_off)?);
    let len = (usize::from(*p.get(sec + 1)? & 0x0F) << 8) | usize::from(*p.get(sec + 2)?);
    let (lo, hi) = (sec + 3, (sec + 3 + len).min(PKT));
    (lo < hi).then_some((lo, hi))
}

/// First byte of a PSI section's BODY — one past the 3-byte section
/// header (`table_id`, then the two `section_syntax_indicator` /
/// `section_length` bytes).
///
/// The section itself starts at `payload_off + 1 + pointer_field`
/// (H.222.0 §2.4.4.1): the payload's first byte is the `pointer_field`,
/// and its VALUE is how many further bytes of the previous section's tail
/// stand between it and this one. This harness's generator always emits a
/// zero pointer, but the arithmetic is the general one.
///
/// `None` for anything that is not the start of a section on a PSI PID,
/// and for the degenerate case where the packet leaves no body byte at
/// all after that header — callers fall back to their ordinary range
/// there rather than skipping the injection, because the draw must
/// consume the same number of PRNG values on every path.
///
/// Used by [`Class::BodyFlip`] to keep its flips off the bytes a decoder
/// reads to FIND the section. Deliberately a different question from
/// [`sensitive_span`], which answers "would a flip here be noticed" and
/// therefore spans the body AND the CRC: this one answers "may a flip
/// land here at all".
fn psi_section_body_start(
    p: &[u8; PKT],
    info: &crate::rawts::PacketInfo,
    psi: bool,
) -> Option<usize> {
    if !psi || !info.has_payload || !info.pusi {
        return None;
    }
    let sec = info.payload_off + 1 + usize::from(*p.get(info.payload_off)?);
    let body = sec.checked_add(3)?;
    (body < PKT).then_some(body)
}

/// Byte range of a PSI section's body, excluding the 3-byte section header
/// and the trailing CRC32. `None` when the packet does not carry the start
/// of a single complete section.
fn psi_body_range(p: &[u8; PKT], info: &crate::rawts::PacketInfo) -> Option<(usize, usize)> {
    if !info.has_payload || !info.pusi {
        return None;
    }
    let sec = info.payload_off + 1 + usize::from(*p.get(info.payload_off)?);
    let len = (usize::from(*p.get(sec + 1)? & 0x0F) << 8) | usize::from(*p.get(sec + 2)?);
    // 9 = table_id_ext(2) + version/current_next(1) + section_number(1) +
    // last_section_number(1) + CRC32(4): the minimum any PSI section can be.
    if len < 9 {
        return None;
    }
    let lo = sec + 3;
    let hi = lo + len - 4;
    if hi > PKT || lo >= hi {
        return None;
    }
    Some((lo, hi))
}

impl<T: Transport> Transport for Corrupter<T> {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        let out = self.transform(msg);
        self.emit(&out)
    }

    /// The INNER transport's budget: the sender upstream must keep sizing
    /// its pushes for the real wire, and the tap splits its own (possibly
    /// larger) output afterwards.
    fn max_payload(&self) -> usize {
        self.inner.max_payload()
    }

    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    fn close(&mut self) {
        let _ = self.log.flush();
        self.inner.close();
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        self.inner.cancel_handle()
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        self.inner.socket_stats()
    }
}

// ============================================================
// Attribution
// ============================================================

/// Receiver-side error-event kinds the attribution engine understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// The raw reader lost and regained packet sync.
    Resync,
    ContinuityJump,
    OtherDiscontinuity,
    PsiChecksum,
    MalformedPes,
    OtherNonConformant,
}

/// What the receiver's evidence says about the injections.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttributionReport {
    pub injected: u64,
    pub detectable: u64,
    pub resolved: u64,
    pub unresolved: u64,
    pub events: u64,
    pub attributed_events: u64,
    /// Attributed events whose signal was a non-conformance
    /// (`PsiChecksum` / `MalformedPes` / `OtherNonConformant`).
    pub attributed_nonconformant: u64,
    /// Attributed events whose signal was a discontinuity
    /// (`ContinuityJump` / `OtherDiscontinuity`).
    pub attributed_discontinuities: u64,
    /// Sample of events no injection explains and no transport-loss
    /// excusal covers — capped at [`MAX_SAMPLES`]. The true count is
    /// [`unexplained_total`](Self::unexplained_total); never judge a run
    /// by this list's length.
    pub unexplained_events: Vec<String>,
    /// Uncapped count of unexplained `Resync` events. A resync is the one
    /// signal with no second detection path — it is not a `DemuxEvent`,
    /// and in resync mode it does not surface as `rawts_sync_loss` either
    /// — so it must never be lost behind a sample cap.
    #[serde(default)]
    pub unexplained_resyncs: u64,
    /// Uncapped count of unexplained discontinuity-family events that
    /// were NOT excused as transport loss (always the whole family under
    /// strict judgement; always zero under lossy, where the excusal takes
    /// them all — see [`unexplained_transport_loss`](Self::unexplained_transport_loss)).
    #[serde(default)]
    pub unexplained_discontinuities: u64,
    /// Uncapped count of unexplained non-conformance-family events
    /// (`PsiChecksum` / `MalformedPes` / `OtherNonConformant`). Never
    /// excused in either tier: a lost packet does not forge a bad CRC.
    #[serde(default)]
    pub unexplained_nonconformant: u64,
    /// First UNEXPLAINED event of each family, uncapped and
    /// first-writer-wins. A verifier's surviving
    /// `nonconformant_event`/`discontinuity_event` failures quote these:
    /// the first event of a class and the first UNEXPLAINED event of it
    /// are routinely different events, and naming the explained one would
    /// point a reader at evidence the report has already accounted for.
    pub first_unexplained_nonconformant: Option<String>,
    pub first_unexplained_discontinuity: Option<String>,
    /// Sample of injections a conformant receiver had to notice and did
    /// not — capped at [`MAX_SAMPLES`];
    /// [`undetected_count`](Self::undetected_count) is the true number.
    pub undetected: Vec<String>,
    /// Uncapped count of the same.
    #[serde(default)]
    pub undetected_count: u64,
    /// Sample of injections the stream never produced media after —
    /// capped at [`MAX_SAMPLES`];
    /// [`unrecovered_count`](Self::unrecovered_count) is the true number.
    pub unrecovered: Vec<String>,
    /// Uncapped count of the same.
    #[serde(default)]
    pub unrecovered_count: u64,
    /// Injections that would have landed in [`undetected`](Self::undetected)
    /// but were excused as lost in transit — a `ContinuityJump` inside
    /// their attribution window says the transport itself dropped packets
    /// there, and a corrupted packet the network never delivered cannot be
    /// noticed by its damage. Only ever nonzero for a report finished with
    /// transport-loss excusal on (see [`Attribution::lossy`]).
    #[serde(default)]
    pub undetected_lost: u64,
    /// Same excusal, for [`unrecovered`](Self::unrecovered): media that a
    /// dropped-packet gap swallowed is not the injection failing to
    /// recover.
    #[serde(default)]
    pub unrecovered_lost: u64,
    /// Detectable injections the receiver's OWN demuxer reported (a
    /// `Discontinuity` / `NonConformant` event inside the window).
    #[serde(default)]
    pub detected_by_demux: u64,
    /// Detectable injections noticed ONLY by the harness's raw-TS reader
    /// ([`Signal::Resync`], which is not a `DemuxEvent`): garbage runs and
    /// destroyed sync bytes that tst-core re-syncs past without an event,
    /// by design. Reported separately so the evidence page does not
    /// credit the receiver with the harness's own detection (META-13).
    #[serde(default)]
    pub detected_by_reader_only: u64,
    /// The same, per class name ([`Class::name`]).
    #[serde(default)]
    pub detected_by_reader_only_per_class: BTreeMap<String, u64>,
    /// Unexplained events kept out of
    /// [`unexplained_events`](Self::unexplained_events) as transport loss
    /// rather than corruption — discontinuity-family signals under
    /// transport-loss excusal. Recorded so `events - attributed_events`
    /// still reconciles with the list a reader is shown.
    #[serde(default)]
    pub unexplained_transport_loss: u64,
    pub resyncs: u64,
    pub injected_fraction: f64,
    pub attribution_window: u64,
    pub recovery_bound: u64,
    /// The tap spec the sender's log header declares, carried through so
    /// a report can be checked against what the run was CONFIGURED to
    /// inject rather than only against what it happens to contain — see
    /// `report soak`'s `corruption_declared_<leg>` verdict.
    /// `#[serde(default)]` so an archived pre-realism report still
    /// deserializes (as a zero rate, which that verdict reads as "the
    /// report predates the declaration").
    #[serde(default)]
    pub rate_per_10k: u32,
    #[serde(default)]
    pub min_gap: u64,
    #[serde(default)]
    pub classes: Vec<Class>,
}

impl AttributionReport {
    /// Unexplained events that are corruption evidence — every
    /// unattributed event except the ones transport-loss excusal took.
    ///
    /// **This, not `unexplained_events.len()`, is what a verdict must
    /// gate on.** The list is a capped sample: a run that produced
    /// thousands of unexplained events shows sixty-four of them, and a
    /// check reading the list's length would read a badly-broken run as
    /// a mildly-broken one.
    #[must_use]
    pub fn unexplained_total(&self) -> u64 {
        Self::judged(
            self.unexplained_resyncs
                + self.unexplained_discontinuities
                + self.unexplained_nonconformant,
            &self.unexplained_events,
        )
    }

    /// Injections a conformant receiver had to notice and did not,
    /// uncapped — the count a verdict gates on, not
    /// `undetected.len()`.
    #[must_use]
    pub fn undetected_total(&self) -> u64 {
        Self::judged(self.undetected_count, &self.undetected)
    }

    /// Injections the stream never produced media after, uncapped.
    #[must_use]
    pub fn unrecovered_total(&self) -> u64 {
        Self::judged(self.unrecovered_count, &self.unrecovered)
    }

    /// Reconcile an uncapped counter with the sample list beside it.
    ///
    /// The counters are `#[serde(default)]`, so a report written BEFORE
    /// they existed deserializes with every one of them zero while its
    /// sample list still holds real findings. Gating on the counter alone
    /// would then read such a report as clean and pass a run that failed
    /// — verified against this arc's own 1-hour smoke artifacts, where
    /// re-judging them under the new counters flipped both
    /// `corruption_detected_*` failures to passes while the verdict
    /// detail still quoted the finding.
    ///
    /// A sample list can never be LONGER than the true count (it is a
    /// prefix of it, capped at [`MAX_SAMPLES`]), so the larger of the two
    /// is the honest answer for a report of either vintage: the counter
    /// for a current one, the list length for an archived one.
    fn judged(count: u64, sample: &[String]) -> u64 {
        count.max(sample.len() as u64)
    }
}

#[derive(Clone, Copy, Default)]
struct InjState {
    /// Receiver packet ordinal this injection landed at, once its PCR
    /// anchor has been seen.
    resolved_at: Option<u64>,
    detected: bool,
    recovered: bool,
    /// Resolved against a LATER base than the one logged (the logged one
    /// never arrived), so the position is approximate.
    approx: bool,
    /// Its anchor base fell more than `MAX_APPROX_TICKS` behind the first
    /// base the receiver saw afterwards, so it can never be placed. Kept
    /// distinct from "not resolved yet" so the scan cursors can move past
    /// it instead of stalling on it forever.
    stranded: bool,
    /// A `ContinuityJump` landed inside this injection's attribution
    /// window, on a PID this injection could itself have suffered a gap
    /// on ([`reaches`]). Evidence that packets went missing around here
    /// for a reason the corruption log cannot own: an impairment proxy's
    /// loss, an SRT/RIST buffer overrun, a reconnect after an outage. See
    /// [`Attribution::lossy`] for what it excuses.
    cc_jump_in_window: bool,
    /// Set when a NON-[`Signal::Resync`] signal detected this injection,
    /// i.e. when the receiver's own demuxer reported it rather than only
    /// the harness's raw reader. See
    /// [`AttributionReport::detected_by_reader_only`].
    detected_by_demux: bool,
}

/// Matches receiver events against a corruption log. Pure: no I/O, no
/// clock, and no knowledge of the receiver beyond the three `on_*` calls.
///
/// All three `on_*` methods expect monotonically non-decreasing `at`
/// values (receiver packet ordinals), which is how a receiver naturally
/// produces them. Internally that lets the engine keep a sliding cursor
/// over the injection list instead of rescanning it per event — a 72-hour
/// soak produces millions of events against a quarter-million injections,
/// and the quadratic form of that scan would not finish.
/// The half of a logged [`Injection`] the attribution engine reads.
///
/// Deliberately not the `Injection` itself. That carries `before`/`after`
/// byte images of every corrupted packet (up to ~500 bytes each), and a
/// 72-hour run logs a quarter of a million of them — state an engine that
/// only ever looks at the class, the PID and two flags has no business
/// holding onto. `Copy`, so retiring one costs nothing.
#[derive(Clone, Copy, Debug)]
struct Tracked {
    coord: Coord,
    class: Class,
    pid: u16,
    detectable: bool,
    psi: bool,
    /// Damage that breaks packet framing for the whole multiplex — a
    /// truncation, an inserted garbage run, or a header rewrite of the
    /// sync byte (offset 0). Such an injection can surface on ANY PID;
    /// every other class is confined to [`Tracked::pid`].
    framing_wide: bool,
}

impl From<&Injection> for Tracked {
    fn from(i: &Injection) -> Tracked {
        Tracked {
            coord: i.coord,
            class: i.class,
            pid: i.pid,
            detectable: i.detectable,
            psi: i.psi,
            framing_wide: matches!(i.class, Class::Truncate | Class::Garbage)
                || (i.class == Class::Header && i.offsets.first() == Some(&0)),
        }
    }
}

pub struct Attribution {
    /// Injections still in play, i.e. from `base` onwards. Everything
    /// before `base` has been judged into the counters below and dropped
    /// — see [`Attribution::prune`].
    inj: Vec<Tracked>,
    st: Vec<InjState>,
    /// Absolute index of `inj[0]`. Every cursor below is absolute, so
    /// they survive pruning unchanged.
    base: usize,
    window: u64,
    recovery_bound: u64,
    /// The sender's declared tap spec, carried from the log header into
    /// the report unchanged.
    rate_per_10k: u32,
    min_gap: u64,
    classes: Vec<Class>,
    /// First injection whose windows may still be open.
    lo: usize,
    /// One past the last injection resolved at or before the latest event.
    hi: usize,
    /// First injection not yet resolved; resolution runs front-to-back
    /// because logged coordinates are in stream order.
    next_unresolved: usize,
    /// Whether an unexplained discontinuity-family signal is charged to
    /// the corruption tap or written off as transport loss. Fixed at
    /// construction, not at `finish`: a multi-day run cannot keep every
    /// unexplained event around waiting to be classified, and the tier is
    /// a property of the capture (`VerifyMode`), known before the first
    /// byte arrives. See [`Attribution::lossy`].
    excuse_transport_loss: bool,
    events: u64,
    attributed_events: u64,
    attributed_nonconformant: u64,
    attributed_discontinuities: u64,
    resyncs: u64,
    /// Injections ever added, across `new` and every `append` — `inj.len()`
    /// no longer answers this once pruning starts.
    logged: u64,
    // Per-injection verdicts, accumulated as injections retire (and, at
    // `finish`, over whatever is still retained).
    detectable: u64,
    resolved: u64,
    unresolved: u64,
    undetected_count: u64,
    unrecovered_count: u64,
    undetected_lost: u64,
    unrecovered_lost: u64,
    detected_by_demux: u64,
    detected_by_reader_only: u64,
    detected_by_reader_only_per_class: BTreeMap<String, u64>,
    undetected_samples: Vec<String>,
    unrecovered_samples: Vec<String>,
    // Unexplained-event verdicts. The counts are per family and uncapped;
    // only `unexplained_samples` is bounded.
    unexplained_resyncs: u64,
    unexplained_discontinuities: u64,
    unexplained_nonconformant: u64,
    unexplained_transport_loss: u64,
    unexplained_samples: Vec<String>,
    first_unexplained_nc: Option<String>,
    first_unexplained_disc: Option<String>,
    /// Whether any PCR has been seen yet — decides whether an injection
    /// APPENDED mid-capture can still trust its ordinal-0 anchor. See
    /// [`Attribution::append`].
    seen_pcr: bool,
}

/// How far `a` is ahead of `b` on the 33-bit PCR-base circle. A PCR base
/// wraps every ~26.5 hours, so plain subtraction would mis-order every
/// coordinate straddling a wrap; by the standard half-space convention a
/// result below 2^32 means "ahead", and anything larger means `a` is
/// really behind `b`.
fn ticks_ahead(a: u64, b: u64) -> u64 {
    a.wrapping_sub(b) & ((1 << 33) - 1)
}

/// Signals a conformant receiver must produce for a given injection.
/// `true` for classes whose observable effect is not pinned to one signal
/// (any event inside the window then counts as having noticed).
fn expects(inj: &Tracked, sig: Signal) -> bool {
    match inj.class {
        // All three destroy packet framing: the reader resyncs, or the
        // packet vanishes from its PID and the CC jumps. An
        // adaptation-field-length overrun (one of the `Header` sub-kinds)
        // is instead reported as a plain non-conformance, so that counts
        // too — and so does a malformed PES, because all three can leave
        // a decoder reading a PES header off bytes that are not one: a
        // truncation misaligns the byte stream until a parser re-locks
        // (see `truncation_explains`), a garbage run does the same, and
        // an overrun adaptation-field length moves the payload offset
        // inside an otherwise intact packet. Measured: the `truncate`
        // positive control in `tests/corruption.rs` produces exactly one
        // `MalformedPes` on the video PID at the re-lock.
        Class::Header | Class::Truncate | Class::Garbage => matches!(
            sig,
            Signal::Resync
                | Signal::ContinuityJump
                | Signal::MalformedPes
                | Signal::OtherNonConformant
        ),
        Class::Drop => matches!(sig, Signal::ContinuityJump),
        // A broken section usually fails its CRC, but a flipped pointer
        // field or section-length can surface as a table-id / section
        // -length non-conformance before the CRC is ever reached.
        Class::PsiFlip => matches!(sig, Signal::PsiChecksum | Signal::OtherNonConformant),
        Class::BodyFlip if inj.psi => {
            matches!(sig, Signal::PsiChecksum | Signal::OtherNonConformant)
        }
        // Everything else — a non-PSI body flip, a duplicate — is never
        // `detectable`, so this arm only decides a flag nothing reads.
        _ => true,
    }
}

/// The PID half of causality: whether `inj` can reach a signal reported
/// on `pid`. A resync carries no PID (sync was lost for the whole
/// multiplex) and a PSI checksum failure is the PSI PID's own; a
/// framing-wide injection reaches everything; anything else has to
/// land on the PID it damaged.
fn reaches(inj: &Tracked, pid: Option<u16>, sig: Signal) -> bool {
    match pid {
        None => true,
        Some(p) => inj.framing_wide || sig == Signal::PsiChecksum || inj.pid == p,
    }
}

/// Whether `inj` can be the CAUSE of `sig` on `pid`: the class rule
/// ([`expects`]) and the PID rule ([`reaches`]) together. Position in
/// the window is the caller's third condition.
fn can_explain(inj: &Tracked, pid: Option<u16>, sig: Signal) -> bool {
    expects(inj, sig) && reaches(inj, pid, sig)
}

fn describe(inj: &Tracked, at: u64) -> String {
    format!(
        "{} on pid 0x{:04x} at packet {at}",
        inj.class.name(),
        inj.pid
    )
}

impl Attribution {
    /// Build from a parsed log, judging every finding strictly. Use for
    /// an offline capture (`verify`, and `recv --strict` on a transparent
    /// cell): there is no transport between a file and its verifier, so
    /// nothing is written off as transport loss.
    ///
    /// Injections logged before the stream's first PCR carry no base and
    /// resolve immediately, against receiver ordinal 0.
    #[must_use]
    pub fn strict(injections: Vec<Injection>, header: &LogHeader) -> Self {
        Self::build(injections, header, false)
    }

    /// Build from a parsed log, writing UNEXPLAINED DISCONTINUITIES off
    /// as transport loss rather than charging them to the corruption tap.
    ///
    /// Use for a live capture that crossed a real impaired link
    /// (`VerifyMode::Lossy`). That tier's whole contract is that packets
    /// go missing for reasons the sender never logged — an impairment
    /// proxy's loss, an SRT/RIST buffer overrun, the gap a reconnect
    /// leaves after an outage — and the engine has no way to tell such a
    /// gap from one the tap caused. With this:
    ///
    /// - Unexplained `ContinuityJump`/`OtherDiscontinuity` signals are
    ///   not corruption evidence at all. They are already counted in the
    ///   verifier's own `discontinuities`, which is exactly what the
    ///   lossy contract says to do with them, and they land in
    ///   [`AttributionReport::unexplained_transport_loss`] instead.
    ///   Unexplained `Resync`/`PsiChecksum`/`MalformedPes`/
    ///   `OtherNonConformant` still fail: a lost packet does not forge a
    ///   bad CRC or a malformed PES header.
    /// - An undetected or unrecovered injection with a FOREIGN
    ///   `ContinuityJump` in its window is excused into
    ///   [`AttributionReport::undetected_lost`] /
    ///   [`AttributionReport::unrecovered_lost`]: a corrupted packet the
    ///   network then threw away cannot be noticed by its damage, and
    ///   media the same gap swallowed is not the injection failing to
    ///   recover. Foreign is load-bearing — an injection's OWN jump never
    ///   excuses it (see [`Attribution::on_signal`]), or a `Drop`, whose
    ///   only observable is a continuity jump, would arrive pre-excused
    ///   and never have to recover at all.
    ///
    /// The excusal is applied as each event arrives, not at `finish`.
    /// Deciding late would mean holding every unexplained event for the
    /// length of the run, and a bounded hold would then let a run's
    /// excused events crowd out the one signal — an unexplained resync —
    /// that has no second detection path.
    #[must_use]
    pub fn lossy(injections: Vec<Injection>, header: &LogHeader) -> Self {
        Self::build(injections, header, true)
    }

    fn build(injections: Vec<Injection>, header: &LogHeader, excuse_transport_loss: bool) -> Self {
        let st = injections
            .iter()
            .map(|i| InjState {
                resolved_at: match i.coord.pcr_base {
                    None => Some(i.coord.since_pcr),
                    Some(_) => None,
                },
                ..InjState::default()
            })
            .collect();
        Attribution {
            logged: injections.len() as u64,
            inj: injections.iter().map(Tracked::from).collect(),
            st,
            base: 0,
            window: header.attribution_window,
            recovery_bound: header.recovery_bound,
            rate_per_10k: header.rate_per_10k,
            min_gap: header.min_gap,
            classes: header.classes.clone(),
            lo: 0,
            hi: 0,
            next_unresolved: 0,
            excuse_transport_loss,
            events: 0,
            attributed_events: 0,
            attributed_nonconformant: 0,
            attributed_discontinuities: 0,
            resyncs: 0,
            detectable: 0,
            resolved: 0,
            unresolved: 0,
            undetected_count: 0,
            unrecovered_count: 0,
            undetected_lost: 0,
            unrecovered_lost: 0,
            detected_by_demux: 0,
            detected_by_reader_only: 0,
            detected_by_reader_only_per_class: BTreeMap::new(),
            undetected_samples: Vec::new(),
            unrecovered_samples: Vec::new(),
            unexplained_resyncs: 0,
            unexplained_discontinuities: 0,
            unexplained_nonconformant: 0,
            unexplained_transport_loss: 0,
            unexplained_samples: Vec::new(),
            first_unexplained_nc: None,
            first_unexplained_disc: None,
            seen_pcr: false,
        }
    }

    /// Whether this attribution writes unexplained discontinuities off as
    /// transport loss — i.e. whether it was built by
    /// [`Attribution::lossy`]. Read by `verify::Tally::finish` to check
    /// that the tier the capture is being judged in is the tier the
    /// attribution was built for.
    #[must_use]
    pub fn excuses_transport_loss(&self) -> bool {
        self.excuse_transport_loss
    }

    /// Injections whose per-injection state is still held. Bounded by
    /// [`PRUNE_BATCH`] plus whatever the sender has logged ahead of the
    /// receiver's current position; exposed so a test can assert the
    /// bound rather than infer it.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.inj.len()
    }

    /// One past the last absolute injection index.
    fn end(&self) -> usize {
        self.base + self.inj.len()
    }

    fn tracked(&self, i: usize) -> &Tracked {
        &self.inj[i - self.base]
    }

    fn state(&self, i: usize) -> &InjState {
        &self.st[i - self.base]
    }

    fn state_mut(&mut self, i: usize) -> &mut InjState {
        &mut self.st[i - self.base]
    }

    /// Add injections the sender logged AFTER this attribution was built —
    /// what a live receiver's [`LogTail`] hands back as the run proceeds.
    ///
    /// They arrive in log order, which is stream order, so they extend the
    /// list past every cursor and the forward scan stays valid.
    ///
    /// One case cannot be carried over honestly. An injection whose
    /// coordinate has no PCR anchor (`pcr_base: None`) means "before the
    /// stream's first PCR", and [`Attribution::strict`] and [`Attribution::lossy`] resolve it against
    /// receiver ordinal 0 — sound only for a receiver that was listening
    /// from the stream's first packet. Once this attribution has seen a
    /// PCR, the receiver is demonstrably past that point, so an anchorless
    /// injection appended now belongs to a part of the stream it can no
    /// longer place: it is stranded instead, counted `unresolved` and
    /// never judged. Absent evidence is not evidence of a failure.
    pub fn append(&mut self, injections: Vec<Injection>) {
        for i in injections {
            let anchorless = i.coord.pcr_base.is_none();
            let st = InjState {
                resolved_at: match (anchorless, self.seen_pcr) {
                    (true, false) => Some(i.coord.since_pcr),
                    _ => None,
                },
                stranded: anchorless && self.seen_pcr,
                ..InjState::default()
            };
            self.inj.push(Tracked::from(&i));
            self.st.push(st);
            self.logged += 1;
        }
    }

    /// The receiver saw `pcr_base` on the packet at receiver ordinal `at`.
    ///
    /// Resolves every injection anchored at that base, plus any anchored
    /// at a base that never arrived — the corruption may have destroyed
    /// the packet carrying it — for which this is the first base at or
    /// after the anchor, and which is within [`MAX_APPROX_TICKS`] of it.
    /// Those resolve approximately and are given one PCR interval of extra
    /// attribution window. An anchor further behind than that is stranded:
    /// the receiver was not listening (a reconnect outage), so the
    /// injection stays unresolved and is never judged.
    pub fn on_pcr(&mut self, pcr_base: u64, at: u64) {
        self.seen_pcr = true;
        while self.next_unresolved < self.end() {
            let i = self.next_unresolved;
            let Some(b) = self.tracked(i).coord.pcr_base else {
                // Resolved at construction.
                self.next_unresolved += 1;
                continue;
            };
            let ahead = ticks_ahead(pcr_base, b);
            if b != pcr_base {
                if ahead >= 1 << 32 {
                    // Anchored at a base still in the future; so is every
                    // later injection, because logged coordinates are
                    // ordered.
                    break;
                }
                if ahead > MAX_APPROX_TICKS {
                    // Permanently unresolvable — every later base is
                    // further still — so advance past it rather than
                    // stalling the cursor on it.
                    self.state_mut(i).stranded = true;
                    self.next_unresolved += 1;
                    continue;
                }
            }
            // Saturating: a corrupt log must not panic the verifier.
            let since = self.tracked(i).coord.since_pcr;
            let st = self.state_mut(i);
            st.resolved_at = Some(at.saturating_add(since));
            st.approx = b != pcr_base;
            self.next_unresolved += 1;
        }
    }

    /// How far BEFORE its resolved position an injection's own evidence
    /// may surface.
    ///
    /// `resolved_at` is `ord(anchor PCR) + since_pcr`: a receiver ordinal
    /// plus a SENDER-side packet count. That sum is only exact if every
    /// packet between the anchor and the injection also reached the
    /// receiver. Under transport loss it over-shoots by however many of
    /// them went missing, so the injected packet's true receiver ordinal
    /// lies somewhere in `[r - since_pcr, r]` — the lower end being the
    /// degenerate case where the whole span was lost. A forward-only
    /// window (`r <= at`) therefore rejects an injection's OWN event
    /// whenever the backward displacement exceeds the demuxer's forward
    /// lateness, and the injection is reported undetected for damage the
    /// receiver noticed perfectly well.
    ///
    /// Measured on the arc's 1-hour smoke: 3 of 1738 detectable
    /// injections across two legs. Rare because the displacement usually
    /// loses to the lateness `ATTRIBUTION_WINDOW` was sized for, but the
    /// rate scales with run length and loss — order 100-200 over 72
    /// hours.
    ///
    /// CLAMPED, because windows must stay disjoint: two injections are
    /// `min_gap` apart, so extending one backwards by more than
    /// `min_gap - (window + APPROX_SLACK) - 1` would let it overlap its
    /// predecessor's, and `hit` would hand an event to the newer of the
    /// two — exactly the ambiguity [`CorruptConfig::validate`]'s `min_gap`
    /// floor exists to prevent. The clamp keeps the invariant true for
    /// ANY valid config rather than resting on a bound `since_pcr`
    /// happens to obey.
    ///
    /// The `- 1` is load-bearing. [`Attribution::window_contains`] is
    /// INCLUSIVE at both ends, so without it a predecessor's forward edge
    /// `rA + window + APPROX_SLACK` and a successor's lower edge
    /// `rB - reach` land on the same packet, and that one packet sits in
    /// both windows. `APPROX_SLACK` is in the budget unconditionally
    /// because the predecessor may have resolved approximately, which is
    /// what widens its forward edge.
    ///
    /// The bound is not "one PCR interval": `since_pcr` counts packets
    /// since the last packet that CARRIED a PCR, and the muxer's PCR
    /// cadence is not perfectly regular (`pcr_only_due` catch-up packets,
    /// and the PCR PID's own push rate). Measured on the same smoke, it
    /// reached 302 packets on a 40 ms-PCR profile and 393 on a 100 ms
    /// one — roughly 2.3 nominal intervals — against the ~130 a
    /// one-interval model predicts. At the default `min_gap` of 1000 the
    /// clamp is 371, which covers every injection in that run bar 4 of
    /// 3026.
    fn backward_reach(&self, i: usize) -> u64 {
        let budget = self
            .min_gap
            .saturating_sub(self.window.saturating_add(APPROX_SLACK))
            .saturating_sub(1);
        self.tracked(i).coord.since_pcr.min(budget)
    }

    /// First receiver ordinal injection `i`'s window covers, or `None`
    /// when it has no resolved position at all.
    fn window_lo(&self, i: usize) -> Option<u64> {
        let r = self.state(i).resolved_at?;
        Some(r.saturating_sub(self.backward_reach(i)))
    }

    /// Move the scan cursors up to event ordinal `at`, then retire what
    /// they have left behind.
    fn advance(&mut self, at: u64) {
        // A stranded injection has no position at all, so it must not park
        // the cursor: injections logged AFTER an outage resolve normally
        // and still need to be reachable.
        //
        // The test is the window's LOWER edge, not `resolved_at`: an
        // injection resolved past `at` can still cover `at` through
        // `backward_reach`, and a cursor keyed on `resolved_at <= at`
        // would leave it outside the scan range entirely.
        while self.hi < self.end()
            && (self.state(self.hi).stranded || self.window_lo(self.hi).is_some_and(|lo| lo <= at))
        {
            self.hi += 1;
        }
        let span = self.window.max(self.recovery_bound) + APPROX_SLACK;
        while self.lo < self.hi
            && (self.state(self.lo).stranded
                || self
                    .state(self.lo)
                    .resolved_at
                    .is_some_and(|r| r.saturating_add(span) < at))
        {
            self.lo += 1;
        }
        self.prune();
    }

    /// Fold injections both cursors have passed into the running
    /// counters and drop their per-injection state.
    ///
    /// Safe to do at all because the two cursors only ever move forward
    /// and nothing below them is ever read again: `lo` advances only past
    /// an injection that is STRANDED (a terminal state — it can never be
    /// resolved, so it can never be judged) or RESOLVED with every window
    /// closed, and `next_unresolved` is the only other index into the
    /// list. Retiring at the minimum of the two keeps both valid.
    ///
    /// Without this the engine holds every injection of the run: a
    /// 72-hour soak logs a quarter of a million of them, and even reduced
    /// to [`Tracked`] that is state growing with the capture, on the same
    /// process the run's own `rss_slope_*_recv` verdict gates at 200
    /// KiB/h.
    ///
    /// `next_unresolved` only moves when [`Attribution::on_pcr`] is fed,
    /// so a receiver that stopped seeing PCRs entirely would stop
    /// retiring. That is not a memory hazard worth guarding: a capture
    /// with no PCR resolves no coordinate at all, and its whole
    /// corruption verdict is already `unresolved`.
    fn prune(&mut self) {
        let keep_from = self.lo.min(self.next_unresolved);
        let n = keep_from - self.base;
        if n < PRUNE_BATCH {
            return;
        }
        for k in 0..n {
            let (inj, st) = (self.inj[k], self.st[k]);
            // A retired injection's recovery window is necessarily closed:
            // `lo` only passed it once `resolved_at + max(window,
            // recovery_bound) + APPROX_SLACK < at`, and `at` is a receiver
            // ordinal, so it is at most the capture's final packet count.
            self.judge(&inj, &st, true);
        }
        self.inj.drain(..n);
        self.st.drain(..n);
        self.base = keep_from;
    }

    /// Fold one injection's verdict into the running counters.
    /// `recovery_window_closed` says whether the capture ran far enough
    /// past it to hold it to a recovery obligation at all.
    fn judge(&mut self, inj: &Tracked, st: &InjState, recovery_window_closed: bool) {
        if inj.detectable {
            self.detectable += 1;
        }
        let Some(r) = st.resolved_at else {
            self.unresolved += 1;
            return;
        };
        self.resolved += 1;
        let lost = self.excuse_transport_loss && st.cc_jump_in_window;
        if inj.detectable && !st.detected {
            if lost {
                self.undetected_lost += 1;
            } else {
                self.undetected_count += 1;
                if self.undetected_samples.len() < MAX_SAMPLES {
                    self.undetected_samples.push(describe(inj, r));
                }
            }
        }
        // META-13: split the detection credit. A `Resync` is the
        // harness's raw reader noticing; everything else is the
        // receiver's own demuxer.
        if inj.detectable && st.detected {
            if st.detected_by_demux {
                self.detected_by_demux += 1;
            } else {
                self.detected_by_reader_only += 1;
                *self
                    .detected_by_reader_only_per_class
                    .entry(inj.class.name().to_string())
                    .or_insert(0) += 1;
            }
        }
        if !st.recovered && recovery_window_closed {
            if lost {
                self.unrecovered_lost += 1;
            } else {
                self.unrecovered_count += 1;
                if self.unrecovered_samples.len() < MAX_SAMPLES {
                    self.unrecovered_samples.push(describe(inj, r));
                }
            }
        }
    }

    /// Index of the resolved injection whose attribution window contains
    /// `at`, newest first — the closest preceding injection is the one
    /// that explains something happening at `at`, if any does.
    fn hit(&mut self, at: u64) -> Option<usize> {
        self.advance(at);
        (self.lo..self.hi)
            .rev()
            .find(|&i| self.window_contains(i, at))
    }

    /// Index of the newest resolved injection whose window contains
    /// `at` AND that can cause `sig` on `pid` — see [`can_explain`].
    /// [`Attribution::hit`] keeps the position-only answer for
    /// [`Attribution::truncation_explains`].
    fn explainer(&mut self, at: u64, pid: Option<u16>, sig: Signal) -> Option<usize> {
        self.advance(at);
        (self.lo..self.hi)
            .rev()
            .find(|&i| self.window_contains(i, at) && can_explain(self.tracked(i), pid, sig))
    }

    /// Whether injection `i` is resolved and its attribution window
    /// contains `at`. Takes `&self` — unlike [`Attribution::hit`] it never
    /// advances the cursors, so it is usable from read-only queries that
    /// run after some `on_*` call has already positioned them.
    fn window_contains(&self, i: usize, at: u64) -> bool {
        let Some(r) = self.state(i).resolved_at else {
            return false;
        };
        let w = if self.state(i).approx {
            self.window + APPROX_SLACK
        } else {
            self.window
        };
        // `[r - backward_reach, r + w]`. The backward half absorbs the
        // transport loss `resolved_at` cannot see (see `backward_reach`);
        // the forward half is the demuxer lateness the window was always
        // sized for.
        let lo = r.saturating_sub(self.backward_reach(i));
        lo <= at && at <= r.saturating_add(w)
    }

    /// Whether an injection plausibly DAMAGED the bytes a receiver-side
    /// decoder is about to reject at receiver ordinal `at` on `pid`.
    /// Records nothing: it is a question a content oracle asks before
    /// judging a record, not a receiver event.
    ///
    /// True when `at` falls in the window of a resolved injection that
    /// either targeted `pid` (any class — a body flip, a header rewrite or
    /// a dropped packet all leave the record on that PID malformed or
    /// incomplete) or was a [`Class::Truncate`]/[`Class::Garbage`] on any
    /// PID (both destroy packet framing for the whole multiplex, so the
    /// damage is not confined to the PID they hit).
    ///
    /// The point is not to excuse a decoder bug. A record the tap never
    /// touched still has to decode, and a record inside a window still has
    /// to be DELIVERED — this only stops a content oracle from reporting
    /// deliberate damage as a producer defect, which would make every run
    /// with the tap enabled fail for doing its job.
    ///
    /// Takes `&mut self` for the same reason
    /// [`Attribution::truncation_explains`] does: it advances the sliding
    /// cursors to `at` itself rather than trusting that some other `on_*`
    /// call already did. Depending on the caller's ordering would make the
    /// answer silently wrong (an unpositioned cursor range is empty, so
    /// every query would answer "no") the moment anyone reordered the
    /// receiver's event handling. Like the `on_*` methods it expects
    /// non-decreasing `at`.
    #[must_use]
    pub fn explains_damage(&mut self, at: u64, pid: u16) -> bool {
        self.advance(at);
        (self.lo..self.hi).rev().any(|i| {
            self.window_contains(i, at)
                && (self.tracked(i).pid == pid
                    || matches!(self.tracked(i).class, Class::Truncate | Class::Garbage))
        })
    }

    /// Whether a TRUNCATION explains an anomaly observed at receiver
    /// ordinal `at`, WITHOUT recording it as an event.
    ///
    /// One class, for one measured reason. [`Class::Truncate`] drops a
    /// non-multiple of 188 bytes, so from that point the byte stream is
    /// MISALIGNED and stays misaligned until a parser hunts a new
    /// 188-stride lock. During that hunt it can lock onto a 0x47 that is
    /// live media payload and read the bytes after it as a PES header —
    /// yielding a "PTS" that was never a timestamp. That is the
    /// truncation doing exactly what it is for, not a timing defect, and
    /// a receiver that re-locks and carries on is behaving correctly.
    ///
    /// No other class can do it. [`Class::Garbage`] inserts bytes but
    /// never emits 0x47, precisely so it cannot fake a packet start;
    /// every other class preserves packet length and therefore alignment.
    /// Measured over 6 profiles × 7 classes × 40 seeds: 42 PTS
    /// monotonicity breaks, all 42 from `truncate`, none from anything
    /// else. So the exception stays pinned to the one class rather than
    /// excusing derived anomalies in general — a PTS defect inside a body
    /// flip's, drop's, dup's, header's or psi_flip's window still fails
    /// the run, and after the `BodyFlip` draw range was moved past the PES
    /// header no flip can manufacture one anyway.
    ///
    /// Deliberately NOT routed through [`Attribution::on_signal`]: that
    /// would inflate `attributed_events` with something the receiver never
    /// reported, and a verifier subtracting those counts from its own
    /// event tallies would then excuse one real event per anomaly.
    ///
    /// Like the `on_*` methods this expects non-decreasing `at`.
    pub fn truncation_explains(&mut self, at: u64) -> bool {
        self.hit(at)
            .is_some_and(|i| self.tracked(i).class == Class::Truncate)
    }

    /// An error-class event surfaced at receiver ordinal `at` (`pid` is
    /// `None` for a resync, which is not attributable to a PID).
    ///
    /// The event is EXPLAINED by the newest injection whose window
    /// contains `at` and that can actually cause `sig` on `pid` — the
    /// class rule (`expects`) and the PID rule (`reaches`) together,
    /// see `can_explain`. An event of a class an injection cannot
    /// cause, or on a PID it never touched, is therefore UNEXPLAINED
    /// even when it lands squarely inside that injection's window:
    /// window position is proximity, not causation.
    pub fn on_signal(&mut self, at: u64, pid: Option<u16>, sig: Signal) {
        self.events += 1;
        if sig == Signal::Resync {
            self.resyncs += 1;
        }
        let attributed_to = self.explainer(at, pid, sig);
        // A continuity jump is the one signal that also means "packets
        // went missing here", so it is recorded against every injection
        // whose window contains it — EXCEPT the injection it is attributed
        // to.
        //
        // That exception is the whole point. An injection's own jump is
        // its own damage surfacing; it cannot also be evidence that the
        // network swallowed that same injection. Without the exception a
        // `Drop` would excuse itself: its only observable IS a continuity
        // jump, so every drop would arrive pre-excused and its recovery
        // obligation would evaporate. A jump from an OUTAGE or from
        // another injection still excuses, which is what this flag is for.
        //
        // `hit` picks the newest covering injection, so the loop can still
        // mark an older one whose window also covers `at`. Under a valid
        // config windows cannot overlap (`min_gap >= 2 *
        // ATTRIBUTION_WINDOW`) and only an `approx` resolution's widened
        // window reaches this at all, but marking solely the newest would
        // silently under-record when they do overlap.
        if sig == Signal::ContinuityJump {
            for i in self.lo..self.hi {
                // Same causal restriction as the explanation above: a
                // gap on a PID this injection never touched is not
                // evidence its own packet went missing.
                if !self.window_contains(i, at) || !reaches(self.tracked(i), pid, sig) {
                    continue;
                }
                // An injection's own jump is its own damage surfacing —
                // but ONLY for a class whose expected signals include a
                // continuity jump. There the jump IS the detection, and
                // letting it also excuse would pre-excuse every `Drop`,
                // whose only observable is exactly that jump, so no drop
                // would ever have to show the stream recovering.
                //
                // For a class whose expected signals EXCLUDE it — a
                // `PsiFlip`, or a `BodyFlip` on a PSI packet, both of
                // which have to answer with a bad CRC or a
                // non-conformance — an attributed continuity jump means
                // something else entirely: the packet carrying the
                // injection went missing in transit, so the damage never
                // reached the receiver to be noticed. That is the
                // transport-loss evidence `undetected_lost` exists for.
                //
                // Measured on the arc's 1-hour smoke: a PSI body flip on
                // the srt leg whose corrupted PMT packet the proxy
                // dropped. The receiver emitted a `ContinuityJump` on
                // PID 0x1000, `on_signal` attributed it to that very
                // injection, `expects` rejected it, and the old blanket
                // exclusion then kept it out of `undetected_lost` too —
                // so the run failed for damage nobody could have seen.
                //
                // `explainer` only ever hands back an injection whose
                // expected signals include this one, so being the
                // explainer IS the class check.
                if Some(i) == attributed_to {
                    continue;
                }
                self.state_mut(i).cc_jump_in_window = true;
            }
        }
        match attributed_to {
            Some(i) => {
                self.attributed_events += 1;
                match sig {
                    Signal::PsiChecksum | Signal::MalformedPes | Signal::OtherNonConformant => {
                        self.attributed_nonconformant += 1;
                    }
                    Signal::ContinuityJump | Signal::OtherDiscontinuity => {
                        self.attributed_discontinuities += 1;
                    }
                    Signal::Resync => {}
                }
                self.state_mut(i).detected = true;
                // META-13: a resync is the harness's own raw reader
                // losing and regaining packet sync, not a `DemuxEvent`.
                // Only the other signals are the receiver reporting.
                if sig != Signal::Resync {
                    self.state_mut(i).detected_by_demux = true;
                }
            }
            // Unexplained. Which family it belongs to — and therefore
            // whether transport-loss excusal takes it — is decided HERE,
            // not at `finish`.
            //
            // Deciding late was a hole with teeth. The sample list is
            // capped, and a lossy multi-day run produces excused
            // continuity jumps by the thousand: they would fill the cap
            // within the first minutes, after which a genuinely
            // unexplained RESYNC — the one signal with no second
            // detection path, since it is not a `DemuxEvent` and resync
            // mode suppresses `rawts_sync_loss` — would be dropped on the
            // floor and the run would pass. Excused events now consume no
            // cap budget, and each family is counted without a cap.
            None => {
                let text = format!("{sig:?} on pid {pid:?} at packet {at}");
                match sig {
                    Signal::PsiChecksum | Signal::MalformedPes | Signal::OtherNonConformant => {
                        self.first_unexplained_nc
                            .get_or_insert_with(|| text.clone());
                        self.unexplained_nonconformant += 1;
                    }
                    Signal::ContinuityJump | Signal::OtherDiscontinuity => {
                        self.first_unexplained_disc
                            .get_or_insert_with(|| text.clone());
                        if self.excuse_transport_loss {
                            self.unexplained_transport_loss += 1;
                            return;
                        }
                        self.unexplained_discontinuities += 1;
                    }
                    Signal::Resync => self.unexplained_resyncs += 1,
                }
                if self.unexplained_samples.len() < MAX_SAMPLES {
                    self.unexplained_samples.push(text);
                }
            }
        }
    }

    /// A Sample/Metadata event on `pid` surfaced at receiver ordinal `at` —
    /// evidence the stream recovered from whatever preceded it.
    pub fn on_media(&mut self, at: u64, pid: u16) {
        self.advance(at);
        for i in self.lo..self.hi {
            if self.state(i).recovered {
                continue;
            }
            let Some(r) = self.state(i).resolved_at else {
                continue;
            };
            if at < r || at > r.saturating_add(self.recovery_bound) {
                continue;
            }
            // Truncation and inserted garbage break packet sync for the
            // whole multiplex, so media on ANY PID proves recovery; the
            // other classes damage one PID and must be answered on it.
            //
            // A PAT/PMT PID is the exception to "answered on it": it
            // carries no media at all. `Sample`/`Metadata` events only
            // ever name an elementary stream, so an injection on a PSI PID
            // would wait forever for media on PID 0 and be reported
            // unrecovered no matter how healthy the stream was. What
            // recovery means there is that the multiplex kept delivering —
            // which is exactly media on any PID.
            let any_pid = self.tracked(i).psi
                || matches!(self.tracked(i).class, Class::Garbage | Class::Truncate);
            if any_pid || self.tracked(i).pid == pid {
                self.state_mut(i).recovered = true;
            }
        }
    }

    /// Final verdict over a capture of `packets_total` receiver packets.
    ///
    /// An injection whose recovery window runs past the end of the capture
    /// is not judged for recovery, and an injection that never resolved is
    /// not judged at all — absent evidence is not evidence of a failure.
    ///
    /// Whether findings are judged strictly or with transport-loss
    /// excusal was fixed when this attribution was built
    /// ([`Attribution::strict`] / [`Attribution::lossy`]).
    #[must_use]
    pub fn finish(mut self, packets_total: u64) -> AttributionReport {
        // Everything already retired has been judged (`Attribution::judge`
        // via `prune`); this folds in whatever is still retained.
        for k in 0..self.inj.len() {
            let (inj, st) = (self.inj[k], self.st[k]);
            let closed = st
                .resolved_at
                .is_some_and(|r| r.saturating_add(self.recovery_bound) <= packets_total);
            self.judge(&inj, &st, closed);
        }
        AttributionReport {
            injected: self.logged,
            detectable: self.detectable,
            resolved: self.resolved,
            unresolved: self.unresolved,
            events: self.events,
            attributed_events: self.attributed_events,
            attributed_nonconformant: self.attributed_nonconformant,
            attributed_discontinuities: self.attributed_discontinuities,
            unexplained_events: self.unexplained_samples,
            unexplained_resyncs: self.unexplained_resyncs,
            unexplained_discontinuities: self.unexplained_discontinuities,
            unexplained_nonconformant: self.unexplained_nonconformant,
            first_unexplained_nonconformant: self.first_unexplained_nc,
            first_unexplained_discontinuity: self.first_unexplained_disc,
            undetected: self.undetected_samples,
            undetected_count: self.undetected_count,
            unrecovered: self.unrecovered_samples,
            unrecovered_count: self.unrecovered_count,
            undetected_lost: self.undetected_lost,
            unrecovered_lost: self.unrecovered_lost,
            detected_by_demux: self.detected_by_demux,
            detected_by_reader_only: self.detected_by_reader_only,
            detected_by_reader_only_per_class: self.detected_by_reader_only_per_class,
            unexplained_transport_loss: self.unexplained_transport_loss,
            resyncs: self.resyncs,
            injected_fraction: self.logged as f64 / packets_total.max(1) as f64,
            attribution_window: self.window,
            recovery_bound: self.recovery_bound,
            rate_per_10k: self.rate_per_10k,
            min_gap: self.min_gap,
            classes: self.classes,
        }
    }
}

// ============================================================
// Test support (shared with the crate's integration tests)
// ============================================================

/// In-memory sinks the tap's tests drive it over. Public so the crate's
/// integration-test binaries can use the same ones the unit tests do;
/// hidden from the docs because nothing outside the test tree should.
#[doc(hidden)]
pub mod testing {
    use super::{Arc, Mutex, Transport, TransportError, Write};

    /// Collects every byte the tap sends, flattened.
    pub struct VecTransport(pub Arc<Mutex<Vec<u8>>>);

    impl Transport for VecTransport {
        fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
            self.0
                .lock()
                .expect("VecTransport mutex")
                .extend_from_slice(msg);
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    /// Collects the tap's JSONL log.
    pub struct VecWriter(pub Arc<Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("VecWriter mutex").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory transport that records every `send_bytes` payload
    /// separately — the per-push view the splitting test needs.
    struct Capture(Arc<Mutex<Vec<Vec<u8>>>>);
    impl Transport for Capture {
        fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
            self.0.lock().unwrap().push(msg.to_vec());
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    fn baseline_bytes(seconds: f64, tag: &str) -> Vec<u8> {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-{tag}-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            seconds,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        b
    }

    /// Push `bytes` through a `Corrupter` in 1316-byte pushes; return (wire bytes, log text, stats).
    fn run_tap(bytes: &[u8], cfg: CorruptConfig) -> (Vec<u8>, String, CorruptionStats) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg,
            Box::new(testing::VecWriter(Arc::clone(&log))),
        )
        .unwrap();
        for chunk in bytes.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        drop(tap);
        let wire: Vec<u8> = sink.lock().unwrap().iter().flatten().copied().collect();
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        (wire, text, stats)
    }

    fn cfg(classes: &[Class], rate: u32, min_gap: u64) -> CorruptConfig {
        CorruptConfig {
            rate_per_10k: rate,
            min_gap,
            classes: classes.to_vec(),
            seed: 7,
        }
    }

    /// On a PAT/PMT packet the `Header` class must use ONLY the sync-byte
    /// sub-kind, because the other three are invisible to a receiver
    /// there (see the `Class::Header` arm's own comment) and an
    /// invisible-but-`detectable` injection makes the report fail a
    /// conformant library.
    ///
    /// Driven over a stream of nothing but PAT packets rather than a
    /// normal capture: PSI is ~1% of a real multiplex, so with `min_gap`
    /// at its 1000-packet floor a normal stream would have to run for
    /// minutes before an injection happened to land on one, and the test
    /// would pass vacuously long before that.
    #[test]
    fn header_class_on_a_psi_packet_only_ever_kills_the_sync_byte() {
        let b = baseline_bytes(2.0, "psihdr");
        let pat: [u8; PKT] = b[..PKT].try_into().unwrap();
        assert_eq!(
            crate::rawts::classify_packet(&pat).unwrap().pid,
            0,
            "the generator emits the PAT first"
        );
        let mut stream = Vec::with_capacity(PKT * 5000);
        for _ in 0..5000 {
            stream.extend_from_slice(&pat);
        }

        let (_, text, _) = run_tap(&stream, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj.len() >= 4, "{} injection(s)", inj.len());
        for i in &inj {
            assert_eq!(i.pid, 0);
            assert_eq!(
                i.offsets,
                vec![0],
                "sub-kind {:?} on a PSI packet",
                i.offsets
            );
            assert_ne!(i.after[0], 0x47, "the sync byte must actually be gone");
        }
    }

    /// The flip side: on MEDIA PIDs all four sub-kinds stay in play, so
    /// the PSI rule above cannot have quietly collapsed the class into
    /// "always kill the sync byte" everywhere.
    #[test]
    fn header_class_on_media_packets_still_uses_every_sub_kind() {
        let b = baseline_bytes(600.0, "mediahdr");
        let (_, text, _) = run_tap(&b, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let media: Vec<&Injection> = inj
            .iter()
            .filter(|i| !matches!(i.pid, 0 | 0x1000))
            .collect();
        assert!(media.len() >= 8, "{} media injection(s)", media.len());
        let shapes: std::collections::BTreeSet<Vec<usize>> =
            media.iter().map(|i| i.offsets.clone()).collect();
        assert!(
            shapes.len() >= 2,
            "every media injection took the same sub-kind: {shapes:?}"
        );
    }

    #[test]
    fn parse_corrupt_accepts_the_documented_grammar_and_rejects_bad_gaps() {
        let c = parse_corrupt("rate=5,min_gap=1000", 3).unwrap();
        assert_eq!(
            c,
            CorruptConfig {
                rate_per_10k: 5,
                min_gap: 1000,
                classes: Class::ALL.to_vec(),
                seed: 3
            }
        );
        let c = parse_corrupt("rate=50,min_gap=1200,classes=header+psi_flip", 3).unwrap();
        assert_eq!(c.classes, vec![Class::Header, Class::PsiFlip]);
        assert!(
            parse_corrupt("rate=5,min_gap=999", 3).is_err(),
            "min_gap < 2*window"
        );
        assert!(parse_corrupt("rate=5,min_gap=1000,classes=bogus", 3).is_err());
        assert!(
            parse_corrupt("rate=0,min_gap=1000", 3).is_err(),
            "rate 0 is 'disabled', reject"
        );
        assert!(parse_corrupt("min_gap=1000", 3).is_err(), "rate required");
        assert!(
            parse_corrupt("rate=5,min_gap=1000,bogus=1", 3).is_err(),
            "unknown keys fail closed"
        );
    }

    #[test]
    fn same_seed_same_input_gives_byte_identical_wire_and_log() {
        let b = baseline_bytes(6.0, "det");
        let (w1, l1, _) = run_tap(&b, cfg(&Class::ALL, 200, 1000));
        let (w2, l2, _) = run_tap(&b, cfg(&Class::ALL, 200, 1000));
        assert_eq!(w1, w2);
        assert_eq!(l1, l2);
        let (w3, _, _) = run_tap(
            &b,
            CorruptConfig {
                seed: 8,
                ..cfg(&Class::ALL, 200, 1000)
            },
        );
        assert_ne!(w1, w3);
    }

    #[test]
    fn log_header_then_one_line_per_injection_and_min_gap_is_honored() {
        // 60 s of baseline is 3600 packets (the profile muxes 60 packets
        // per second — see `rawts`'s own 180-packets-in-3 s assertion), so
        // a 5% rate with a 1000-packet floor lands ~3 injections.
        let b = baseline_bytes(60.0, "gap");
        let (_, text, stats) = run_tap(&b, cfg(&Class::ALL, 500, 1000));
        let (hdr, inj) = parse_log(&text).unwrap();
        assert_eq!(hdr.min_gap, 1000);
        assert_eq!(inj.len() as u64, stats.injections);
        assert!(
            stats.injections >= 2,
            "3600 packets at 5% -> expect >= 2 with gap 1000: {stats:?}"
        );
        for w in inj.windows(2) {
            assert!(w[1].ordinal - w[0].ordinal >= 1000, "{:?}", w);
        }
    }

    /// Each class, forced (rate 10_000 = every eligible packet, min_gap
    /// 1000 so exactly one injection lands in the first ~1000 packets),
    /// produces the documented wire mutation.
    #[test]
    fn body_flip_changes_only_bytes_past_the_header() {
        let b = baseline_bytes(6.0, "bf");
        let (w, text, _) = run_tap(&b, cfg(&[Class::BodyFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let i = &inj[0];
        assert_eq!(i.class, Class::BodyFlip);
        assert!(i.offsets.iter().all(|&o| (4..188).contains(&o)));
        let orig = &b[i.ordinal as usize * 188..][..188];
        let got = &w[i.ordinal as usize * 188..][..188];
        assert_eq!(&orig[..4], &got[..4]);
        assert_ne!(orig, got);
        assert_eq!(w.len(), b.len());
    }

    /// A body flip must never land inside a PES header.
    ///
    /// The bytes there are undetectable by contract (tst-core's PES
    /// parser accepts `stream_id`, `PES_packet_length`,
    /// `header_data_length` and 33 of the 40 PTS bits as-is) but they are
    /// read STRUCTURALLY by the wire oracles, so a flip that silently
    /// rewrites a PTS or a stream_id fails the capture for damage the tap
    /// itself declared nobody has to notice. Measured before this rule:
    /// `pts_wrap_unexpected` on 5 seeds and `av1_carriage_wire` on 3,
    /// across a 6-profile × 7-class × 40-seed sweep.
    ///
    /// Swept over 64 seeds rather than one because the offset is a PRNG
    /// draw; the assertion that PES-start injections actually occurred is
    /// what stops the sweep passing vacuously.
    #[test]
    fn a_body_flip_never_lands_in_a_pes_header() {
        let b = baseline_bytes(300.0, "bfpes");
        let mut pes_starts = 0;
        for seed in 1..=64u64 {
            let (_, text, _) = run_tap(
                &b,
                CorruptConfig {
                    seed,
                    ..cfg(&[Class::BodyFlip], 10_000, 1000)
                },
            );
            let (_, inj) = parse_log(&text).unwrap();
            for i in inj.iter().filter(|i| i.pes_start) {
                pes_starts += 1;
                let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
                let info = crate::rawts::classify_packet(&pkt).unwrap();
                // §2.4.3.7: the ES payload begins after the 9 fixed PES
                // header bytes plus `header_data_length` optional ones.
                let es = info.payload_off + 9 + usize::from(pkt[info.payload_off + 8]);
                for &o in &i.offsets {
                    assert!(
                        o >= es,
                        "seed {seed}: offset {o} is inside the PES header \
                         (payload_off {}, ES payload starts at {es})",
                        info.payload_off
                    );
                }
            }
        }
        assert!(
            pes_starts >= 8,
            "only {pes_starts} PES-start injection(s) across 64 seeds — the rule above \
             is barely exercised"
        );
    }

    /// A body flip is only claimed detectable when it lands under a
    /// section CRC. This pins the one span that decides that.
    #[test]
    fn sensitive_span_covers_the_psi_section_and_nothing_else() {
        let b = baseline_bytes(2.0, "span");
        let pkt_at = |n: usize| -> [u8; 188] { b[n * 188..][..188].try_into().unwrap() };

        // The PAT: section body through the end of the CRC. Everything
        // after it is 0xFF stuffing, where a flip is invisible — and
        // everything BEFORE it (pointer field, table_id, section_length)
        // is read to find the section, so a flip there makes the receiver
        // discard it before the CRC is ever checked. See
        // `sensitive_span`'s own doc comment for the measured evidence.
        let pat = pkt_at(0);
        let info = crate::rawts::classify_packet(&pat).unwrap();
        assert_eq!(info.pid, 0, "first packet of a fresh mux is the PAT");
        let (lo, hi) = sensitive_span(&pat, &info, true).unwrap();
        // pointer field at payload_off (0 here), then the 3-byte section
        // header, then the body.
        assert_eq!(
            lo,
            info.payload_off + 1 + usize::from(pat[info.payload_off]) + 3
        );
        assert!(hi < 188, "a PAT section is far shorter than its payload");
        assert!(
            pat[hi..].iter().all(|&x| x == 0xFF),
            "stuffing past the CRC"
        );
        // The three header bytes and the pointer field sit OUTSIDE the
        // span: a flip there is not required to be noticed.
        assert!(
            (info.payload_off..lo).len() == 4,
            "pointer field + 3-byte section header are excluded"
        );

        // No video packet has a required-to-notice span, PES start or
        // not: tst-core's PES parser tolerates most of the header, and
        // nothing at all checks the access-unit bytes after it.
        let (mut starts, mut video) = (0, 0);
        for n in 0..b.len() / 188 {
            let pkt = pkt_at(n);
            let info = crate::rawts::classify_packet(&pkt).unwrap();
            if info.pid != 0x1011 {
                continue;
            }
            video += 1;
            starts += usize::from(pes_start(&pkt, &info));
            assert_eq!(sensitive_span(&pkt, &info, false), None, "packet {n}");
        }
        assert!(
            starts > 0,
            "{starts} PES starts among {video} video packets"
        );

        // A PSI packet still needs PUSI: a section continuation carries no
        // section header to locate.
        let mut pat = pkt_at(0);
        pat[1] &= !0x40;
        let info = crate::rawts::classify_packet(&pat).unwrap();
        assert_eq!(sensitive_span(&pat, &info, true), None);
    }

    #[test]
    fn header_class_is_always_detectable_and_keeps_packet_length() {
        let b = baseline_bytes(6.0, "hdr");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj[0].detectable);
        assert!(inj[0].offsets.iter().all(|&o| o <= 4));
        assert_eq!(w.len(), b.len());
    }

    #[test]
    fn truncate_shortens_the_wire_by_the_cut() {
        let b = baseline_bytes(6.0, "tr");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Truncate], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let cut = 188 - inj[0].after.len();
        assert!((1..=148).contains(&cut), "emit 40..=187 bytes");
        assert_eq!(
            w.len(),
            b.len() - inj.iter().map(|i| 188 - i.after.len()).sum::<usize>()
        );
    }

    #[test]
    fn garbage_inserts_1_to_300_non_sync_bytes_and_splits_oversize_pushes() {
        let b = baseline_bytes(6.0, "gb");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Garbage], 10_000, 1000),
            Box::new(std::io::sink()),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        let pushes = sink.lock().unwrap().clone();
        assert!(
            pushes.iter().all(|p| p.len() <= 1316),
            "every push <= max_payload"
        );
        assert!(stats.bytes_out > stats.bytes_in);
        assert!(stats.bytes_out - stats.bytes_in <= 300 * stats.injections);
        let wire: Vec<u8> = pushes.iter().flatten().copied().collect();
        // No inserted byte is 0x47 (garbage never fakes a sync).
        let mut r = crate::rawts::Reader::new();
        r.set_resync_mode(true);
        r.feed(&wire).unwrap();
        assert_eq!(r.resyncs().len() as u64, stats.injections);
    }

    #[test]
    fn drop_removes_and_dup_repeats_a_packet() {
        let b = baseline_bytes(6.0, "dd");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Drop], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert_eq!(w.len(), b.len() - 188 * inj.len());
        // The forced first injection lands on packet 0, which is the PAT —
        // where a drop is invisible. Detectability is
        // `drop_is_detectable_only_on_a_payload_carrying_media_packet`'s
        // subject; this test is about the wire arithmetic.
        assert_eq!(inj[0].pid, 0);
        assert!(!inj[0].detectable);
        let (w, text, _) = run_tap(&b, cfg(&[Class::Dup], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert_eq!(w.len(), b.len() + 188 * inj.len());
        assert!(!inj[0].detectable);
        let o = inj[0].ordinal as usize;
        assert_eq!(&w[o * 188..(o + 1) * 188], &w[(o + 1) * 188..(o + 2) * 188]);
    }

    /// A dropped packet is only ever noticed as a continuity jump, so it
    /// is only `detectable` where a continuity jump can exist: on a media
    /// PID, on a packet that carries payload. Claiming otherwise fails a
    /// conformant receiver for staying silent about a lost PAT repetition
    /// or a lost PCR-only catch-up packet, neither of which a receiver is
    /// required — or in tst-core's case even able — to report.
    ///
    /// Driven over the `audio` profile because its third stream is what
    /// makes the muxer emit adaptation-field-only catch-up packets on a
    /// media PID (`mux/scheduling.rs`'s `pcr_only_due`); the assertion
    /// below that all three shapes occurred is what keeps this from
    /// passing vacuously.
    #[test]
    fn drop_is_detectable_only_on_a_payload_carrying_media_packet() {
        let p = crate::profiles::by_name("audio").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-dropdet-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            300.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let (_, text, _) = run_tap(&b, cfg(&[Class::Drop], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let (mut psi, mut af_only, mut with_payload) = (0, 0, 0);
        for i in &inj {
            let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
            let info = crate::rawts::classify_packet(&pkt).unwrap();
            let is_psi = matches!(i.pid, 0 | 0x1000);
            assert_eq!(
                i.detectable,
                !is_psi && info.has_payload,
                "pid {:#06x} has_payload {} -> detectable {}",
                i.pid,
                info.has_payload,
                i.detectable
            );
            match (is_psi, info.has_payload) {
                (true, _) => psi += 1,
                (false, false) => af_only += 1,
                (false, true) => with_payload += 1,
            }
        }
        assert!(
            psi > 0 && af_only > 0 && with_payload > 0,
            "all three shapes must occur or the assertion above is vacuous: \
             psi {psi}, af-only {af_only}, with-payload {with_payload}"
        );
    }

    /// A PAT/PMT PID carries no media, so "media on the injected PID"
    /// can never arrive there — `Sample`/`Metadata` events only ever name
    /// an elementary stream. Recovery from a PSI injection is therefore
    /// proved by media on ANY PID, or every psi_flip in a soak run would
    /// be reported unrecovered against a perfectly healthy stream.
    #[test]
    fn a_psi_injection_recovers_on_media_from_any_pid() {
        let mut psi_inj = inj(1000, 0, Class::PsiFlip, 0, true);
        psi_inj.psi = true;
        let mut a = Attribution::strict(vec![psi_inj], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::PsiChecksum);
        a.on_media(5020, 0x1011); // an elementary stream, not pid 0
        let r = a.finish(10_000);
        assert!(r.unrecovered.is_empty(), "{:?}", r.unrecovered);

        // A MEDIA-PID injection still has to be answered on its own PID:
        // the relaxation is scoped to PSI, not a blanket "any event
        // anywhere counts".
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(r.unrecovered.len(), 1, "media on a different PID");
    }

    /// A derived anomaly (a PTS that stepped backwards) is asked about,
    /// not fed in — and only a TRUNCATION answers for it. Asking must not
    /// move any of the counters an event would.
    #[test]
    fn only_a_truncation_explains_a_derived_anomaly() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Truncate, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
        // The window opens at `r - since_pcr` (see `backward_reach`), not
        // at `r`: with `since_pcr = 10` the injected packet's true
        // receiver ordinal is somewhere in 5000..=5010, depending on how
        // much of that span the transport lost.
        assert!(!a.truncation_explains(4999), "before the window opens");
        assert!(a.truncation_explains(5000), "earliest the injection can be");
        assert!(a.truncation_explains(5010), "at the injection");
        assert!(
            a.truncation_explains(5010 + ATTRIBUTION_WINDOW),
            "last packet in the window"
        );
        assert!(
            !a.truncation_explains(5011 + ATTRIBUTION_WINDOW),
            "past the window"
        );
        let r = a.finish(10_000);
        assert_eq!(
            (r.events, r.attributed_events, r.attributed_discontinuities),
            (0, 0, 0),
            "asking is not an event"
        );

        // Every other class leaves the anomaly unexplained: truncation is
        // the only one that misaligns the byte stream, so it is the only
        // one whose window may contain a PTS that was never a timestamp.
        for class in Class::ALL.iter().filter(|&&c| c != Class::Truncate) {
            let mut a = Attribution::strict(vec![inj(1000, 10, *class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            assert!(
                !a.truncation_explains(5010),
                "{class:?} must not excuse a derived anomaly"
            );
        }
    }

    #[test]
    fn psi_flip_defers_until_a_pat_or_pmt_packet_and_never_touches_the_crc() {
        let b = baseline_bytes(6.0, "psi");
        let (w, text, _) = run_tap(&b, cfg(&[Class::PsiFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(!inj.is_empty());
        for i in &inj {
            assert!(i.psi && i.detectable && matches!(i.pid, 0 | 0x1000));
            let pkt = &w[i.ordinal as usize * 188..][..188];
            let info = crate::rawts::classify_packet(pkt.try_into().unwrap()).unwrap();
            // Section body = after pointer field + 3-byte header, excluding the 4 CRC bytes.
            let sec = info.payload_off + 1 + usize::from(pkt[info.payload_off]);
            let len = (usize::from(pkt[sec + 1] & 0x0F) << 8) | usize::from(pkt[sec + 2]);
            let body = sec + 3..sec + 3 + len - 4;
            assert!(i.offsets.iter().all(|o| body.contains(o)), "{i:?}");
        }
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

    /// An injection appended mid-capture is judged like any other once a
    /// later PCR places it — but one whose coordinate has NO PCR anchor
    /// arrives too late to be placed at all, and must be left unresolved
    /// rather than blamed on a receiver that never saw that part of the
    /// stream.
    #[test]
    fn appended_injections_resolve_forward_and_anchorless_ones_strand() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
        };
        let mk = |pcr_base: Option<u64>, since: u64| Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base,
                since_pcr: since,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        };

        let mut a = Attribution::strict(vec![mk(Some(100), 2)], &hdr);
        a.on_pcr(100, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);

        // Appended after the capture is under way: anchored at a base
        // still to come, and at none at all.
        a.append(vec![mk(Some(200), 3), mk(None, 0)]);
        a.on_pcr(200, 500);
        a.on_signal(504, Some(0x1011), Signal::ContinuityJump);
        a.on_media(510, 0x1011);

        let rep = a.finish(2000);
        assert_eq!(rep.injected, 3);
        assert_eq!(rep.resolved, 2, "both anchored injections placed");
        assert_eq!(rep.unresolved, 1, "the anchorless late arrival is not");
        assert_eq!(rep.attributed_events, 2);
        assert!(rep.undetected.is_empty(), "{rep:?}");
        assert!(
            rep.unexplained_events.is_empty(),
            "{:?}",
            rep.unexplained_events
        );
    }

    /// Before the first PCR there is nothing to strand against, so an
    /// anchorless injection appended then still resolves at ordinal 0 —
    /// the same treatment the constructors give it.
    #[test]
    fn an_anchorless_injection_appended_before_any_pcr_still_resolves() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
        };
        let mut a = Attribution::strict(Vec::new(), &hdr);
        a.append(vec![Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: None,
                since_pcr: 4,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        }]);
        a.on_signal(6, Some(0x1011), Signal::ContinuityJump);
        a.on_media(10, 0x1011);
        let rep = a.finish(2000);
        assert_eq!(rep.resolved, 1);
        assert_eq!(rep.unresolved, 0);
        assert_eq!(rep.attributed_events, 1);
        assert!(rep.undetected.is_empty(), "{rep:?}");
    }

    #[test]
    fn psi_flip_drawn_on_a_media_packet_defers_to_the_next_psi_packet() {
        let b = baseline_bytes(60.0, "defer");
        let pid_at = |n: usize| -> u16 {
            crate::rawts::classify_packet(b[n * 188..][..188].try_into().unwrap())
                .unwrap()
                .pid
        };
        let is_psi = |n: usize| matches!(pid_at(n), 0 | 0x1000);

        let (_, text, _) = run_tap(&b, cfg(&[Class::PsiFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj.len() >= 2, "{} injection(s)", inj.len());
        // Whatever packet the draw fired on, every injection landed on a
        // PAT or PMT packet.
        for i in &inj {
            assert!(i.psi && matches!(i.pid, 0 | 0x1000), "{i:?}");
            assert!(is_psi(i.ordinal as usize), "ordinal {}", i.ordinal);
        }
        // The first draw fires at ordinal 0, which IS the PAT.
        assert_eq!(inj[0].ordinal, 0);
        // The second can only fire at ordinal >= min_gap, and that packet
        // is media — so the class was held and landed on the next PAT/PMT
        // instead of being re-drawn away.
        assert!(!is_psi(1000), "vacuous unless packet 1000 carries media");
        let next_psi = (1000..b.len() / 188)
            .find(|&n| is_psi(n))
            .expect("a PSI packet follows packet 1000");
        assert!(next_psi > 1000);
        assert_eq!(inj[1].ordinal as usize, next_psi);
    }

    // ---- Attribution ----

    fn hdr() -> LogHeader {
        LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
        }
    }
    fn inj(coord_pcr: u64, since: u64, class: Class, pid: u16, detectable: bool) -> Injection {
        Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: Some(coord_pcr),
                since_pcr: since,
            },
            class,
            pid,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable,
            psi: false,
            pes_start: false,
        }
    }

    /// The exact PMT packet that failed `corruption_detected` on the
    /// 1-hour re-smoke of main `6f0fbeb7` — injection ordinal 2938545,
    /// reconstructed byte for byte from that run's `corruption.jsonl`.
    ///
    /// 37 meaningful bytes then 151 of 0xFF stuffing: a 4-byte TS header,
    /// a zero pointer field, and a 29-byte PMT section (`table_id` 0x02
    /// at offset 5) carrying the video and KLV elementary streams.
    fn resmoke_pmt_packet() -> [u8; PKT] {
        let mut p = [0xffu8; PKT];
        p[..37].copy_from_slice(&[
            0x47, 0x50, 0x00, 0x14, 0x00, 0x02, 0xb0, 0x1d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0xf0,
            0x11, 0xf0, 0x00, 0x1b, 0xf0, 0x11, 0xf0, 0x00, 0x06, 0xf0, 0x31, 0xf0, 0x06, 0x05,
            0x04, 0x4b, 0x4c, 0x56, 0x41, 0x44, 0x55, 0x53, 0x36,
        ]);
        p
    }

    /// A `body_flip` on a PSI packet must never touch the bytes a decoder
    /// reads to FIND the section — the pointer field, the `table_id`, or
    /// either `section_length` byte.
    ///
    /// The re-smoke's failing injection flipped offset 5, which is the
    /// `table_id`. tst-core's `parse_pmt` rejects that before it ever
    /// reaches the CRC, and `psi_topology.rs` drops such a rejection
    /// silently (only `CrcMismatch` and `MultiSectionUnsupported` become
    /// `NonConformant` events) — so the same injection's flip at offset
    /// 14, inside the CRC'd span and therefore `detectable`, could never
    /// be observed. The tap was over-claiming.
    #[test]
    fn a_psi_body_flip_never_lands_on_the_section_header() {
        let pkt = resmoke_pmt_packet();
        let info = crate::rawts::classify_packet(&pkt).expect("the reconstructed packet parses");
        assert!(info.pusi, "it carries a section start");
        let sec = info.payload_off + 1 + usize::from(pkt[info.payload_off]);
        assert_eq!(sec, 5, "pointer field 0, so the section starts at 5");
        assert_eq!(pkt[sec], 0x02, "and byte 5 really is the PMT table_id");

        let body = psi_section_body_start(&pkt, &info, true).expect("a PSI section body");
        assert_eq!(body, sec + 3, "one past table_id + section_length");

        // And the same rule through the real tap, over 64 seeds of
        // generated traffic, so the PSI PIDs are learned from the PAT the
        // way a live run learns them.
        let b = baseline_bytes(300.0, "bfpsi");
        let mut psi_flips = 0;
        for seed in 1..=64u64 {
            let (_, text, _) = run_tap(
                &b,
                CorruptConfig {
                    seed,
                    ..cfg(&[Class::BodyFlip], 10_000, 1000)
                },
            );
            let (_, inj) = parse_log(&text).unwrap();
            for i in inj.iter().filter(|i| i.psi) {
                psi_flips += 1;
                let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
                let info = crate::rawts::classify_packet(&pkt).unwrap();
                let Some(body) = psi_section_body_start(&pkt, &info, true) else {
                    continue; // a section continuation, not a start
                };
                for &o in &i.offsets {
                    assert!(
                        o >= body,
                        "seed {seed}: offset {o} is in the section header \
                         (section at {}, body at {body})",
                        info.payload_off + 1 + usize::from(pkt[info.payload_off])
                    );
                }
            }
        }
        assert!(
            psi_flips >= 8,
            "only {psi_flips} PSI body flips across 64 seeds — the sweep is too thin to mean much"
        );
    }

    /// The other half: the flip is still DETECTABLE when it lands inside
    /// the CRC'd span. Narrowing the draw range must not have narrowed
    /// what the tap claims a receiver has to notice — offset 14, the very
    /// byte the re-smoke's injection also hit, still counts.
    #[test]
    fn a_psi_body_flip_inside_the_crc_span_is_still_detectable() {
        let pkt = resmoke_pmt_packet();
        let info = crate::rawts::classify_packet(&pkt).expect("parses");
        let span = sensitive_span(&pkt, &info, true).expect("a PSI sensitive span");
        assert!(
            (span.0..span.1).contains(&14),
            "offset 14 is inside the CRC'd span {span:?}"
        );
        // …and the span still starts at the section body and ends past
        // the CRC, unchanged by this fix.
        assert_eq!(span.0, 8, "body start");
        assert_eq!(span.1, 5 + 3 + 29, "through the end of the CRC");
    }

    /// Ruling A. Under transport loss an injection's own evidence can
    /// surface BEFORE `resolved_at`, because that ordinal is a receiver
    /// count plus a sender-side offset and the packets between the two
    /// are exactly what the network may have eaten. A forward-only window
    /// rejects it and reports the injection undetected for damage the
    /// receiver noticed perfectly well.
    ///
    /// Measured live: 2 of 429 detectable `garbage`/`header` injections
    /// on the arc's 1-hour rist leg, whose 100 ms PCR cadence gives the
    /// widest `since_pcr` spans.
    #[test]
    fn an_injections_own_event_may_surface_before_its_resolved_position() {
        // since_pcr 40: 25 of those 40 packets were lost in transit, so
        // the injected packet actually arrived at receiver ordinal 4975
        // while its coordinate resolves to 5000.
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960);
        a.on_signal(4975, None, Signal::Resync);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);

        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unexplained_total(), 0, "{:?}", r.unexplained_events);
    }

    /// The backward half is bounded by the injection's OWN `since_pcr`,
    /// not by the whole window: an event further back than the anchor
    /// span can reach is still unattributable.
    #[test]
    fn the_backward_reach_stops_at_the_anchor_span() {
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960); // resolves to 5000, window opens at 4960
        a.on_signal(4959, None, Signal::Resync);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.unexplained_resyncs, 1, "{r:?}");
    }

    /// The clamp that keeps windows disjoint. Two injections one
    /// `min_gap` apart must not both cover one event, or `hit` would hand
    /// it to the newer and rob the older of its detection — the exact
    /// ambiguity `CorruptConfig::validate`'s `min_gap` floor exists to
    /// prevent.
    ///
    /// `since_pcr` really does reach this far: measured at 302 packets on
    /// a 40 ms-PCR profile and 393 on a 100 ms one over the arc's 1-hour
    /// smoke, against the ~130 a one-PCR-interval model predicts. So the
    /// bound has to come from `min_gap`, not from a cadence assumption.
    #[test]
    fn a_wide_anchor_span_cannot_reach_into_its_predecessors_window() {
        let h = hdr();
        let budget = h.min_gap - (h.attribution_window + APPROX_SLACK) - 1;

        // The worst case for disjointness, built deliberately: the
        // PREDECESSOR resolves APPROXIMATELY, which widens its forward
        // edge by APPROX_SLACK, and the SUCCESSOR carries a `since_pcr`
        // wider than the budget, which is what the clamp has to cut back.
        // Anchoring the first at a base the receiver never saw (1000)
        // and delivering a LATER one (1090) is what makes it approx.
        let wide = budget + 200;
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Header, 0x1011, true),
                inj(2000, wide, Class::Header, 0x1011, true),
            ],
            &h,
        );
        // The predecessor's own base (1000) never arrives; the next one
        // does, so it resolves approximately to 5010 with a forward edge
        // widened by APPROX_SLACK.
        a.on_pcr(1090, 5000);
        // The successor resolves exactly, and — as two real consecutive
        // injections always are — exactly `min_gap` further down the
        // stream: 5010 + 1000 = 6010.
        a.on_pcr(2000, 6010 - wide);
        assert!(
            a.state(0).approx,
            "the predecessor must resolve approximately"
        );
        assert!(!a.state(1).approx, "and the successor exactly");
        assert_eq!(a.state(0).resolved_at, Some(5010));
        assert_eq!(a.state(1).resolved_at, Some(6010));

        // The clamp bit at all: the declared span really is wider than
        // the reach the config can fund.
        let reach = a.backward_reach(1);
        assert!(reach < wide, "the wide span must be clamped, got {reach}");

        // Both edges of `window_contains` are inclusive, so the budget
        // has to leave one packet between a widened forward edge and the
        // next window's lower edge. Sweep every ordinal either window can
        // touch and require that none is covered twice.
        let (lo0, hi1) = (
            a.window_lo(0).unwrap(),
            a.state(1).resolved_at.unwrap() + h.attribution_window,
        );
        for at in lo0..=hi1 {
            let covered = (0..2).filter(|&i| a.window_contains(i, at)).count();
            assert!(covered <= 1, "packet {at} covered by {covered} windows");
        }
        // …and the sweep is not vacuous: the two windows really do come
        // within a packet or two of each other.
        let gap = a.window_lo(1).unwrap()
            - (a.state(0).resolved_at.unwrap() + h.attribution_window + APPROX_SLACK);
        assert!(
            gap <= 2,
            "windows must be adjacent for this to test anything, gap {gap}"
        );
    }

    /// Ruling B. A continuity jump inside an injection's window, on the
    /// PID it damaged, excuses that injection when its class does NOT
    /// expect a jump: there the jump means the datagram carrying the
    /// injection was lost in transit, so the damage never reached the
    /// receiver to be noticed.
    ///
    /// The excusal survives X-CORR-04; the ATTRIBUTION does not. A signal
    /// of a class the injection cannot cause is no longer explained by it
    /// (`can_explain`), so the jump is unexplained — transport loss under
    /// lossy judgement, a discontinuity finding under strict — while
    /// still marking the injection's packet as plausibly missing.
    #[test]
    fn a_cc_jump_a_class_does_not_expect_stays_unexplained_but_still_excuses() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::PsiFlip, 0, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000);
            // Its own jump, inside its own window, attributed to it — and
            // NOT a signal `expects` accepts for a psi_flip.
            a.on_signal(5100, Some(0), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            a.finish(10_000)
        };

        let lossy = build(true);
        assert_eq!(
            lossy.attributed_events, 0,
            "class-incompatible: not explained"
        );
        assert_eq!(lossy.unexplained_transport_loss, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_lost, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_count, 0, "{:?}", lossy.undetected);

        // Strict has no transport to blame, so the finding stands.
        let strict = build(false);
        assert_eq!(strict.undetected_count, 1, "{strict:?}");
        assert_eq!(strict.undetected_lost, 0, "{strict:?}");
        assert_eq!(strict.unexplained_discontinuities, 1, "{strict:?}");
    }

    /// The other half of Ruling B, and the reason the blanket exclusion
    /// existed: for a class whose expected signals INCLUDE a continuity
    /// jump, its own jump is the detection and must never also excuse it.
    #[test]
    fn an_attributed_cc_jump_still_detects_a_class_that_expects_one() {
        for class in [Class::Drop, Class::Header, Class::Truncate, Class::Garbage] {
            let mut a = Attribution::lossy(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
            assert_eq!(
                r.undetected_lost, 0,
                "{class:?}: excused, not detected: {r:?}"
            );
        }
    }

    /// C2, the regression this exists for: on a lossy multi-day run the
    /// excused continuity jumps must not be able to crowd an unexplained
    /// RESYNC out of the evidence.
    ///
    /// A resync is the one signal with no second detection path — it is
    /// not a `DemuxEvent`, and resync mode is exactly what suppresses the
    /// `rawts_sync_loss` failure that would otherwise catch it — so if
    /// the sample cap swallows it, `corruption_attributed` passes a run
    /// whose receiver lost packet sync for reasons nothing explains.
    #[test]
    fn excused_jumps_never_crowd_out_an_unexplained_resync() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        // Comfortably more excusable jumps than the sample cap, none of
        // them inside any injection's window.
        for k in 0..(MAX_SAMPLES as u64 * 4) {
            a.on_signal(20_000 + k, Some(0x1011), Signal::ContinuityJump);
        }
        a.on_signal(90_000, None, Signal::Resync);
        let r = a.finish(100_000);

        assert_eq!(r.unexplained_transport_loss, MAX_SAMPLES as u64 * 4);
        assert_eq!(r.unexplained_resyncs, 1);
        assert_eq!(
            r.unexplained_total(),
            1,
            "the resync is the only corruption evidence here: {r:?}"
        );
        assert!(
            r.unexplained_events.iter().any(|e| e.contains("Resync")),
            "and it must be quotable in the failure: {:?}",
            r.unexplained_events
        );
    }

    /// X-CORR-04 (E04): a continuity jump on a PID an injection never
    /// touched is not that injection's evidence. Today `hit` places by
    /// window position alone, so the foreign jump is "attributed" and
    /// the `Drop` counts as detected.
    #[test]
    fn attribution_rejects_unrelated_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_discontinuities, 1, "{r:?}");
    }

    /// Same PID, wrong class: a `Drop` can only surface as a continuity
    /// jump, so a malformed-PES report on its own PID inside its window
    /// is somebody else's problem, not its detection.
    #[test]
    fn attribution_rejects_a_class_incompatible_signal_on_the_same_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1011), Signal::MalformedPes);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");
    }

    /// The loss-excusal follows the same rule: a jump on a foreign PID
    /// says nothing about whether THIS injection's packet was lost, so
    /// under lossy judgement it neither detects nor excuses a PSI flip
    /// (whose packet would have to be missing from PID 0 to be excused).
    #[test]
    fn loss_excusal_needs_a_jump_on_the_injections_own_pid() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected_lost, 0,
            "a foreign jump must not excuse: {r:?}"
        );
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_transport_loss, 1, "{r:?}");

        // Positive control: the jump on the PSI PID itself does excuse.
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.undetected_lost, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{r:?}");
    }

    /// Framing-wide classes reach every PID: a truncation, a garbage
    /// run or a destroyed sync byte breaks the multiplex, so a jump on
    /// any PID inside the window is theirs. A `Header` that rewrote the
    /// CC (offset 3) is confined to its own PID.
    #[test]
    fn framing_wide_injections_explain_signals_on_any_pid() {
        for class in [Class::Truncate, Class::Garbage] {
            let mut a = Attribution::strict(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            let r = a.finish(10_000);
            assert_eq!(r.attributed_events, 1, "{class:?}: {r:?}");
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
        }
        let sync_byte = |offsets: Vec<usize>| {
            let mut i = inj(1000, 10, Class::Header, 0x1011, true);
            i.offsets = offsets;
            let mut a = Attribution::strict(vec![i], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            a.finish(10_000)
        };
        assert_eq!(
            sync_byte(vec![0]).attributed_events,
            1,
            "sync-byte header damage is framing-wide"
        );
        assert_eq!(
            sync_byte(vec![3]).attributed_events,
            0,
            "a CC rewrite is PID-local"
        );
    }

    /// META-13: a resync is the HARNESS's raw reader noticing, not the
    /// receiver. A garbage run detected only by a resync is counted
    /// apart from a drop the demuxer itself reported.
    #[test]
    fn detection_credit_splits_reader_resyncs_from_demux_events() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Garbage, 0x1011, true),
                inj(1000, 2010, Class::Drop, 0x1011, true),
                inj(1000, 4010, Class::Garbage, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_signal(5015, None, Signal::Resync); // garbage #1: reader only
        a.on_media(5100, 0x1011);
        a.on_signal(7015, Some(0x1011), Signal::ContinuityJump); // drop: demux
        a.on_media(7100, 0x1011);
        a.on_signal(9012, None, Signal::Resync); // garbage #2: reader …
        a.on_signal(9015, Some(0x1011), Signal::ContinuityJump); // … then demux too
        a.on_media(9100, 0x1011);
        let r = a.finish(20_000);
        assert_eq!(r.undetected_count, 0, "{r:?}");
        assert_eq!(r.detected_by_demux, 2, "{r:?}");
        assert_eq!(r.detected_by_reader_only, 1, "{r:?}");
        assert_eq!(
            r.detected_by_reader_only_per_class.get("garbage"),
            Some(&1),
            "{r:?}"
        );
    }

    /// The uncapped counters keep counting after the sample list stops.
    /// A verdict reading `unexplained_events.len()` would report 64 for
    /// any run above the cap, which is how a badly-broken run reads as a
    /// mildly-broken one.
    #[test]
    fn unexplained_counts_are_uncapped_while_the_sample_list_is_capped() {
        let mut a = Attribution::strict(vec![], &hdr());
        let n = MAX_SAMPLES as u64 * 10;
        for k in 0..n {
            a.on_signal(1000 + k, Some(0x1011), Signal::PsiChecksum);
        }
        let r = a.finish(100_000);
        assert_eq!(r.unexplained_events.len(), MAX_SAMPLES);
        assert_eq!(r.unexplained_nonconformant, n);
        assert_eq!(r.unexplained_total(), n);
    }

    /// The same cap/count split for the per-injection verdicts (I4):
    /// `undetected`/`unrecovered` are samples, `*_count` are the truth.
    #[test]
    fn undetected_and_unrecovered_lists_are_capped_and_their_counts_are_not() {
        let n = MAX_SAMPLES as u64 * 3;
        // Detectable injections, spaced past every window, none of which
        // ever produces an event or any media afterwards.
        let log: Vec<Injection> = (0..n)
            .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
            .collect();
        let mut a = Attribution::strict(log, &hdr());
        a.on_pcr(1000, 0);
        // Walk the cursors past every window so the retirement path does
        // the counting, not just the final sweep.
        a.on_media(10 + n * 2000 + 10_000, 0x2222);
        let r = a.finish(10 + n * 2000 + 20_000);

        assert_eq!(r.undetected_count, n);
        assert_eq!(r.unrecovered_count, n);
        assert_eq!(r.undetected.len(), MAX_SAMPLES);
        assert_eq!(r.unrecovered.len(), MAX_SAMPLES);
    }

    /// C1, the memory half: the engine must stop holding the log.
    ///
    /// A 72-hour soak logs a quarter of a million injections, each
    /// carrying `before`/`after` byte images, against a receive process
    /// whose own `rss_slope_*_recv` verdict gates at 200 KiB/h. Retained
    /// state has to be bounded by the window the cursors are actually
    /// working in, not by the length of the run.
    #[test]
    fn a_long_run_retires_injections_instead_of_accumulating_them() {
        let n = 20_000u64;
        let mut a = Attribution::strict(vec![], &hdr());
        let mut peak = 0usize;
        for k in 0..n {
            let at = 10 + k * 2000;
            a.append(vec![inj(1000, at, Class::Drop, 0x1011, true)]);
            // A live receiver feeds every PCR it decodes, which is what
            // turns a logged coordinate into a receiver position — and
            // therefore what lets the resolution cursor move on.
            a.on_pcr(1000, 0);
            // One event per injection, inside its own window, which is
            // what walks the scan cursors forward.
            a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
            a.on_media(at + 10, 0x1011);
            peak = peak.max(a.retained());
        }
        assert!(
            peak <= PRUNE_BATCH + 16,
            "retained {peak} injections at peak against a {PRUNE_BATCH}-injection prune batch"
        );

        // …and the verdict is unchanged by the pruning: every one of them
        // was detected and recovered.
        let r = a.finish(10 + n * 2000 + 10_000);
        assert_eq!(r.injected, n);
        assert_eq!(r.resolved, n);
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unrecovered_count, 0, "{:?}", r.unrecovered);
    }

    /// Pruning must not change a verdict. The same event stream is judged
    /// twice — once long enough to retire most of the log, once short
    /// enough that nothing retires — and the two reports must agree on
    /// every counter.
    #[test]
    fn retiring_injections_does_not_change_the_verdict() {
        let judge = |n: u64| {
            let log: Vec<Injection> = (0..n)
                .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
                .collect();
            let mut a = Attribution::strict(log, &hdr());
            a.on_pcr(1000, 0);
            for k in 0..n {
                let at = 10 + k * 2000;
                // Every third injection is left with no event at all, so
                // both the passing and the failing paths are exercised.
                if k % 3 != 0 {
                    a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
                }
                a.on_media(at + 10, 0x1011);
            }
            a.finish(10 + n * 2000 + 10_000)
        };
        // Below the batch nothing is ever retired; well above it, most of
        // the log is. Both must produce the arithmetically exact verdict.
        for n in [100u64, PRUNE_BATCH as u64 * 3] {
            let r = judge(n);
            assert_eq!(r.injected, n, "n={n}: {r:?}");
            assert_eq!(r.resolved, n, "n={n}: {r:?}");
            assert_eq!(r.detectable, n, "n={n}: {r:?}");
            // Exactly the every-third injections that got no event.
            assert_eq!(r.undetected_count, n.div_ceil(3), "n={n}: {r:?}");
            assert_eq!(r.unrecovered_count, 0, "n={n}: {r:?}");
        }
    }

    #[test]
    fn attribution_explains_events_inside_the_window_and_flags_the_rest() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // injection resolves to receiver ordinal 5010
        a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
        a.on_media(5100, 0x1011); // recovered
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump); // unexplained
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.attributed_discontinuities, 1);
        assert_eq!(r.unexplained_events.len(), 1);
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
        assert_eq!(r.resolved, 1);
    }

    /// Transport-loss excusal, the Lossy half. An unexplained
    /// `ContinuityJump` says packets went missing, which on a live
    /// impaired link is the link doing its job — it is already counted in
    /// the verifier's own `discontinuities`, and it is not corruption
    /// evidence. `Strict` keeps failing on the identical input, because a
    /// file has no transport to lose anything.
    #[test]
    fn lossy_excuses_an_unexplained_continuity_jump_and_strict_does_not() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::Header, 0x1011, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
            a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
            a.on_media(5100, 0x1011);
            // Far outside every window: nothing in the log explains it.
            a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
            a
        };

        let lossy = build(true).finish(10_000);
        assert!(
            lossy.unexplained_events.is_empty(),
            "a transport gap is not corruption evidence: {:?}",
            lossy.unexplained_events
        );
        assert_eq!(lossy.unexplained_transport_loss, 1);
        // The raw tallies still reconcile: the event was counted, just not
        // held against the tap.
        assert_eq!(lossy.events, 2);
        assert_eq!(lossy.attributed_events, 1);

        let strict = build(false).finish(10_000);
        assert_eq!(strict.unexplained_events.len(), 1);
        assert_eq!(strict.unexplained_discontinuities, 1);
        assert_eq!(strict.unexplained_transport_loss, 0);
    }

    /// The other families are NOT excused: a dropped packet cannot forge a
    /// bad PSI CRC or a malformed PES header, so an unexplained one of
    /// those still fails in Lossy exactly as in Strict.
    #[test]
    fn lossy_still_fails_an_unexplained_nonconformance_or_resync() {
        for sig in [
            Signal::PsiChecksum,
            Signal::MalformedPes,
            Signal::OtherNonConformant,
            Signal::Resync,
        ] {
            let mut a =
                Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(9000, Some(0x1011), sig);
            let r = a.finish(10_000);
            assert_eq!(
                r.unexplained_events.len(),
                1,
                "{sig:?} must survive transport-loss excusal"
            );
            assert_eq!(r.unexplained_transport_loss, 0, "{sig:?}");
        }
    }

    /// The excusal is evidence-driven, not blanket: with NO continuity
    /// jump in its window, an undetected injection still fails in Lossy.
    /// Without this the whole corruption suite would go vacuous.
    #[test]
    fn lossy_without_a_cc_jump_in_the_window_still_reports_undetected() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        // A jump far outside the window explains nothing about it.
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "{r:?}");
        assert_eq!(r.undetected_lost, 0);
    }

    /// An injection's OWN continuity jump is its own damage surfacing, not
    /// evidence that the network ate it, so it must not excuse that
    /// injection's recovery obligation. `Drop` is the class this protects:
    /// its only observable IS a continuity jump, so a self-excusing rule
    /// would let every drop arrive pre-excused and never have to show the
    /// stream recovering.
    #[test]
    fn an_injections_own_cc_jump_does_not_excuse_its_recovery() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to 5010
        // Its own jump, inside its own window, attributed to it.
        a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
        // No media on 0x1011 afterwards, so it never recovers.
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "the jump IS attributed: {r:?}");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "a drop keeps its full recovery obligation: {r:?}"
        );
        assert_eq!(r.unrecovered_lost, 0, "{r:?}");
    }

    /// The other half of the same rule: a jump from a DIFFERENT injection
    /// still excuses. `explainer` attributes a jump to the newest covering
    /// injection that can cause it, so an older one whose window also
    /// covers it is excused — which needs overlapping windows, the case a
    /// real tap config forbids (`min_gap >= 2 * ATTRIBUTION_WINDOW`) but
    /// an `approx` resolution's widened window can still produce.
    ///
    /// Both injections sit on the PSI PID because the excusal is
    /// PID-constrained too (X-CORR-04): a gap on a PID an injection never
    /// touched says nothing about whether ITS packet went missing, so
    /// only a jump the older injection could itself have suffered counts.
    #[test]
    fn a_foreign_cc_jump_in_the_window_still_excuses() {
        // Two injections 100 packets apart: deliberately closer than
        // `min_gap` so their windows overlap and the newer one can own a
        // jump that also falls inside the older one's window.
        let build = |lossy: bool| {
            let log = vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(1000, 110, Class::Drop, 0, true),
            ];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolve to 5010 and 5110
            // Inside BOTH windows and on a PID both injections damaged;
            // `explainer` gives it to the newer (the Drop, the only one of
            // the two whose class expects a jump), so the PsiFlip sees it
            // as foreign.
            a.on_signal(5200, Some(0), Signal::ContinuityJump);
            a
        };

        // The psi_flip is detectable and expects a PsiChecksum event it
        // never got, so it is BOTH undetected and unrecovered — one
        // fixture covering both excused counters.
        let lossy = build(true).finish(10_000);
        assert_eq!(
            (lossy.undetected_lost, lossy.unrecovered_lost),
            (1, 1),
            "the psi_flip is excused by the drop's foreign jump: {lossy:?}"
        );
        assert!(
            lossy.undetected.is_empty(),
            "nothing else was undetected: {lossy:?}"
        );
        assert_eq!(
            lossy.unrecovered.len(),
            1,
            "and the drop itself is NOT excused by its own jump: {lossy:?}"
        );
        assert!(
            lossy.unrecovered[0].contains("drop"),
            "the surviving one must be the drop: {lossy:?}"
        );

        // Strict excuses neither, as always.
        let strict = build(false).finish(10_000);
        assert_eq!(strict.undetected.len(), 1, "{strict:?}");
        assert_eq!(strict.unrecovered.len(), 2, "{strict:?}");
        assert_eq!((strict.undetected_lost, strict.unrecovered_lost), (0, 0));
    }

    /// `explains_damage` answers for a content oracle, records nothing,
    /// and covers exactly two cases: same PID any class, or Truncate/
    /// Garbage on any PID (both destroy framing multiplex-wide).
    #[test]
    fn explains_damage_covers_same_pid_any_class_and_framing_classes_anywhere() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::BodyFlip, 0x1031, false),
                inj(2000, 10, Class::Truncate, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000); // body flip on 0x1031 resolves to 5010
        a.on_pcr(2000, 7000); // truncate on 0x1011 resolves to 7010

        // Same PID, any class.
        assert!(a.explains_damage(5010, 0x1031));
        assert!(a.explains_damage(5010 + ATTRIBUTION_WINDOW, 0x1031));
        // A body flip on another PID explains nothing there.
        assert!(!a.explains_damage(5010, 0x1011));
        // Truncation breaks framing for the whole multiplex.
        assert!(a.explains_damage(7010, 0x1031));
        assert!(a.explains_damage(7010, 0x1011));
        // Past the window, nothing is explained.
        assert!(!a.explains_damage(7011 + ATTRIBUTION_WINDOW, 0x1031));

        // It recorded nothing: no events, no detection, no recovery.
        let r = a.finish(10_000);
        assert_eq!(r.events, 0);
        assert_eq!(r.attributed_events, 0);
    }

    #[test]
    fn attribution_flags_undetected_and_unrecovered_injections() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(2000, 0, Class::Dup, 0x1011, false),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_pcr(2000, 7000);
        a.on_media(7100, 0x1011); // dup recovered (no detection required)
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "psi_flip had no PsiChecksum event");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "psi_flip (pid 0) never saw media after it"
        );
    }

    #[test]
    fn attribution_resolves_an_unseen_pcr_base_to_the_next_seen_one_wrap_aware() {
        let near_wrap = (1u64 << 33) - 10;
        let mut a = Attribution::strict(
            vec![inj(near_wrap, 3, Class::Truncate, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(5, 8000); // wrapped past 2^33; first base "at or after" near_wrap
        a.on_signal(8010, None, Signal::Resync);
        a.on_media(8020, 0x1011);
        let r = a.finish(9000);
        assert_eq!(r.resolved, 1);
        assert_eq!(r.resyncs, 1);
        assert!(
            r.unexplained_events.is_empty() && r.undetected.is_empty() && r.unrecovered.is_empty()
        );
    }

    #[test]
    fn approximate_resolution_is_bounded_so_an_outage_strands_an_injection() {
        // Just inside the bound: the logged anchor base never arrived (the
        // corruption destroyed the packet carrying it), but the next base
        // is close enough to place the injection approximately.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());

        // One tick further — a reconnect outage, through which the sender
        // kept logging and the receiver saw nothing. Resolving here would
        // pile every injection logged during the outage onto this one
        // ordinal and report all but one as undetected, so the injection
        // stays unresolved and is judged for nothing.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS + 1, 5000);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (0, 1));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
    }

    #[test]
    fn a_stranded_injection_does_not_block_the_ones_after_it() {
        // The cursor must step over an injection it can never place, or
        // everything logged after the outage becomes unattributable too.
        let mut a = Attribution::strict(
            vec![
                inj(2000, 0, Class::Header, 0x1011, true),
                inj(2000 + 2 * MAX_APPROX_TICKS, 0, Class::Header, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(2000 + 2 * MAX_APPROX_TICKS, 5000); // strands #0, resolves #1
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 1));
        assert_eq!(
            r.attributed_events, 1,
            "the surviving injection is reachable"
        );
        assert!(r.undetected.is_empty());
    }

    #[test]
    fn expected_signal_sets_cover_the_non_conformance_shapes() {
        // An adaptation-field-length overrun (a `header` sub-kind) is
        // reported as a plain non-conformance, not a resync, so it must
        // still count as having noticed the injection.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::OtherNonConformant);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);
        assert_eq!(r.attributed_nonconformant, 1);

        // A flipped pointer field or section length surfaces as a
        // table-id/section-length non-conformance before the CRC is
        // reached.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::OtherNonConformant);
        a.on_media(5020, 0);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);

        // But the sets are not wide open: a dropped packet is a continuity
        // jump and nothing else, so a resync must not be mistaken for
        // having noticed it.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, None, Signal::Resync);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected.len(),
            1,
            "a resync does not prove a drop was seen"
        );
    }

    #[test]
    fn attribution_reports_an_injection_whose_window_never_arrived_as_unresolved() {
        let a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        let r = a.finish(100);
        assert_eq!(r.unresolved, 1);
        assert!(
            r.undetected.is_empty(),
            "an unresolved injection is not judged"
        );
    }
}
