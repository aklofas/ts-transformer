//! LATM/LOAS framing: the sync validator, and the demuxer's LOAS resync.
//!
//! Spec: ISO/IEC 14496-3 §1.7 (LATM / LOAS framing) — referenced by
//! ITU-T H.222.0 Table 2-34 stream_type `0x11` (`AudioAacLatm`).
//!
//! ## Scope
//!
//! AAC-LATM PES payloads are sequences of LOAS-framed `audioMuxElement`
//! records. Each record begins with a 24-bit LOAS sync header:
//!
//! ```text
//!   syncword:           11 bits = 0x2B7
//!   audioMuxLengthBytes: 13 bits
//! ```
//!
//! On the wire the first two bytes match the pattern
//! `[0x56, 0xE0 .. 0xFF]` (sync = `0x2B7` shifted left 5 bits):
//!
//! ```text
//!   bytes[0] = 0x56                 (0x2B7 >> 3)
//!   bytes[1] = 0b111x_xxxx          (top 3 bits = remaining sync;
//!                                    low 5 bits = top of length)
//! ```
//!
//! The public [`validate_latm_sync`] checks the LOAS syncword and the
//! length of a PES payload's first record. The crate-private rest is the
//! demuxer's: the walk over a PES's records, and the resync that finds the
//! record position of an AAC-LATM PID after a join or a loss and decides
//! when a PID is not LOAS. Full `audioMuxElement` decode (per ISO/IEC
//! 14496-3 §1.7.3) is deferred
//! — consumers needing AudioSpecificConfig + raw_data_block walks must
//! integrate a downstream decoder (e.g. ffmpeg).
//!
//! ## Rationale
//!
//! Without sync validation, a demuxer that advertises `stream_type=0x11`
//! (AAC-LATM) lets malformed LATM streams (wrong syncword, audio shipped
//! without LATM wrapping on a 0x11 PID) silently produce `Sample` events
//! with garbage payload. Downstream decoders then report cryptic parse
//! errors that don't correlate back to the conformance bug. The demuxer
//! follows each AAC-LATM PID's record position across PES and surfaces a
//! missing syncword as
//! [`crate::mpegts::demux::NonConformantIssue::LatmFraming`] (that variant
//! says when); a record or a header that continues into the next PES is
//! not a fault (ISO/IEC 14496-3 §1.7.2).
//!
//! ## Lenient vs. strict LATM validation
//!
//! Lenient mode (`StrictMode::Off`): the demuxer surfaces the issue as
//! a `NonConformant` event alongside the `Sample` event (permissive —
//! consumers may still want the bytes for forensic analysis).
//! Strict mode (`StrictMode::Full`): the `Sample` event is suppressed
//! and the issue propagates as `DemuxError::StrictRejection`.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// First wire byte of the 11-bit LOAS syncword (`0x2B7 >> 3`).
const LOAS_SYNC_BYTE0: u8 = 0x56;
/// Top 3 bits of byte 1 must equal `0b111` (low 3 bits of the 11-bit
/// `0x2B7` sync, shifted into the top of byte 1).
const LOAS_SYNC_BYTE1_MASK: u8 = 0b1110_0000;
const LOAS_SYNC_BYTE1_VALUE: u8 = 0b1110_0000;

/// Specific LATM/LOAS framing violation detected in a PES payload.
///
/// `#[non_exhaustive]` — future violations (e.g. an
/// `AudioMuxElementMalformed` variant once full decode lands) can be
/// added without breaking matchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LatmFramingKind {
    /// The PES payload does not begin with a valid LOAS syncword.
    /// Either an off-by-one offset (some encoders prepend a `0x00`
    /// byte before the sync), the wrong codec entirely shipped on a
    /// `stream_type=0x11` PID, or a corrupt PES boundary.
    MissingSyncword,
    /// The LOAS header parsed but its declared `audioMuxLengthBytes`
    /// runs past the end of the PES payload.
    ///
    /// [`validate_latm_sync`] still returns it. The demuxer does not report
    /// it: ISO/IEC 14496-3 §1.7.2 lets an AudioSyncStream record continue
    /// into the next PES, so a record running past its PES is not a fault.
    AudioMuxLengthOverrun,
    /// PES payload is shorter than the 3-byte LOAS header — cannot
    /// validate the sync word or read the length field.
    ///
    /// [`validate_latm_sync`] still returns it. The demuxer does not report
    /// it: a LOAS header may be split across PES (ISO/IEC 14496-3 §1.7.2).
    Truncated,
}

/// Validate the LATM/LOAS sync header at the start of an AAC-LATM PES
/// payload.
///
/// Returns `Ok(audio_mux_length_bytes)` on a valid sync — the value is
/// the declared payload length of the first `audioMuxElement` (excluding
/// the 3-byte LOAS header itself), useful for callers that want to walk
/// subsequent records.
///
/// On failure returns the specific [`LatmFramingKind`] so the caller can
/// route to the appropriate `NonConformantIssue` variant.
///
/// `pes_payload` must be a complete PES payload: only its first record is
/// checked, against the bytes given, so part of a payload reports
/// `AudioMuxLengthOverrun` (or `Truncated`) for a record that is whole on
/// the wire.
///
/// # Spec reference
///
/// - ISO/IEC 14496-3 §1.7.2 — LOAS syncword + length.
/// - H.222.0 Table 2-34 — stream_type 0x11 binding to AAC-LATM.
pub fn validate_latm_sync(pes_payload: &[u8]) -> Result<u16, LatmFramingKind> {
    // The 3-byte LOAS header carries the sync word and 13-bit length.
    if pes_payload.len() < 3 {
        return Err(LatmFramingKind::Truncated);
    }
    let b0 = pes_payload[0];
    let b1 = pes_payload[1];
    let b2 = pes_payload[2];
    if b0 != LOAS_SYNC_BYTE0 || (b1 & LOAS_SYNC_BYTE1_MASK) != LOAS_SYNC_BYTE1_VALUE {
        return Err(LatmFramingKind::MissingSyncword);
    }
    // audioMuxLengthBytes: low 5 bits of byte 1 + all 8 bits of byte 2
    // = 13-bit big-endian value.
    let audio_mux_length_bytes: u16 = ((u16::from(b1 & 0x1F)) << 8) | u16::from(b2);
    let total_record_len = 3usize + usize::from(audio_mux_length_bytes);
    if total_record_len > pes_payload.len() {
        return Err(LatmFramingKind::AudioMuxLengthOverrun);
    }
    Ok(audio_mux_length_bytes)
}

/// The longest LOAS record on the wire: the 3-byte header plus a 13-bit
/// `audioMuxLengthBytes` of at most `0x1FFF`.
pub(crate) const MAX_LOAS_RECORD_BYTES: usize = 3 + 0x1FFF;

/// How many syncword candidates a PID's resync follows at once
/// ([`LoasSearch`]). Random record bodies hold a false candidate about every
/// 2 048 bytes, and one lives until its record would end, so a few are
/// pending at a time; a full set ignores a newcomer.
pub(crate) const LOAS_CANDIDATES: usize = 16;

/// Payload bytes a PID may stay unsynced whatever its candidates promise. A
/// LOAS record starts within one record length of where the position was
/// lost; it passes its first boundary within one more record and is
/// confirmed at the next, so a sound stream is synced within three record
/// lengths (3 bytes more for a header split across PES).
pub(crate) const LOAS_UNSYNCED_BOUND: usize = 3 * MAX_LOAS_RECORD_BYTES + 3;

/// The LOAS framing state of one AAC-LATM PID (ISO/IEC 14496-3 §1.7.2: a
/// record, and its header, may continue into the next PES).
///
/// - Position known: the carry of the last walk is followed, and a record
///   boundary without a syncword is an issue at once. A walk that ends
///   exactly on its PES's end leaves the next record starting at the next
///   PES's first byte (the AudioSyncStream is one run of records, ISO/IEC
///   14496-3 Table 1.36), so that PES is walked from there.
/// - Position unknown (a PID's first PES; after a loss, `reset_sync`, a
///   retirement or a change of the PID's kind; after a boundary without a
///   syncword was reported): a PES with `data_alignment_indicator` set,
///   an explicit promise of a record start, is walked from 0, a missing
///   syncword flagged at once. Any other PES may open inside a record, even
///   when it starts with a syncword, and feeds the [`LoasSearch`].
///
/// A position known after a boundary without a syncword is lost, and the
/// unsynced stretch that follows has had its issue.
#[derive(Debug, Default)]
pub(crate) struct LoasFraming {
    /// What the last record still owes the next PES; `None` = unknown.
    position: Option<LoasCarry>,
    /// The resync, while the position is unknown; `None` when nothing is
    /// held or counted.
    search: Option<Box<LoasSearch>>,
}

impl LoasFraming {
    /// Walk one PES payload; the framing violation to report, if any.
    /// `aligned` is the PES's `data_alignment_indicator`.
    pub(crate) fn feed(&mut self, payload: &[u8], aligned: bool) -> Option<LatmFramingKind> {
        let len = payload.len();
        let start = match self.position.take() {
            Some(carry) => match carry.resolve(payload) {
                Ok(LoasCarry::Body(n)) if len <= n => {
                    self.position = Some(LoasCarry::Body(n - len));
                    return None;
                }
                Ok(LoasCarry::Body(n)) => n,
                Ok(header) => {
                    self.position = Some(header);
                    return None;
                }
                Err(kind) => return Some(self.lost_at_once(len, kind)),
            },
            None if aligned => 0,
            None => {
                let search = self.search.get_or_insert_with(Box::default);
                return match search.feed(payload) {
                    LoasSettle::Confirmed(carry) => {
                        self.position = Some(carry.unwrap_or(LoasCarry::Body(0)));
                        self.search = None;
                        None
                    }
                    LoasSettle::Unsynced => None,
                    LoasSettle::NotLoas => Some(LatmFramingKind::MissingSyncword),
                };
            }
        };
        match walk_loas(payload, start) {
            Ok(carry) => {
                self.position = Some(carry.unwrap_or(LoasCarry::Body(0)));
                self.search = None;
                None
            }
            Err(kind) => Some(self.lost_at_once(len, kind)),
        }
    }

    /// Bytes of the PID were lost: the position is unknown, the candidates
    /// are dropped and the count starts again. Kept across a loss, the count
    /// would flag a sound stream that loses a PES more often than every few
    /// records; a PID that is not LOAS and loses as often is not flagged,
    /// and each of its losses reports itself.
    pub(crate) fn forget(&mut self) {
        self.position = None;
        self.search = None;
    }

    /// `true` when nothing is known, held or counted.
    pub(crate) fn is_clear(&self) -> bool {
        self.position.is_none() && self.search.is_none()
    }

    /// A state with a known position and a search, for tests that need
    /// every per-PID field set.
    #[cfg(test)]
    pub(crate) fn populated_for_test() -> Self {
        Self {
            position: Some(LoasCarry::Body(7)),
            search: Some(Box::default()),
        }
    }

    /// A boundary of this PES lacks the syncword, reported at once: the
    /// position is lost, and the stretch this PES starts or continues has
    /// had its issue.
    fn lost_at_once(&mut self, len: usize, kind: LatmFramingKind) -> LatmFramingKind {
        let search = self.search.get_or_insert_with(Box::default);
        search.unsynced_bytes = search.unsynced_bytes.saturating_add(len);
        search.raised = true;
        kind
    }
}

/// The resync of a PID whose LOAS position is unknown. Every byte-aligned
/// syncword candidate within the first [`MAX_LOAS_RECORD_BYTES`] bytes of a
/// PES (the tail of a record carried in from an earlier PES is never longer),
/// and a lone `0x56` as its last byte, is followed to the record boundary it
/// predicts, up to [`LOAS_CANDIDATES`] at once.
///
/// A candidate passes a boundary when a header sits there and the walk from
/// it to the end of that PES succeeds; one whose first record fits inside the
/// PES it is found in is walked there. A candidate that fails is dropped
/// silently. Its first pass makes a candidate a provisional position; its
/// second confirms it, which ends the search. A false candidate passes a
/// boundary about once in 2 048, and a false confirmation clears the genuine
/// candidate and makes the next boundary a missing syncword, so one pass is
/// not trusted. A stream whose record bodies carry byte-aligned syncword
/// patterns at short intervals can still be misread, after a join and again
/// after each misreading: false candidates then pass two boundaries often
/// enough for one to be confirmed, and its next boundary raises a
/// `MissingSyncword`, which loses the position again.
///
/// Every payload byte since the position was lost is counted, and the
/// stretch is judged not LOAS, with one `MissingSyncword` and nothing more
/// until the search ends, when the count passes one record length with no
/// candidate pending, or passes [`LOAS_UNSYNCED_BOUND`] whatever is pending.
/// A provisional position defers the judgement until its next boundary (a
/// sound PES longer than the bound may hold the record a join found), but
/// only until the first provisional position of the stretch has failed:
/// from then the bound applies whatever is held, so deferring cannot be
/// renewed. Whatever the stream, a stretch that no candidate confirms is
/// therefore flagged by the PES that takes the count past the bound, or, if
/// a provisional position is pending then and none has failed, by the PES
/// that holds the next boundary of the earliest one, which lies less than
/// one record length (8 194 bytes) past the PES that made it provisional.
///
/// Work per PES is linear in its length: the walks of one PES share a set of
/// the boundaries already walked, and a walk that reaches one ends as the
/// earlier walk did, so no boundary is walked twice. That set lives only
/// while the PES is settled; between PES a search holds its candidates and
/// two counters.
#[derive(Debug)]
struct LoasSearch {
    /// The candidates followed, `held[..held_len]`.
    held: [LoasCandidate; LOAS_CANDIDATES],
    held_len: usize,
    /// Payload bytes since the position was lost.
    unsynced_bytes: usize,
    /// This stretch's issue was raised.
    raised: bool,
    /// A provisional position failed in this stretch: no candidate defers
    /// the judgement any more.
    provisional_failed: bool,
}

impl Default for LoasSearch {
    fn default() -> Self {
        Self {
            held: [LoasCandidate::pending(LoasCarry::Body(0)); LOAS_CANDIDATES],
            held_len: 0,
            unsynced_bytes: 0,
            raised: false,
            provisional_failed: false,
        }
    }
}

/// One syncword candidate of a [`LoasSearch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoasCandidate {
    /// What its record, or its header, still owes the next PES.
    carry: LoasCarry,
    /// It passed one record boundary: a provisional position.
    passed: bool,
}

impl LoasCandidate {
    fn pending(carry: LoasCarry) -> Self {
        Self {
            carry,
            passed: false,
        }
    }

    /// Passed a boundary; the walk from it ended with `carry` (`None`: on a
    /// record boundary, so the next one is the next PES's first byte).
    fn passed(carry: Option<LoasCarry>) -> Self {
        Self {
            carry: carry.unwrap_or(LoasCarry::Body(0)),
            passed: true,
        }
    }
}

/// What one PES did to a [`LoasSearch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoasSettle {
    /// A provisional position passed its second boundary: the walk's carry
    /// (see [`walk_loas`]). The search is over.
    Confirmed(Option<LoasCarry>),
    /// Still searching.
    Unsynced,
    /// The stretch is judged not LOAS: report `MissingSyncword`, once.
    NotLoas,
}

/// How a walk over one PES ended ([`walk_unwalked`]).
enum Walked {
    /// It reached the end of the payload: its carry (see [`walk_loas`]).
    Ends(Option<LoasCarry>),
    /// A boundary lacks the syncword.
    Refuted,
    /// It reached a boundary an earlier walk of this payload took, so it
    /// ends as that one did: its candidate is that one's duplicate, or is
    /// refuted with it.
    Joined,
}

impl LoasSearch {
    /// Settle the held candidates against the next PES payload, scan it for
    /// new ones, and count it.
    fn feed(&mut self, payload: &[u8]) -> LoasSettle {
        self.feed_counting(payload, &mut 0)
    }

    /// [`Self::feed`], adding to `steps` each record boundary it walks.
    fn feed_counting(&mut self, payload: &[u8], steps: &mut usize) -> LoasSettle {
        let mut walked: Vec<u64> = alloc::vec![0; payload.len().div_ceil(64)];
        let mut walks = 0;
        let confirmed = self.settle(payload, &mut walked, &mut walks);
        debug_assert!(walks <= payload.len(), "a boundary was walked twice");
        debug_assert!(self.held_len <= LOAS_CANDIDATES);
        *steps += walks;
        if let Some(carry) = confirmed {
            return LoasSettle::Confirmed(carry);
        }
        self.unsynced_bytes = self.unsynced_bytes.saturating_add(payload.len());
        let held = &self.held[..self.held_len];
        let deferred = !self.provisional_failed && held.iter().any(|c| c.passed);
        if self.raised || deferred {
            return LoasSettle::Unsynced;
        }
        let not_loas = (self.unsynced_bytes > MAX_LOAS_RECORD_BYTES && held.is_empty())
            || self.unsynced_bytes > LOAS_UNSYNCED_BOUND;
        if not_loas {
            self.raised = true;
            return LoasSettle::NotLoas;
        }
        LoasSettle::Unsynced
    }

    /// Settle the held candidates and scan `payload` for new ones; the carry
    /// of the provisional position it confirms, if any.
    fn settle(
        &mut self,
        payload: &[u8],
        walked: &mut [u64],
        steps: &mut usize,
    ) -> Option<Option<LoasCarry>> {
        let len = payload.len();
        let (held, held_len) = (self.held, core::mem::take(&mut self.held_len));
        let held = &held[..held_len];
        // Provisional positions first: a candidate whose walk joins an
        // earlier one's is dropped as its duplicate, so it must not be the
        // one that has passed a boundary.
        for c in held
            .iter()
            .filter(|c| c.passed)
            .chain(held.iter().filter(|c| !c.passed))
        {
            let boundary = match c.carry.resolve(payload) {
                Ok(LoasCarry::Body(n)) if len <= n => {
                    self.hold(LoasCandidate {
                        carry: LoasCarry::Body(n - len),
                        ..*c
                    });
                    continue;
                }
                Ok(LoasCarry::Body(n)) => n,
                Ok(header) => {
                    self.hold(LoasCandidate {
                        carry: header,
                        ..*c
                    });
                    continue;
                }
                Err(_) => {
                    self.provisional_failed |= c.passed;
                    continue;
                }
            };
            match walk_unwalked(payload, boundary, walked, steps) {
                Walked::Ends(carry) if c.passed => return Some(carry),
                Walked::Ends(carry) => self.hold(LoasCandidate::passed(carry)),
                Walked::Refuted | Walked::Joined => self.provisional_failed |= c.passed,
            }
        }
        for at in 0..len.min(MAX_LOAS_RECORD_BYTES) {
            let rest = &payload[at..];
            if !opens_loas_header(rest) {
                continue;
            }
            if rest.len() < 3 {
                if let Ok(Some(carry)) = split_header_carry(rest) {
                    self.hold(LoasCandidate::pending(carry));
                }
                continue;
            }
            let first_record_end = at + 3 + record_body_len(rest);
            if first_record_end > len {
                self.hold(LoasCandidate::pending(LoasCarry::Body(
                    first_record_end - len,
                )));
            } else if let Walked::Ends(carry) = walk_unwalked(payload, at, walked, steps) {
                self.hold(LoasCandidate::passed(carry));
            }
        }
        None
    }

    /// Follow `candidate` too, unless the set is full. Two candidates that
    /// owe the same bytes have one future: they are held as one.
    fn hold(&mut self, candidate: LoasCandidate) {
        let held = &mut self.held[..self.held_len];
        if let Some(same) = held.iter_mut().find(|h| h.carry == candidate.carry) {
            same.passed |= candidate.passed;
        } else if self.held_len < LOAS_CANDIDATES {
            self.held[self.held_len] = candidate;
            self.held_len += 1;
        }
    }
}

/// [`walk_loas`] from `start`, stopping at a boundary `walked` marks and
/// marking (and adding to `steps`) each one it takes.
fn walk_unwalked(payload: &[u8], start: usize, walked: &mut [u64], steps: &mut usize) -> Walked {
    let mut pos = start;
    while let Some(rest) = payload.get(pos..).filter(|r| !r.is_empty()) {
        let (word, bit) = (pos / 64, 1u64 << (pos % 64));
        if walked[word] & bit != 0 {
            return Walked::Joined;
        }
        walked[word] |= bit;
        *steps += 1;
        if rest.len() < 3 {
            return match split_header_carry(rest) {
                Ok(carry) => Walked::Ends(carry),
                Err(_) => Walked::Refuted,
            };
        }
        if !starts_with_loas_sync(rest) {
            return Walked::Refuted;
        }
        pos += 3 + record_body_len(rest);
    }
    Walked::Ends(
        pos.checked_sub(payload.len())
            .filter(|&n| n > 0)
            .map(LoasCarry::Body),
    )
}

/// The 13-bit `audioMuxLengthBytes` of the LOAS header `header` opens.
fn record_body_len(header: &[u8]) -> usize {
    (usize::from(header[1] & 0x1F) << 8) | usize::from(header[2])
}

/// `true` when `bytes` begins with the 11-bit LOAS syncword `0x2B7`.
pub(crate) fn starts_with_loas_sync(bytes: &[u8]) -> bool {
    matches!(bytes, [b0, b1, ..]
        if *b0 == LOAS_SYNC_BYTE0 && (*b1 & LOAS_SYNC_BYTE1_MASK) == LOAS_SYNC_BYTE1_VALUE)
}

/// `true` when `bytes` can open a LOAS header: it starts with the syncword,
/// or it is the header's first byte alone (the rest is in the next PES).
pub(crate) fn opens_loas_header(bytes: &[u8]) -> bool {
    starts_with_loas_sync(bytes) || bytes == [LOAS_SYNC_BYTE0]
}

/// What the last LOAS record of a PES still owes the next PES
/// (ISO/IEC 14496-3 §1.7.2: both a record and its header may be split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoasCarry {
    /// The record continues for this many more bytes.
    Body(usize),
    /// The PES ended inside the 3-byte header; its first `len` (1 or 2)
    /// bytes, already checked against the syncword.
    Header { bytes: [u8; 2], len: u8 },
}

impl LoasCarry {
    /// Apply the carry to the next PES payload: `Body(n)` when the record
    /// owes `n` bytes of it, `Header` when the header is still incomplete,
    /// `MissingSyncword` when the completed header lacks the syncword.
    pub(crate) fn resolve(self, payload: &[u8]) -> Result<LoasCarry, LatmFramingKind> {
        let Self::Header { bytes, len } = self else {
            return Ok(self);
        };
        let have = usize::from(len);
        let take = payload.len().min(3 - have);
        let mut header = [0u8; 3];
        header[..have].copy_from_slice(&bytes[..have]);
        header[have..have + take].copy_from_slice(&payload[..take]);
        // The record's body starts in `payload` right after the header bytes.
        Ok(match walk_loas(&header[..have + take], 0)? {
            Some(Self::Body(n)) => Self::Body(take + n),
            Some(header) => header,
            None => Self::Body(take),
        })
    }
}

/// The `Header` carry for a record boundary closer than 3 bytes to the end
/// of the PES, or `MissingSyncword` when those bytes cannot open a header.
fn split_header_carry(rest: &[u8]) -> Result<Option<LoasCarry>, LatmFramingKind> {
    let prefix_ok = rest[0] == LOAS_SYNC_BYTE0
        && rest
            .get(1)
            .is_none_or(|b1| (b1 & LOAS_SYNC_BYTE1_MASK) == LOAS_SYNC_BYTE1_VALUE);
    if !prefix_ok {
        return Err(LatmFramingKind::MissingSyncword);
    }
    let mut bytes = [0u8; 2];
    bytes[..rest.len()].copy_from_slice(rest);
    Ok(Some(LoasCarry::Header {
        bytes,
        len: rest.len() as u8,
    }))
}

/// Walk the LOAS records (ISO/IEC 14496-3 §1.7.2 AudioSyncStream) of one PES
/// payload, starting at the record boundary `start`.
///
/// - `Ok(Some(carry))`: the last record, or its header, continues into the
///   next PES.
/// - `Ok(None)`: the walk ended exactly at the end of the payload: the next
///   record starts with the next PES.
/// - `Err(MissingSyncword)`: a record boundary lacks the syncword.
///
/// Every record advances at least 3 bytes, so the walk is bounded by the
/// payload length; a `Body` carry is at most one record (`3 + 0x1FFF` bytes).
pub(crate) fn walk_loas(
    payload: &[u8],
    start: usize,
) -> Result<Option<LoasCarry>, LatmFramingKind> {
    let mut pos = start;
    while let Some(rest) = payload.get(pos..).filter(|r| !r.is_empty()) {
        if rest.len() < 3 {
            return split_header_carry(rest);
        }
        if !starts_with_loas_sync(rest) {
            return Err(LatmFramingKind::MissingSyncword);
        }
        let audio_mux_length_bytes = (usize::from(rest[1] & 0x1F) << 8) | usize::from(rest[2]);
        pos += 3 + audio_mux_length_bytes;
    }
    Ok(pos
        .checked_sub(payload.len())
        .filter(|&n| n > 0)
        .map(LoasCarry::Body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_prelude::*;

    /// Helper: build a valid LOAS-framed `audioMuxElement` of `len` body
    /// bytes (zero-filled). Returns the wire bytes (3-byte header + len
    /// zero bytes).
    fn build_loas_record(len: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + usize::from(len));
        out.push(LOAS_SYNC_BYTE0);
        // top 3 bits = sync, low 5 bits = high 5 bits of length
        out.push(LOAS_SYNC_BYTE1_VALUE | ((len >> 8) as u8 & 0x1F));
        out.push((len & 0xFF) as u8);
        out.resize(3 + usize::from(len), 0);
        out
    }

    #[test]
    fn walk_loas_reports_the_bytes_a_split_record_still_owes() {
        let mut buf = build_loas_record(10);
        buf.extend_from_slice(&build_loas_record(50)[..20]);
        assert_eq!(walk_loas(&buf, 0), Ok(Some(LoasCarry::Body(33))));
    }

    #[test]
    fn walk_loas_flags_a_missing_sync_at_a_record_boundary() {
        let mut buf = build_loas_record(10);
        buf.extend_from_slice(&[0x00, 0x11, 0x22, 0x33]);
        assert_eq!(walk_loas(&buf, 0), Err(LatmFramingKind::MissingSyncword));
    }

    #[test]
    fn walk_loas_ending_on_a_boundary_carries_nothing() {
        let buf = build_loas_record(10);
        assert_eq!(walk_loas(&buf, 0), Ok(None));
    }

    #[test]
    fn walk_loas_carries_a_split_header_and_resolves_it_on_the_next_pes() {
        let record = build_loas_record(50);
        let mut buf = build_loas_record(10);
        buf.push(record[0]);
        let carry = walk_loas(&buf, 0).unwrap().unwrap();
        assert_eq!(
            carry,
            LoasCarry::Header {
                bytes: [0x56, 0],
                len: 1
            }
        );
        // One more header byte still leaves the header split.
        let carry = carry.resolve(&record[1..2]).unwrap();
        assert!(
            matches!(carry, LoasCarry::Header { len: 2, .. }),
            "{carry:?}"
        );
        // The last header byte, then 50 body bytes owed from that PES.
        assert_eq!(carry.resolve(&record[2..23]), Ok(LoasCarry::Body(51)));
        assert_eq!(LoasCarry::Body(7).resolve(&[0; 3]), Ok(LoasCarry::Body(7)));
    }

    #[test]
    fn walk_loas_flags_a_split_header_without_sync() {
        let mut buf = build_loas_record(10);
        buf.extend_from_slice(&[0x56, 0x00]);
        assert_eq!(walk_loas(&buf, 0), Err(LatmFramingKind::MissingSyncword));
        let carry = LoasCarry::Header {
            bytes: [0x56, 0xE0],
            len: 2,
        };
        assert_eq!(carry.resolve(&[0x05]), Ok(LoasCarry::Body(6)));
        let carry = LoasCarry::Header {
            bytes: [0x56, 0],
            len: 1,
        };
        assert_eq!(
            carry.resolve(&[0x00, 0x05]),
            Err(LatmFramingKind::MissingSyncword)
        );
    }

    /// What `search` holds, as `(carry, passed)`.
    fn held(search: &LoasSearch) -> Vec<(LoasCarry, bool)> {
        search.held[..search.held_len]
            .iter()
            .map(|c| (c.carry, c.passed))
            .collect()
    }

    /// A fresh search fed `payload` as one PES: what it settled and holds.
    fn search_one(payload: &[u8]) -> (LoasSettle, Vec<(LoasCarry, bool)>) {
        let mut search = LoasSearch::default();
        let settled = search.feed(payload);
        (settled, held(&search))
    }

    /// `pes`, fed in turn to a fresh `LoasFraming` (no PES aligned): the
    /// issues raised and the state after the last.
    fn frame_all(pes: &[&[u8]]) -> (Vec<LatmFramingKind>, LoasFraming) {
        let mut framing = LoasFraming::default();
        let issues = pes.iter().filter_map(|p| framing.feed(p, false)).collect();
        (issues, framing)
    }

    /// `records` records of `len` body bytes back to back.
    fn loas_stream(len: u16, records: usize) -> Vec<u8> {
        (0..records).flat_map(|_| build_loas_record(len)).collect()
    }

    #[test]
    fn loas_search_looks_only_within_one_record() {
        let mut buf = vec![0x00; MAX_LOAS_RECORD_BYTES - 1];
        buf.extend_from_slice(&build_loas_record(0));
        assert_eq!(
            search_one(&buf),
            (LoasSettle::Unsynced, vec![(LoasCarry::Body(0), true)])
        );
        // Nothing in the window: more than one record length without a
        // candidate is not LOAS.
        buf.insert(0, 0x00);
        assert_eq!(search_one(&buf), (LoasSettle::NotLoas, vec![]));
    }

    #[test]
    fn loas_search_passes_a_candidate_only_when_a_whole_record_follows() {
        let mut buf = vec![0x00; 4];
        buf.extend_from_slice(&build_loas_record(8));
        assert_eq!(search_one(&buf).1, [(LoasCarry::Body(0), true)]);
        buf.extend_from_slice(&build_loas_record(50)[..20]);
        assert_eq!(search_one(&buf).1, [(LoasCarry::Body(33), true)]);
        // A pair whose length (0xFF) runs past the payload: 4 + 3 + 0xFF =
        // 262 against a 19-byte payload, held pending; the scan goes on to
        // the whole record after it.
        let mut tail = vec![0x00; 4];
        tail.extend_from_slice(&[0x56, 0xE0, 0xFF, 0x00]);
        tail.extend_from_slice(&build_loas_record(8));
        assert_eq!(
            search_one(&tail).1,
            [
                (LoasCarry::Body(262 - tail.len()), false),
                (LoasCarry::Body(0), true)
            ]
        );
        // A candidate in the last two bytes: its header continues.
        assert_eq!(
            search_one(&[0x00, 0x56, 0xE0]).1,
            [(
                LoasCarry::Header {
                    bytes: [0x56, 0xE0],
                    len: 2
                },
                false
            )]
        );
        assert_eq!(
            search_one(&[0x00, 0x00, 0x56]).1,
            [(
                LoasCarry::Header {
                    bytes: [0x56, 0x00],
                    len: 1
                },
                false
            )]
        );
        assert_eq!(search_one(&[0x00; 64]).1, []);
        // The first record fits but no syncword follows it: refuted.
        let mut refuted = vec![0x00; 4];
        refuted.extend_from_slice(&[0x56, 0xE0, 0x02, 0x00, 0x00, 0x11, 0x22, 0x33]);
        assert_eq!(search_one(&refuted).1, []);
    }

    /// A refuted candidate in a record's tail does not end the search: the
    /// first real record later in the window is found.
    #[test]
    fn loas_search_searches_past_a_refuted_candidate() {
        // A 100-byte tail whose false pair at offset 4 declares a 2-byte
        // record ending at 9, where no syncword follows; then nine records.
        let mut buf = vec![0x00; 100];
        buf[4..7].copy_from_slice(&[0x56, 0xE0, 0x02]);
        for _ in 0..9 {
            buf.extend_from_slice(&build_loas_record(1000));
        }
        assert_eq!(buf.len(), 9127);
        assert_eq!(search_one(&buf).1, [(LoasCarry::Body(0), true)]);
        // Past a refuted candidate, a later one whose record runs past the
        // payload is held for the next PES.
        buf.truncate(100);
        buf.extend_from_slice(&build_loas_record(1000)[..500]);
        assert_eq!(search_one(&buf).1, [(LoasCarry::Body(503), false)]);
    }

    /// A 64 KiB chain of empty records ending in junk offers a candidate
    /// every third byte of the window, each one refuted at the end of the
    /// same chain: the walks share the boundaries already taken, so the
    /// chain is walked once, not once per candidate.
    #[test]
    fn loas_search_walks_a_refuted_chain_once() {
        let mut buf = Vec::new();
        while buf.len() + 3 <= 65_532 {
            buf.extend_from_slice(&[0x56, 0xE0, 0x00]);
        }
        buf.resize(65_536, 0x00);
        let mut search = LoasSearch::default();
        let mut steps = 0;
        assert_eq!(search.feed_counting(&buf, &mut steps), LoasSettle::NotLoas);
        assert_eq!(held(&search), []);
        // The 21 844 boundaries of the chain and the one that refutes it; a
        // walk per candidate would be about 2 700 times that.
        assert_eq!(steps, 21_845);
    }

    /// Candidates are still searched only in the first
    /// `MAX_LOAS_RECORD_BYTES` bytes, however many are refuted on the way.
    #[test]
    fn loas_search_keeps_its_window_past_refuted_candidates() {
        let mut buf = Vec::new();
        while buf.len() + 4 <= MAX_LOAS_RECORD_BYTES {
            // A 0-length record at each candidate, then a non-syncword byte.
            buf.extend_from_slice(&[0x56, 0xE0, 0x00, 0x00]);
        }
        buf.resize(MAX_LOAS_RECORD_BYTES, 0x00);
        buf.extend_from_slice(&build_loas_record(8));
        assert_eq!(search_one(&buf).1, []);
    }

    /// A header split 1 + 2 and 2 + 1 over two PES, on a stream joined
    /// inside a record: found, passed and confirmed, with no issue.
    #[test]
    fn a_header_split_over_two_pes_is_followed_either_way() {
        let stream = loas_stream(40, 10);
        for header_bytes_in_first in [1, 2] {
            let cut = 43 + header_bytes_in_first;
            let rest = &stream[cut..];
            let (issues, framing) = frame_all(&[&stream[10..cut], &rest[..60], &rest[60..120]]);
            assert_eq!(issues, [], "{header_bytes_in_first}");
            assert!(framing.position.is_some() && framing.search.is_none());
        }
    }

    /// A record that ends exactly where its PES ends passes, and its next
    /// boundary is the next PES's first byte.
    #[test]
    fn a_record_ending_exactly_at_a_pes_end_is_tested_at_the_next_pes_start() {
        let first = zeros_then_record(10, 20);
        assert_eq!(search_one(&first).1, [(LoasCarry::Body(0), true)]);
        let mut second = build_loas_record(30);
        second.extend_from_slice(&build_loas_record(30)[..10]);
        let (issues, framing) = frame_all(&[&first, &second]);
        assert_eq!(issues, []);
        assert_eq!(framing.position, Some(LoasCarry::Body(23)));
        let (issues, framing) = frame_all(&[&first, &[0x00; 40]]);
        assert_eq!(issues, []);
        assert!(framing.position.is_none());
    }

    /// PES of one, two and three bytes: headers split every way, still found.
    #[test]
    fn pes_of_one_two_and_three_bytes_find_the_records() {
        let stream = loas_stream(20, 50);
        for pes_len in [1, 2, 3] {
            let pes: Vec<&[u8]> = stream[5..].chunks(pes_len).collect();
            let (issues, framing) = frame_all(&pes);
            assert_eq!(issues, [], "{pes_len}");
            assert!(framing.search.is_none(), "{pes_len}: confirmed");
        }
    }

    /// A join exactly at a record boundary: the record there is a candidate
    /// like any other, confirmed at its second boundary.
    #[test]
    fn a_join_at_a_record_boundary_is_confirmed_at_its_second_boundary() {
        let stream = loas_stream(20, 10);
        let mut framing = LoasFraming::default();
        assert_eq!(framing.feed(&stream[..30], false), None);
        assert!(framing.position.is_none(), "one boundary is provisional");
        assert_eq!(framing.feed(&stream[30..60], false), None);
        assert_eq!(framing.position, Some(LoasCarry::Body(9)));
        assert!(framing.search.is_none());
    }

    /// The set holds at most `LOAS_CANDIDATES`; a newcomer to a full set is
    /// ignored, and a candidate refuted at its boundary frees its slot.
    #[test]
    fn a_refuted_candidate_frees_its_slot() {
        // 18 candidates whose records run 8 194 bytes from their header.
        let mut first = vec![0x00; 100];
        for at in (0..90).step_by(5) {
            first[at..at + 3].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        }
        let mut search = LoasSearch::default();
        assert_eq!(search.feed(&first), LoasSettle::Unsynced);
        let carries: Vec<_> = held(&search).iter().map(|h| h.0).collect();
        let want: Vec<_> = (0..80)
            .step_by(5)
            .map(|at| LoasCarry::Body(at + MAX_LOAS_RECORD_BYTES - 100))
            .collect();
        assert_eq!(carries, want, "the first 16 are held, the last 2 ignored");
        // The next PES refutes all 16 where their records end, and offers
        // one more past them.
        let mut second = vec![0x00; MAX_LOAS_RECORD_BYTES];
        second[8180..8183].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        assert_eq!(search.feed(&second), LoasSettle::Unsynced);
        assert_eq!(
            held(&search),
            [(
                LoasCarry::Body(8180 + 3 + 0x1FFF - MAX_LOAS_RECORD_BYTES),
                false
            )]
        );
    }

    /// A confirmation ends the search: the set and the count go with it.
    #[test]
    fn a_confirmation_clears_the_set_and_zeroes_the_count() {
        let mut busy = vec![0x00; 400];
        busy[40..43].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        busy[300..].copy_from_slice(&loas_stream(97, 1));
        let mut framing = LoasFraming::default();
        assert_eq!(framing.feed(&[0x00; 8000], false), None);
        assert_eq!(framing.feed(&busy, false), None);
        let search = framing.search.as_ref().expect("searching");
        assert_eq!(search.unsynced_bytes, 8400);
        assert_eq!(search.held_len, 2);
        // The record at 300 ended on the PES's end: its next boundary is
        // here, and so is the one after.
        assert_eq!(framing.feed(&loas_stream(10, 3), false), None);
        assert!(framing.search.is_none());
        // The confirming walk ended on the PES's end, so the next PES opens
        // on a known boundary: one without a syncword is reported at once,
        // and the unsynced stretch it starts is counted from that PES.
        assert_eq!(
            framing.feed(&[0x00; 1000], false),
            Some(LatmFramingKind::MissingSyncword)
        );
        for _ in 0..7 {
            assert_eq!(framing.feed(&[0x00; 1000], false), None);
        }
        assert_eq!(
            framing.search.as_ref().map(|s| s.unsynced_bytes),
            Some(8000)
        );
    }

    /// A loss drops the position, the candidates and the count.
    #[test]
    fn a_loss_drops_the_pending_candidates() {
        let mut busy = vec![0x00; 100];
        busy[40..43].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        let mut framing = LoasFraming::default();
        assert_eq!(framing.feed(&busy, false), None);
        assert_eq!(framing.search.as_ref().map(|s| s.held_len), Some(1));
        framing.forget();
        assert!(framing.is_clear());
    }

    /// Bodies that keep the set full of long-lived false candidates (abuse):
    /// one issue for the stretch, the set never over `LOAS_CANDIDATES`, and
    /// no PES walks more boundaries than it has bytes.
    #[test]
    fn a_full_candidate_set_costs_at_most_one_issue_per_stretch() {
        // Twenty candidates a PES whose records run past it, then in every
        // fourth PES a run of empty records ending in junk: dense walks.
        let mut full = vec![0x00; 100];
        for at in (0..100).step_by(5) {
            full[at..at + 3].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        }
        let mut chains = Vec::new();
        while chains.len() < 96 {
            chains.extend_from_slice(&[0x56, 0xE0, 0x00]);
        }
        chains.resize(100, 0x11);
        let mut search = LoasSearch::default();
        let mut issues = 0;
        for i in 0..1000 {
            let pes = if i % 4 == 3 { &chains } else { &full };
            let mut steps = 0;
            if search.feed_counting(pes, &mut steps) == LoasSettle::NotLoas {
                issues += 1;
            }
            assert!(steps <= pes.len(), "PES {i}: {steps} steps");
            assert!(search.held_len <= LOAS_CANDIDATES);
        }
        assert_eq!(issues, 1);
    }

    /// A PES longer than the hard bound that holds whole records after a
    /// join: its candidate passes there and is provisional, so the bytes it
    /// adds are not judged, and the next PES confirms it.
    #[test]
    fn a_long_pes_of_whole_records_is_not_judged_while_its_candidate_is_provisional() {
        let mut long = vec![0x00; 100];
        while long.len() < LOAS_UNSYNCED_BOUND + 1_000 {
            long.extend_from_slice(&build_loas_record(997));
        }
        long.extend_from_slice(&build_loas_record(997)[..500]);
        let mut framing = LoasFraming::default();
        assert_eq!(framing.feed(&long, false), None);
        assert!(framing.position.is_none(), "provisional");
        let mut next = build_loas_record(997)[500..].to_vec();
        next.extend_from_slice(&build_loas_record(997)[..100]);
        assert_eq!(framing.feed(&next, false), None);
        assert_eq!(framing.position, Some(LoasCarry::Body(900)));
    }

    /// Only a provisional position that fails ends the deferral: the same
    /// long PES after a PES whose candidate it refutes (a byte of a record
    /// body where that candidate's record would end) is still not judged.
    #[test]
    fn a_refuted_candidate_does_not_end_the_deferral_of_a_provisional_position() {
        let mut first = vec![0x00; 100];
        first[..3].copy_from_slice(&[0x56, 0xFF, 0xFF]);
        let mut long = vec![0x00; 100];
        while long.len() < LOAS_UNSYNCED_BOUND + 1_000 {
            long.extend_from_slice(&build_loas_record(997));
        }
        long.extend_from_slice(&build_loas_record(997)[..500]);
        let mut framing = LoasFraming::default();
        assert_eq!(framing.feed(&first, false), None);
        assert_eq!(framing.feed(&long, false), None);
        let search = framing.search.as_ref().expect("searching");
        assert_eq!(held(search), [(LoasCarry::Body(500), true)]);
        assert!(!search.provisional_failed);
    }

    /// A pending candidate whose walk joins a provisional position's is
    /// that position's duplicate: the position is walked first and confirmed,
    /// and the duplicate does not take its place as a once-passed candidate.
    #[test]
    fn a_candidate_that_joins_a_provisional_position_does_not_take_its_place() {
        let mut search = LoasSearch::default();
        search.held[0] = LoasCandidate::pending(LoasCarry::Body(20));
        search.held[1] = LoasCandidate::passed(Some(LoasCarry::Body(50)));
        search.held_len = 2;
        // 20: a record to 50; 50: a record to 100; 100: one past the PES.
        let mut pes = vec![0x00; 20];
        pes.extend_from_slice(&build_loas_record(27));
        pes.extend_from_slice(&build_loas_record(47));
        pes.extend_from_slice(&build_loas_record(97)[..40]);
        assert_eq!(
            search.feed(&pes),
            LoasSettle::Confirmed(Some(LoasCarry::Body(60)))
        );
    }

    /// Between PES a search holds its candidates and its counters, nothing
    /// that grows with the PES it saw: a stream declaring many AAC-LATM PIDs
    /// that never sync holds the candidates of each and no more.
    #[test]
    fn a_search_holds_only_its_candidates_and_counters_between_pes() {
        let words = 3 * core::mem::size_of::<usize>();
        assert!(
            core::mem::size_of::<LoasSearch>()
                <= core::mem::size_of::<[LoasCandidate; LOAS_CANDIDATES]>() + words
        );
    }

    /// `zeros` zero bytes, then one record of `len` body bytes.
    fn zeros_then_record(zeros: usize, len: u16) -> Vec<u8> {
        let mut out = vec![0x00; zeros];
        out.extend_from_slice(&build_loas_record(len));
        out
    }

    #[test]
    fn opens_loas_header_accepts_a_syncword_or_a_lone_first_byte() {
        assert!(opens_loas_header(&[0x56, 0xE0, 0x05]));
        assert!(opens_loas_header(&[0x56]));
        assert!(!opens_loas_header(&[0x56, 0x00]));
        assert!(!opens_loas_header(&[0x00]));
        assert!(!opens_loas_header(&[]));
    }

    #[test]
    fn walk_loas_starts_after_the_carried_bytes() {
        let mut buf = vec![0xAA; 5];
        buf.extend_from_slice(&build_loas_record(8));
        assert_eq!(walk_loas(&buf, 5), Ok(None));
        assert_eq!(walk_loas(&buf, 0), Err(LatmFramingKind::MissingSyncword));
    }

    #[test]
    fn valid_loas_sync_returns_length() {
        let buf = build_loas_record(100);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 100);
    }

    #[test]
    fn empty_payload_yields_truncated() {
        assert_eq!(validate_latm_sync(&[]), Err(LatmFramingKind::Truncated));
    }

    #[test]
    fn two_byte_payload_yields_truncated() {
        // Need 3 bytes minimum to read sync + length.
        assert_eq!(
            validate_latm_sync(&[0x56, 0xE0]),
            Err(LatmFramingKind::Truncated)
        );
    }

    /// Primary lenient-mode test: PES that does not begin with the
    /// LOAS syncword on a `stream_type=0x11` PID is non-conformant.
    #[test]
    fn missing_syncword_returns_missing_syncword() {
        // Plausible ADTS sync (0xFFF) — common confusion: ADTS-framed
        // AAC mistakenly shipped on a LATM-advertising PID.
        let bytes = [0xFF, 0xF1, 0x4C, 0x80, 0x00, 0x00];
        assert_eq!(
            validate_latm_sync(&bytes),
            Err(LatmFramingKind::MissingSyncword)
        );
    }

    #[test]
    fn wrong_byte1_top_bits_yields_missing_syncword() {
        // Byte 0 matches but byte 1 has top 3 bits != 0b111.
        let bytes = [0x56, 0x40, 0x00, 0x00, 0x00];
        assert_eq!(
            validate_latm_sync(&bytes),
            Err(LatmFramingKind::MissingSyncword)
        );
    }

    #[test]
    fn audio_mux_length_overrun_returns_overrun() {
        // Declare 200 bytes but only ship 50 in the buffer.
        let mut buf = build_loas_record(50);
        // Patch the length to 200 (overflow vs 50-byte body).
        buf[1] = LOAS_SYNC_BYTE1_VALUE | ((200u16 >> 8) as u8);
        buf[2] = (200u16 & 0xFF) as u8;
        assert_eq!(
            validate_latm_sync(&buf),
            Err(LatmFramingKind::AudioMuxLengthOverrun)
        );
    }

    #[test]
    fn zero_length_record_is_valid() {
        // Edge case: audioMuxLengthBytes == 0. Spec doesn't forbid this
        // (a zero-byte audioMuxElement is unusual but parseable as a
        // structural matter); the validator only checks sync + length-fits.
        let buf = build_loas_record(0);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 0);
    }

    #[test]
    fn max_length_record_is_valid() {
        // 13-bit length field: 0x1FFF = 8191.
        let buf = build_loas_record(8191);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 8191);
    }
}
