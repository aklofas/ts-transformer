//! Per-PID PES reassembly.
//!
//! Drive with `Reassembler::push(pid, payload, pusi)` for each TS packet.
//! Emits a complete `PesPayload` whenever a PES boundary is detected
//! (next-PUSI on same PID, or `PES_packet_length`-driven end). Enforces
//! per-PID and aggregate caps; breaches surface as `Overflow` events.

use crate::error::DemuxError;
use crate::mpegts::demux::event::PesHeaderMalformedKind;
use alloc::vec::Vec;
use hashbrown::HashMap;

/// One reassembled PES on a single PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PesPayload {
    pub pid: u16,
    pub stream_id: u8,
    /// 90 kHz PTS, if the PES carried one. Typed
    /// [`Pts90khz`](crate::mpegts::common::Pts90khz) per the public-boundary
    /// policy (PTS surfaces as `int64_t` across the C ABI).
    pub pts: Option<crate::mpegts::common::Pts90khz>,
    /// 90 kHz DTS, if the PES carried one. Typed
    /// [`Pts90khz`](crate::mpegts::common::Pts90khz) per the public-boundary
    /// policy (DTS surfaces as `int64_t` across the C ABI).
    pub dts: Option<crate::mpegts::common::Pts90khz>,
    /// Adaptation-field `random_access_indicator` captured from the
    /// PES_start packet (PUSI=1). First-packet-wins: continuation
    /// packets don't overwrite the latched value, matching how
    /// encoders/muxers signal RA points (ffmpeg/tsduck convention).
    pub random_access_indicator: bool,
    /// PES flags1 bit 2 — `data_alignment_indicator`. Required for DVB
    /// subtitle (EN 300 743 §6.2), DVB teletext (EN 300 472 §4.2), AC-3
    /// (ATSC A/52:2018 §A.6.3), AV1 (binding §3.4), metadata streams
    /// (H.222.0 V9 §2.12.4.1). For codecs that don't require it the bit
    /// is informational. `false` when the PES has no optional header.
    pub data_alignment_indicator: bool,
    /// PES header structural issues detected during parsing.
    /// These are best-effort observations: the
    /// dispatcher decides whether to escalate via `queue_nonconformant`.
    /// Empty for conformant PESes.
    pub header_issues: Vec<PesHeaderMalformedKind>,
    /// Elementary stream payload bytes (after the PES header, including
    /// header_data_length adjustment).
    pub payload: Vec<u8>,
}

/// Internally buffered partial PES on a PID.
#[derive(Debug)]
struct Partial {
    declared_total_len: Option<usize>, // PES_packet_length-derived total of the body
    buf: Vec<u8>,
    /// Latched at PUSI=1; never overwritten by continuation packets.
    random_access_indicator: bool,
    /// Why the buffered bytes are known to be incomplete; the first damage
    /// recorded wins.
    damage: Option<&'static str>,
}

/// Per-call output from `Reassembler::push`. A single TS payload chunk
/// can complete the previous PES *and* begin a new one, so a vec of
/// outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyOutcome {
    /// A complete PES is now available.
    Complete(PesPayload),
    /// Buffer cap exceeded for this PID; partial PES dropped.
    Overflow { pid: u16 },
    /// Aggregate buffer cap exceeded; all partial PES on all PIDs dropped.
    OverflowTotal,
    /// A zero `PES_packet_length` (unbounded) arrived on a stream the PMT
    /// classifies as non-video. H.222.0 §2.4.3.7 permits zero only when the
    /// payload is a video elementary stream; the reassembler drops the
    /// partial (does not flush a bogus sample) and surfaces this so the
    /// demuxer can emit a NonConformant. `stream_id` is the observed PES
    /// stream_id, carried for diagnostics.
    ZeroLengthNonVideo { pid: u16, stream_id: u8 },
    /// A PES on this PID could not be parsed (the payload of
    /// `DemuxError::MalformedPes`). Surfaced as an outcome, IN WIRE ORDER
    /// beside the completions of the same `push`, so the caller sees it
    /// without losing a valid PES that completed in the same call. The
    /// demuxer turns it into a `NonConformant` event in
    /// lenient mode and into the error in strict mode.
    ///
    /// With a damage reason (a continuity gap inside the PES, or a bounded
    /// PES that ended short of `PES_packet_length`) it is immediately
    /// followed by the `Complete` it describes, so the caller can mark or
    /// drop that payload.
    Malformed { pid: u16, reason: &'static str },
}

/// Damage reasons a [`ReassemblyOutcome::Malformed`] carries ahead of the
/// `Complete` whose payload is missing bytes.
pub(crate) const DAMAGE_CC_GAP: &str =
    "continuity_counter gap inside the PES; payload bytes are missing";
pub(crate) const DAMAGE_SHORT_PES: &str = "PES ended before PES_packet_length bytes arrived";

pub(crate) fn is_damage_reason(reason: &str) -> bool {
    reason == DAMAGE_CC_GAP || reason == DAMAGE_SHORT_PES
}

/// `parse_complete` constructs only `MalformedPes`; keep the PID and a fixed
/// reason rather than dropping the outcome if that ever changes.
fn malformed(pid: u16, e: DemuxError) -> ReassemblyOutcome {
    match e {
        DemuxError::MalformedPes { pid, reason } => ReassemblyOutcome::Malformed { pid, reason },
        _ => ReassemblyOutcome::Malformed {
            pid,
            reason: "unparseable PES",
        },
    }
}

#[derive(Debug)]
pub struct Reassembler {
    by_pid: HashMap<u16, Partial>,
    /// PIDs whose partials the last aggregate-cap overflow dropped.
    dropped_on_total: Vec<u16>,
    total_buffered: usize,
    cap_per_pid: usize,
    cap_total: usize,
}

impl Reassembler {
    pub fn new(cap_per_pid: usize, cap_total: usize) -> Self {
        Self {
            by_pid: HashMap::new(),
            dropped_on_total: Vec::new(),
            total_buffered: 0,
            cap_per_pid,
            cap_total,
        }
    }

    /// Feed one TS-packet's payload bytes for `pid`. `pusi=true` means
    /// this packet begins a new PES on this PID. A PES that fails to parse
    /// is reported as `ReassemblyOutcome::Malformed` in wire order; `push`
    /// itself never fails.
    ///
    /// `random_access_indicator` is sourced from the TS adaptation-field
    /// RAI bit (ISO/IEC 13818-1 §2.4.3.4). Only the value carried on the
    /// PES_start packet (PUSI=1) is latched onto the in-flight PES;
    /// continuation packets' RAI bits are ignored — encoders/muxers signal
    /// AU-level RA on the start packet only (matches ffmpeg/tsduck).
    ///
    /// `is_video` reflects whether the PMT-declared stream type of `pid` is a
    /// video elementary stream — both the codecs tst-core parses
    /// ([`StreamKind::Video`](crate::mpegts::demux::StreamKind::Video)) and
    /// video codecs it does not (MPEG-1/2/4, classified
    /// [`StreamKind::Unknown`](crate::mpegts::demux::StreamKind::Unknown) but a
    /// video `stream_type`). It gates the zero-`PES_packet_length` rule
    /// (H.222.0 §2.4.3.7): an unbounded PES is legal only when the
    /// payload is a video elementary stream. Callers without a PMT entry for
    /// `pid` pass `false` (conservatively flag).
    pub fn push(
        &mut self,
        pid: u16,
        payload: &[u8],
        pusi: bool,
        random_access_indicator: bool,
        is_video: bool,
    ) -> Vec<ReassemblyOutcome> {
        let mut out = Vec::new();
        if pusi {
            // PUSI: drain whatever was in flight on this PID first.
            let prev = self.by_pid.remove(&pid);
            // Start the fresh partial up-front so the new PES's payload
            // (appended below) is captured even if `parse_complete` on
            // the prior buffer errors.
            self.by_pid.insert(
                pid,
                Partial {
                    declared_total_len: None,
                    buf: Vec::new(),
                    random_access_indicator,
                    damage: None,
                },
            );
            if let Some(prev) = prev {
                self.total_buffered = self.total_buffered.saturating_sub(prev.buf.len());
                self.finish(pid, prev, &mut out);
            }
        }
        // Append payload to whatever partial exists for this PID.
        let part = match self.by_pid.get_mut(&pid) {
            Some(p) => p,
            None => return out, // bytes before we ever saw a PUSI; drop.
        };
        if part.buf.len() + payload.len() > self.cap_per_pid {
            // Cap-per-PID exceeded. Drop, surface, resume from next PUSI on this PID.
            self.total_buffered = self.total_buffered.saturating_sub(part.buf.len());
            self.by_pid.remove(&pid);
            out.push(ReassemblyOutcome::Overflow { pid });
            return out;
        }
        part.buf.extend_from_slice(payload);
        self.total_buffered += payload.len();
        // Try to derive declared_total_len if still unknown.
        if part.declared_total_len.is_none() && part.buf.len() >= 6 {
            let stream_id = part.buf[3];
            let pkt_len_field = u16::from_be_bytes([part.buf[4], part.buf[5]]);
            if pkt_len_field != 0 {
                // PES_packet_length is the count of bytes after this 6-byte header.
                part.declared_total_len = Some(6 + pkt_len_field as usize);
            } else if !is_video {
                // H.222.0 §2.4.3.7: a zero PES_packet_length (unbounded) is
                // permitted ONLY when the payload is a video elementary
                // stream. The decision is keyed on the PMT-declared
                // `StreamKind` (passed as `is_video`), NOT on `stream_id`:
                // AV1 *video* in MPEG-2-TS-binding carriage rides
                // `stream_id = 0xBD` (private_stream_1) yet is classified
                // `StreamKind::Video(Av1)`, so it must pass un-flagged; while
                // AC-3 audio, DVB subtitle, async-KLV, and private-data PES
                // (which also use 0xBD) are non-video and must be caught.
                // On a non-video stream a zero length would buffer until the
                // next PUSI / cap and flush a bogus sample — drop the partial
                // and surface the violation. `stream_id` is kept
                // purely as the diagnostic payload on the event.
                self.total_buffered = self.total_buffered.saturating_sub(part.buf.len());
                self.by_pid.remove(&pid);
                out.push(ReassemblyOutcome::ZeroLengthNonVideo { pid, stream_id });
                return out;
            }
        }
        // Aggregate cap check.
        if self.total_buffered > self.cap_total {
            let mut dropped: Vec<u16> = self.by_pid.keys().copied().collect();
            dropped.sort_unstable();
            self.dropped_on_total = dropped;
            self.by_pid.clear();
            self.total_buffered = 0;
            out.push(ReassemblyOutcome::OverflowTotal);
            return out;
        }
        // Length-driven completion.
        //
        // Per ITU-T H.222.0 V9 §2.4.3.7, `PES_packet_length` (when non-zero)
        // is the byte count after the 6-byte fixed prefix, so the total PES
        // length on the wire is `6 + PES_packet_length`. When the buffer
        // holds more bytes than `total`, those extra bytes belong to the
        // *next* PES on this PID or are stray trailing bytes (most PIDs use
        // PUSI for boundaries, so reaching this branch with residual is the
        // off-nominal path). The emitted sample MUST be exactly the first
        // `total` bytes; including the trailing bytes here would both
        // corrupt the current sample and silently consume the start of the
        // next one.
        //
        // Residual disposition (deliberate): we discard `part.buf[total..]` and
        // remove the per-PID state. Recovering the residual into a fresh
        // `Partial` would require a PUSI signal AND a fresh adaptation-field
        // RAI value, neither of which is available at this completion site
        // — the next PUSI on this PID re-initializes cleanly. The next
        // PES start, if it landed in the residual, is reacquired then.
        if part
            .declared_total_len
            .is_some_and(|total| part.buf.len() >= total)
        {
            // Release the body and the discarded tail together.
            self.total_buffered = self.total_buffered.saturating_sub(part.buf.len());
            if let Some(mut done) = self.by_pid.remove(&pid) {
                if let Some(total) = done.declared_total_len {
                    done.buf.truncate(total);
                }
                self.finish(pid, done, &mut out);
            }
        }
        out
    }

    /// Turn a closed partial into outcomes: a damage `Malformed` (if any)
    /// immediately before its `Complete`, or the parse error alone.
    fn finish(&mut self, pid: u16, mut part: Partial, out: &mut Vec<ReassemblyOutcome>) {
        // A bounded PES closed (by the next PUSI or a flush) before
        // `PES_packet_length` bytes arrived is missing its tail.
        if part.damage.is_none()
            && part
                .declared_total_len
                .is_some_and(|total| part.buf.len() < total)
        {
            part.damage = Some(DAMAGE_SHORT_PES);
        }
        match parse_complete(pid, &part.buf, part.random_access_indicator) {
            Ok(Some(pes)) => {
                if let Some(reason) = part.damage {
                    out.push(ReassemblyOutcome::Malformed { pid, reason });
                }
                out.push(ReassemblyOutcome::Complete(pes));
            }
            Ok(None) => {}
            Err(e) => out.push(malformed(pid, e)),
        }
    }

    /// Release every partial PES (end of stream) and return the payloads
    /// that parse, without damage markers or parse errors.
    pub fn drain_partial(&mut self) -> Vec<PesPayload> {
        self.drain_partial_outcomes()
            .into_iter()
            .filter_map(|o| match o {
                ReassemblyOutcome::Complete(pes) => Some(pes),
                _ => None,
            })
            .collect()
    }

    /// Release every partial PES (end of stream) as outcomes: each damaged
    /// payload is preceded by its `Malformed` damage marker, and a payload
    /// that fails to parse is reported as `Malformed` alone.
    pub fn drain_partial_outcomes(&mut self) -> Vec<ReassemblyOutcome> {
        // PID order: the flush order is part of the observable event order,
        // so it must not depend on the hash map's per-process seed.
        let mut partials: Vec<(u16, Partial)> =
            core::mem::take(&mut self.by_pid).into_iter().collect();
        partials.sort_unstable_by_key(|(pid, _)| *pid);
        self.total_buffered = 0;
        let mut out = Vec::new();
        for (pid, p) in partials {
            self.finish(pid, p, &mut out);
        }
        out
    }

    /// Record that payload bytes of the PES in flight on `pid` were lost
    /// (H.222.0 §2.4.3.3: a continuity_counter gap). The PES still completes,
    /// preceded by a `Malformed` carrying the damage reason. The first damage
    /// recorded wins; with no PES in flight this does nothing.
    pub fn mark_damaged(&mut self, pid: u16) {
        if let Some(part) = self.by_pid.get_mut(&pid)
            && part.damage.is_none()
        {
            part.damage = Some(DAMAGE_CC_GAP);
        }
    }

    /// The PIDs, ascending, whose partials the last
    /// `ReassemblyOutcome::OverflowTotal` dropped; empty once taken.
    pub fn take_total_overflow_pids(&mut self) -> Vec<u16> {
        core::mem::take(&mut self.dropped_on_total)
    }

    /// Drop any partial PES state buffered for `pid` without emitting it.
    /// Used by the demuxer when PAT removes a program — per-PID reassembly
    /// state for that program's PIDs is no longer reachable (no PSI binding
    /// connects the PID to a stream), so leaving the buffer in place is a
    /// bounded leak under PAT rotation.
    pub fn remove_pid(&mut self, pid: u16) {
        if let Some(p) = self.by_pid.remove(&pid) {
            self.total_buffered = self.total_buffered.saturating_sub(p.buf.len());
        }
    }

    /// Test probe: does `pid` hold a partial PES?
    #[cfg(test)]
    pub(crate) fn has_pid(&self, pid: u16) -> bool {
        self.by_pid.contains_key(&pid)
    }

    pub fn buffered_bytes(&self) -> usize {
        self.total_buffered
    }
}

/// Parse a fully-buffered PES packet (header + body) into a `PesPayload`.
/// Returns `None` if the buffer is too short to be a valid PES.
///
/// Performs structural validation on the PES header:
/// `flags1` marker bits, `PTS_DTS_flags` (forbidden 0b01), PTS/DTS
/// 4-bit prefixes, and PTS/DTS 5-byte trailing marker bits. Issues are
/// collected onto `PesPayload::header_issues` rather than thrown as
/// fatal errors — the dispatcher in `pes_emit.rs` routes them through
/// the strict-mode cascade.
fn parse_complete(
    pid: u16,
    buf: &[u8],
    random_access_indicator: bool,
) -> Result<Option<PesPayload>, DemuxError> {
    if buf.len() < 6 {
        return Ok(None);
    }
    if buf[0] != 0x00 || buf[1] != 0x00 || buf[2] != 0x01 {
        return Err(DemuxError::MalformedPes {
            pid,
            reason: "missing 0x000001 PES start code prefix",
        });
    }
    let stream_id = buf[3];
    // H.222.0 Table 2-21: every stream_id from 0xBD to 0xFE carries the
    // optional PES header except padding (0xBE), private_stream_2 (0xBF),
    // ECM (0xF0), EMM (0xF1), DSMCC (0xF2) and H.222.1 type E (0xF8).
    // program_stream_map (0xBC) and program_stream_directory (0xFF) fall
    // outside the range.
    let has_optional_header = (0xBD..=0xFE).contains(&stream_id)
        && !matches!(stream_id, 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8);
    let mut body_off = 6;
    let mut pts = None;
    let mut dts = None;
    let mut data_alignment_indicator = false;
    let mut header_issues: Vec<PesHeaderMalformedKind> = Vec::new();
    if has_optional_header {
        if buf.len() < 9 {
            return Err(DemuxError::MalformedPes {
                pid,
                reason: "PES too short for optional header",
            });
        }
        // Byte 6 (`flags1`): top 2 bits must be the marker '10'. Per
        // H.222.0 V9 §2.4.3.6 Table 2-21. Surface as a structural issue
        // rather than a fatal — encoders that scramble this byte usually
        // still have valid PTS bytes.
        let marker_bits = (buf[6] >> 6) & 0x03;
        if marker_bits != 0b10 {
            header_issues.push(PesHeaderMalformedKind::InvalidMarkerBits);
        }
        data_alignment_indicator = (buf[6] & 0x04) != 0;
        let pts_dts_flags = (buf[7] >> 6) & 0x03;
        // Per H.222.0 V9 §2.4.3.7 Table 2-21 `PTS_DTS_flags` of `0b01`
        // is "forbidden" — DTS-without-PTS is not a valid shape. Some
        // legacy non-conformant encoders emit it (we've seen field
        // tests). Surface as a structural issue and treat as "no
        // PTS/DTS" (don't try to decode either since their offsets are
        // undefined under this flag).
        if pts_dts_flags == 0b01 {
            header_issues.push(PesHeaderMalformedKind::ForbiddenPtsDtsFlags);
        }
        let header_data_length = buf[8] as usize;
        body_off = 9 + header_data_length;
        if buf.len() < body_off {
            return Err(DemuxError::MalformedPes {
                pid,
                reason: "PES too short for declared header_data_length",
            });
        }
        // PTS/DTS live inside the header_data_length bytes; a shorter length
        // means the bytes that follow are payload, not timestamps.
        let needed = match pts_dts_flags {
            0b10 => 5,
            0b11 => 10,
            _ => 0,
        };
        if header_data_length < needed {
            header_issues.push(PesHeaderMalformedKind::HeaderDataLengthTooShort);
        }
        if (pts_dts_flags == 0b10 || pts_dts_flags == 0b11) && header_data_length >= 5 {
            // PTS in 5 bytes at offset 9.
            if buf.len() < 14 {
                return Err(DemuxError::MalformedPes {
                    pid,
                    reason: "PES too short for PTS",
                });
            }
            // 4-bit PTS prefix: '0010' (PTS-only) or '0011' (PTS+DTS).
            let pts_prefix = (buf[9] >> 4) & 0x0F;
            let expected_pts_prefix = if pts_dts_flags == 0b11 {
                0b0011
            } else {
                0b0010
            };
            if pts_prefix != expected_pts_prefix {
                header_issues.push(PesHeaderMalformedKind::InvalidPtsPrefix);
            }
            if !pts_dts_marker_bits_ok(&buf[9..14]) {
                header_issues.push(PesHeaderMalformedKind::InvalidPtsDtsMarkerBits);
            }
            pts = Some(crate::mpegts::common::Pts90khz::new(decode_pts_dts(
                &buf[9..14],
            )));
        }
        if pts_dts_flags == 0b11 && header_data_length >= 10 {
            if buf.len() < 19 {
                return Err(DemuxError::MalformedPes {
                    pid,
                    reason: "PES too short for DTS",
                });
            }
            // 4-bit DTS prefix: '0001'.
            let dts_prefix = (buf[14] >> 4) & 0x0F;
            if dts_prefix != 0b0001 {
                header_issues.push(PesHeaderMalformedKind::InvalidDtsPrefix);
            }
            if !pts_dts_marker_bits_ok(&buf[14..19]) {
                header_issues.push(PesHeaderMalformedKind::InvalidPtsDtsMarkerBits);
            }
            dts = Some(crate::mpegts::common::Pts90khz::new(decode_pts_dts(
                &buf[14..19],
            )));
        }
    }
    let payload = buf[body_off..].to_vec();
    Ok(Some(PesPayload {
        pid,
        stream_id,
        pts,
        dts,
        random_access_indicator,
        data_alignment_indicator,
        header_issues,
        payload,
    }))
}

/// Decode a 5-byte PTS or DTS field (4-bit prefix + 33 PTS bits + 3 marker bits).
fn decode_pts_dts(b: &[u8]) -> i64 {
    let p32_30 = ((b[0] >> 1) & 0x07) as u64;
    let p29_15 = (((b[1] as u64) << 7) | ((b[2] as u64) >> 1)) & 0x7FFF;
    let p14_0 = (((b[3] as u64) << 7) | ((b[4] as u64) >> 1)) & 0x7FFF;
    ((p32_30 << 30) | (p29_15 << 15) | p14_0) as i64
}

/// Validate the 3 trailing marker bits inside a 5-byte PTS / DTS field.
///
/// Per H.222.0 V9 §2.4.3.7 the 5-byte field is laid out as:
///
/// ```text
///   prefix(4) | PTS[32..30](3) | marker(1)
///   PTS[29..22](8)
///   PTS[21..15](7) | marker(1)
///   PTS[14..7](8)
///   PTS[6..0](7) | marker(1)
/// ```
///
/// Each of the three `marker` bits MUST be `1`. Returns `true` when all
/// three are set; the caller decides whether to surface an issue or
/// continue decoding (we always continue — the field's data bits are
/// still readable independently of the markers).
fn pts_dts_marker_bits_ok(b: &[u8]) -> bool {
    (b[0] & 0x01) == 1 && (b[2] & 0x01) == 1 && (b[4] & 0x01) == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pes(stream_id: u8, pts: Option<i64>, payload: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
        // Reserve length field; backfill at end.
        s.push(0x00);
        s.push(0x00);
        // Optional header: marker(2)=10 + scrambling(2)=00 + priority(1) + alignment(1) +
        // copyright(1) + original(1)
        s.push(0x80);
        // pts_dts_flags(2) + ESCR(1) + ES_rate(1) + DSM_trick(1) + additional_copy(1) +
        // PES_CRC(1) + PES_extension(1)
        let pts_dts_flags = if pts.is_some() { 0b10 } else { 0b00 };
        s.push(pts_dts_flags << 6);
        let pts_bytes = if let Some(p) = pts {
            let p = p as u64;
            let b0 = 0x21 | (((p >> 30) as u8) << 1) & 0x0E;
            let b1 = ((p >> 22) & 0xFF) as u8;
            let b2 = (((p >> 14) & 0xFE) as u8) | 0x01;
            let b3 = ((p >> 7) & 0xFF) as u8;
            let b4 = (((p << 1) & 0xFE) as u8) | 0x01;
            vec![b0, b1, b2, b3, b4]
        } else {
            vec![]
        };
        s.push(pts_bytes.len() as u8); // header_data_length
        s.extend_from_slice(&pts_bytes);
        s.extend_from_slice(payload);
        // Backfill PES_packet_length (count of bytes after byte 5).
        let pes_packet_length = (s.len() - 6) as u16;
        s[4] = (pes_packet_length >> 8) as u8;
        s[5] = pes_packet_length as u8;
        s
    }

    /// H.222.0 §2.4.3.7: PTS/DTS sit inside the PES_header_data_length bytes.
    /// With PTS_DTS_flags = '10' and header_data_length = 0 the PTS is absent,
    /// and the 5 bytes after the header are elementary-stream payload.
    #[test]
    fn pts_flag_with_zero_header_data_length_is_flagged_and_bytes_stay_payload() {
        let mut pes = build_pes(0xE0, Some(90_000), b"abc");
        pes[8] = 0; // header_data_length 0, flags still say PTS
        let p = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert_eq!(p.pts, None);
        assert_eq!(
            p.header_issues,
            vec![PesHeaderMalformedKind::HeaderDataLengthTooShort]
        );
        assert_eq!(p.payload, [&pes[9..14], b"abc"].concat());
    }

    #[test]
    fn pts_dts_flags_with_header_data_length_5_reads_pts_but_not_dts() {
        // flags '11' need 10 bytes; 5 covers PTS only.
        let mut pes = build_pes(0xE0, Some(90_000), b"abcdefghij");
        pes[7] = 0b11 << 6;
        let p = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert_eq!(p.pts.map(|t| t.as_ticks()), Some(90_000));
        assert_eq!(p.dts, None);
        assert!(
            p.header_issues
                .contains(&PesHeaderMalformedKind::HeaderDataLengthTooShort)
        );
        assert_eq!(
            p.header_issues
                .iter()
                .filter(|k| **k == PesHeaderMalformedKind::HeaderDataLengthTooShort)
                .count(),
            1
        );
        assert_eq!(p.payload, b"abcdefghij");
    }

    /// H.222.0 Table 2-21: every stream_id except program_stream_map (0xBC),
    /// padding (0xBE), private_stream_2 (0xBF), ECM (0xF0), EMM (0xF1),
    /// DSMCC (0xF2), H.222.1 type E (0xF8) and program_stream_directory (0xFF)
    /// carries the optional PES header.
    #[test]
    fn optional_header_present_for_every_table_2_21_stream_id() {
        for sid in [0xF3u8, 0xF4, 0xF5, 0xF6, 0xF7, 0xF9, 0xFA, 0xFB, 0xFE] {
            let p = parse_complete(0x100, &build_pes(sid, Some(90_000), b"xy"), false)
                .unwrap()
                .unwrap();
            assert_eq!(
                p.pts.map(|t| t.as_ticks()),
                Some(90_000),
                "stream_id 0x{sid:02X}"
            );
            assert_eq!(p.payload, b"xy");
        }
        for sid in [0xBCu8, 0xBE, 0xBF, 0xF0, 0xF1, 0xF2, 0xF8, 0xFF] {
            let pes = build_pes(sid, None, b"xy");
            let p = parse_complete(0x100, &pes, false).unwrap().unwrap();
            assert_eq!(&p.payload[..], &pes[6..], "stream_id 0x{sid:02X}");
        }
    }

    /// A completed PES survives a malformed predecessor
    /// on the same PID, and both are reported in wire order.
    #[test]
    fn completed_pes_survives_a_malformed_predecessor() {
        let mut r = Reassembler::new(4 << 20, 64 << 20);
        // PUSI #1: stream_id 0xBD, PTS flagged, declared length 64, but only
        // these 10 bytes ever arrive (the rest is lost).
        let short = [0x00, 0x00, 0x01, 0xBD, 0x00, 0x40, 0x84, 0x80, 0x05, 0x21];
        assert!(r.push(0x200, &short, true, false, false).is_empty());
        // PUSI #2: a complete bounded PES that length-completes in this call.
        let good = build_pes(0xBD, Some(90_000), b"klv-bytes-here");
        let outcomes = r.push(0x200, &good, true, false, false);
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert!(
            matches!(
                outcomes[0],
                ReassemblyOutcome::Malformed {
                    pid: 0x200,
                    // header_data_length (5) is checked before the PTS
                    // bytes themselves, so this is the reason that fires.
                    reason: "PES too short for declared header_data_length"
                }
            ),
            "{outcomes:?}"
        );
        assert!(
            matches!(&outcomes[1], ReassemblyOutcome::Complete(p) if p.payload == b"klv-bytes-here"),
            "{outcomes:?}"
        );
    }

    /// Mirror order: a valid predecessor flushed at
    /// PUSI survives a corrupt BOUNDED follower that length-completes in the
    /// same `push`; both are reported in wire order `[Complete, Malformed]`.
    #[test]
    fn completed_predecessor_survives_a_malformed_follower() {
        let mut r = Reassembler::new(4 << 20, 64 << 20);
        // PUSI #1: an unbounded (PES_packet_length = 0) video PES, so only
        // the next PUSI completes it.
        let mut good = build_pes(0xE0, Some(90_000), b"video-bytes");
        good[4] = 0;
        good[5] = 0;
        assert!(r.push(0x100, &good, true, false, true).is_empty());
        // PUSI #2: bounded (declared total 6 + 4 = 10 bytes, all present) but
        // header_data_length 5 overruns it, so it completes AND fails to parse.
        let bad = [0x00, 0x00, 0x01, 0xE0, 0x00, 0x04, 0x80, 0x80, 0x05, 0x21];
        let outcomes = r.push(0x100, &bad, true, false, true);
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert!(
            matches!(&outcomes[0], ReassemblyOutcome::Complete(p) if p.payload == b"video-bytes"),
            "{outcomes:?}"
        );
        assert!(
            matches!(
                outcomes[1],
                ReassemblyOutcome::Malformed {
                    pid: 0x100,
                    reason: "PES too short for declared header_data_length"
                }
            ),
            "{outcomes:?}"
        );
    }

    #[test]
    fn reassembles_one_pes_via_pusi_then_pusi() {
        let mut pes = build_pes(0xE0, Some(900_000), b"hello");
        // Video PES (stream_id 0xE0-0xEF) commonly uses PES_packet_length=0
        // for unbounded payload — boundary is then detected only via the next
        // PUSI on the same PID. Zero the length field so this test exercises
        // the PUSI-driven path rather than length-driven completion.
        pes[4] = 0;
        pes[5] = 0;
        // Split across two PUSI calls: first PUSI starts the PES, second PUSI
        // emits it.
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        let out = r.push(0x100, &pes, true, false, true);
        assert!(out.is_empty());
        // A second PUSI on the same PID closes the previous one. Zero this
        // PES's length field too so it doesn't immediately length-complete
        // and add a second outcome.
        let mut pes2 = build_pes(0xE0, None, b"");
        pes2[4] = 0;
        pes2[5] = 0;
        let out = r.push(0x100, &pes2, true, false, true);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ReassemblyOutcome::Complete(p) => {
                assert_eq!(p.pts, Some(crate::mpegts::common::Pts90khz::new(900_000)));
                assert_eq!(p.payload, b"hello");
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn length_driven_completion() {
        let pes = build_pes(0xE0, Some(0), b"abc");
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        let out = r.push(0x100, &pes, true, false, true);
        // PES_packet_length is set => completion when all bytes seen.
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn per_pid_overflow_emits_event_and_clears() {
        let mut r = Reassembler::new(64, 1 << 20);
        let _ = r.push(
            0x100,
            b"\x00\x00\x01\xE0\x00\x00\x80\x00\x00",
            true,
            false,
            true,
        );
        // Now flood until overflow.
        let big = vec![0xCC; 256];
        let out = r.push(0x100, &big, false, false, true);
        assert!(matches!(out[0], ReassemblyOutcome::Overflow { pid: 0x100 }));
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn aggregate_overflow() {
        let mut r = Reassembler::new(1 << 20, 200);
        let _ = r.push(
            0x100,
            b"\x00\x00\x01\xE0\x00\x00\x80\x00\x00",
            true,
            false,
            true,
        );
        let big = vec![0xCC; 300];
        let out = r.push(0x100, &big, false, false, true);
        assert!(
            out.iter()
                .any(|o| matches!(o, ReassemblyOutcome::OverflowTotal))
        );
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn random_access_indicator_first_packet_wins() {
        // Start a PES with RAI=true, append continuation with RAI=false, then
        // a new PUSI to flush — the completed PES retains RAI=true.
        let mut pes = build_pes(0xE0, Some(900_000), b"hello");
        pes[4] = 0;
        pes[5] = 0;
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        // PUSI=1 with RAI=true latches RAI on the in-flight PES.
        let _ = r.push(0x100, &pes, true, true, true);
        // Continuation with RAI=false MUST NOT overwrite the latched value.
        let _ = r.push(0x100, b"world", false, false, true);
        // Second PUSI closes the previous PES.
        let mut pes2 = build_pes(0xE0, None, b"");
        pes2[4] = 0;
        pes2[5] = 0;
        let out = r.push(0x100, &pes2, true, false, true);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ReassemblyOutcome::Complete(p) => {
                assert!(p.random_access_indicator, "RAI should latch from PUSI=1");
                assert_eq!(p.payload, b"helloworld");
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn bounded_pes_with_trailing_bytes_emits_only_declared_payload() {
        // VIDEO-03 regression: when the reassembler's buffer holds more bytes
        // than the declared PES_packet_length (e.g., the next PES's first
        // bytes were appended in the same TS-payload chunk), the completion
        // path must slice off exactly `total` bytes and not leak trailing
        // bytes into the emitted sample's payload. Per H.222.0 §2.4.3.7,
        // `PES_packet_length` is authoritative for non-zero (bounded) values.
        let pes = build_pes(0xE0, Some(900_000), b"abc");
        let pes_len = pes.len();
        let mut combined = pes.clone();
        combined.extend_from_slice(b"GARBAGE_NEXT_PES_BYTES");
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        let out = r.push(0x100, &combined, true, false, true);
        assert_eq!(
            out.len(),
            1,
            "exactly one PES completes from bounded length"
        );
        match &out[0] {
            ReassemblyOutcome::Complete(p) => {
                assert_eq!(
                    p.payload, b"abc",
                    "payload must be EXACTLY the declared body, not include trailing bytes"
                );
            }
            _ => panic!("expected Complete"),
        }
        // total_buffered must have been decremented by exactly `pes_len`
        // (the declared total), not by the whole combined buffer length.
        // Residual trailing bytes are discarded (option b — best-effort;
        // next PUSI on this PID re-initializes the per-PID state).
        assert_eq!(
            r.buffered_bytes(),
            0,
            "after length-driven completion, total_buffered should reflect exact consumed count"
        );
        let _ = pes_len;
    }

    /// The residual of a length-completed PES on one PID is released, and
    /// the bytes still held for another PID's partial stay counted.
    #[test]
    fn bounded_pes_total_buffered_decrements_by_exact_consumed_count() {
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        // PID 0x201: a bounded PES with only its first packet's worth in.
        let pes_b = build_pes(0xC0, Some(0), &[0xBB; 300]);
        assert!(r.push(0x201, &pes_b[..184], true, false, false).is_empty());
        // PID 0x200: a complete bounded PES followed by trailing bytes.
        let mut chunk_a = build_pes(0xE0, Some(0), b"hello");
        chunk_a.extend_from_slice(&[0xAA; 7]);
        let out = r.push(0x200, &chunk_a, true, false, true);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ReassemblyOutcome::Complete(p) => assert_eq!(p.payload, b"hello"),
            _ => panic!("expected Complete"),
        }
        assert_eq!(r.buffered_bytes(), 184, "only PID 0x201's partial is held");
        assert!(!r.has_pid(0x200));
        assert!(r.has_pid(0x201));
    }

    #[test]
    fn pusi_completion_of_a_short_bounded_pes_reports_damage_then_completes() {
        let pes = build_pes(0xC0, Some(0), &[0xAA; 300]); // bounded
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        assert!(r.push(0x101, &pes[..184], true, false, false).is_empty());
        let out = r.push(
            0x101,
            &build_pes(0xC0, Some(3000), b"next"),
            true,
            false,
            false,
        );
        assert!(
            matches!(&out[0], ReassemblyOutcome::Malformed { pid: 0x101, reason } if *reason == DAMAGE_SHORT_PES)
        );
        assert!(matches!(&out[1], ReassemblyOutcome::Complete(p) if p.payload.len() == 184 - 14));
    }

    #[test]
    fn marked_partial_reports_cc_gap_even_when_length_completes() {
        let pes = build_pes(0xC0, Some(0), &[0xAA; 300]);
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        r.push(0x101, &pes[..184], true, false, false);
        r.mark_damaged(0x101);
        let out = r.push(0x101, &pes[184..], false, false, false);
        assert!(
            matches!(&out[0], ReassemblyOutcome::Malformed { reason, .. } if *reason == DAMAGE_CC_GAP)
        );
        assert!(matches!(&out[1], ReassemblyOutcome::Complete(_)));
    }

    #[test]
    fn drain_partial_outcomes_marks_short_bounded_and_keeps_unbounded_clean() {
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        let short = build_pes(0xC0, Some(0), &[0xAA; 300]);
        r.push(0x101, &short[..184], true, false, false);
        let mut video = build_pes(0xE0, Some(0), b"vid");
        video[4] = 0;
        video[5] = 0;
        r.push(0x100, &video, true, false, true);
        let out = r.drain_partial_outcomes();
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(
            matches!(&out[0], ReassemblyOutcome::Complete(p) if p.pid == 0x100 && p.payload == b"vid")
        );
        assert!(
            matches!(&out[1], ReassemblyOutcome::Malformed { pid: 0x101, reason } if *reason == DAMAGE_SHORT_PES)
        );
        assert!(matches!(&out[2], ReassemblyOutcome::Complete(p) if p.pid == 0x101));
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn aggregate_overflow_reports_every_dropped_pid() {
        let mut r = Reassembler::new(1 << 20, 400);
        r.push(
            0x101,
            &build_pes(0xC0, Some(0), &[0; 300])[..184],
            true,
            false,
            false,
        );
        r.push(
            0x102,
            &build_pes(0xC0, Some(0), &[0; 300])[..184],
            true,
            false,
            false,
        );
        let out = r.push(
            0x103,
            &build_pes(0xC0, Some(0), &[0; 300])[..184],
            true,
            false,
            false,
        );
        assert_eq!(out, vec![ReassemblyOutcome::OverflowTotal]);
        assert_eq!(r.take_total_overflow_pids(), vec![0x101, 0x102, 0x103]);
        assert!(r.take_total_overflow_pids().is_empty());
    }

    #[test]
    fn random_access_indicator_false_when_pusi_packet_clears_it() {
        let pes = build_pes(0xE0, Some(0), b"abc");
        let mut r = Reassembler::new(1 << 20, 4 << 20);
        let out = r.push(0x100, &pes, true, false, true);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ReassemblyOutcome::Complete(p) => {
                assert!(!p.random_access_indicator);
            }
            _ => panic!("expected Complete"),
        }
    }

    // ------------------------------------------------------------------
    // PES header structural validation
    // ------------------------------------------------------------------

    #[test]
    fn parse_complete_captures_data_alignment_indicator_true() {
        let mut pes = build_pes(0xE0, Some(0), b"x");
        // Set bit 2 of flags1 (byte 6).
        pes[6] |= 0x04;
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed.data_alignment_indicator,
            "DAI bit set in flags1 should round-trip to PesPayload::data_alignment_indicator"
        );
    }

    #[test]
    fn parse_complete_captures_data_alignment_indicator_false() {
        // build_pes pushes flags1 = 0x80 (marker only); DAI = 0.
        let pes = build_pes(0xE0, Some(0), b"x");
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(!parsed.data_alignment_indicator);
    }

    #[test]
    fn parse_complete_flags_forbidden_pts_dts_combo() {
        let mut pes = build_pes(0xE0, Some(0), b"x");
        // Set PTS_DTS_flags to 0b01 (forbidden combination).
        pes[7] = (pes[7] & 0x3F) | (0b01 << 6);
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed
                .header_issues
                .contains(&PesHeaderMalformedKind::ForbiddenPtsDtsFlags)
        );
    }

    #[test]
    fn parse_complete_flags_invalid_byte6_marker_bits() {
        let mut pes = build_pes(0xE0, Some(0), b"x");
        // Clear the top '10' marker on flags1 — leave bit 2 (DAI) etc. zero.
        pes[6] &= 0x3F;
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed
                .header_issues
                .contains(&PesHeaderMalformedKind::InvalidMarkerBits)
        );
    }

    #[test]
    fn parse_complete_flags_invalid_pts_prefix() {
        let mut pes = build_pes(0xE0, Some(0), b"x");
        // PTS lives at offset 9. Top nibble should be '0010'. Smash it.
        pes[9] = (pes[9] & 0x0F) | 0xA0; // top nibble '1010'
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed
                .header_issues
                .contains(&PesHeaderMalformedKind::InvalidPtsPrefix)
        );
    }

    #[test]
    fn parse_complete_flags_invalid_pts_marker_bits() {
        let mut pes = build_pes(0xE0, Some(0), b"x");
        // Clear the bottom marker bit on PTS byte 0 (offset 9).
        pes[9] &= 0xFE;
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed
                .header_issues
                .contains(&PesHeaderMalformedKind::InvalidPtsDtsMarkerBits)
        );
    }

    #[test]
    fn parse_complete_conformant_has_no_header_issues() {
        let pes = build_pes(0xE0, Some(900_000), b"x");
        let parsed = parse_complete(0x100, &pes, false).unwrap().unwrap();
        assert!(
            parsed.header_issues.is_empty(),
            "conformant PES should produce zero header issues, got {:?}",
            parsed.header_issues
        );
    }

    #[test]
    fn zero_length_non_video_pes_is_dropped_and_flagged() {
        let mut r = Reassembler::new(1 << 20, 1 << 22);
        // Build a zero-PES_packet_length PES on an AUDIO stream_id (0xC0).
        let pes = build_pes(0xC0, Some(0), b"abcdef");
        // build_pes backfills PES_packet_length; force it to zero (bytes 4-5).
        let mut pes = pes;
        pes[4] = 0;
        pes[5] = 0;
        // is_video=false: PMT classifies this PID as non-video.
        let out = r.push(0x101, &pes, true, false, false);
        assert!(
            out.iter().any(|o| matches!(
                o,
                ReassemblyOutcome::ZeroLengthNonVideo {
                    pid: 0x101,
                    stream_id: 0xC0
                }
            )),
            "got {out:?}"
        );
        // The partial must be dropped — no buffered bytes linger.
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn zero_length_video_pes_still_unbounded() {
        let mut r = Reassembler::new(1 << 20, 1 << 22);
        let mut pes = build_pes(0xE0, Some(0), b"abcdef");
        pes[4] = 0;
        pes[5] = 0; // zero length is LEGAL for video
        // is_video=true: PMT classifies this PID as video.
        let out = r.push(0x100, &pes, true, false, true);
        assert!(
            !out.iter()
                .any(|o| matches!(o, ReassemblyOutcome::ZeroLengthNonVideo { .. })),
            "video zero-length must not flag; got {out:?}"
        );
    }

    #[test]
    fn zero_length_av1_0xbd_pes_still_unbounded() {
        // AV1 *video* in MPEG-2-TS-binding carriage rides
        // stream_id=0xBD (private_stream_1) yet is StreamKind::Video — so an
        // unbounded PES on 0xBD must NOT be flagged when is_video=true. This
        // proves the exemption is kind-based, not a stream_id allowlist.
        let mut r = Reassembler::new(1 << 20, 1 << 22);
        let mut pes = build_pes(0xBD, Some(0), b"abcdef");
        pes[4] = 0;
        pes[5] = 0;
        let out = r.push(0x100, &pes, true, false, true);
        assert!(
            !out.iter()
                .any(|o| matches!(o, ReassemblyOutcome::ZeroLengthNonVideo { .. })),
            "0xBD video (AV1 binding) zero-length must not flag; got {out:?}"
        );
    }

    #[test]
    fn zero_length_0xbd_non_video_pes_is_dropped_and_flagged() {
        // Inverse of the AV1 case: a zero-length PES on stream_id=0xBD that the
        // PMT classifies as NON-video (e.g. AC-3 audio / DVB subtitle /
        // async-KLV / private-data — all of which also use 0xBD) IS caught.
        // Confirms the decision keys on the kind, not on stream_id==0xBD.
        let mut r = Reassembler::new(1 << 20, 1 << 22);
        let mut pes = build_pes(0xBD, Some(0), b"abcdef");
        pes[4] = 0;
        pes[5] = 0;
        let out = r.push(0x102, &pes, true, false, false);
        assert!(
            out.iter().any(|o| matches!(
                o,
                ReassemblyOutcome::ZeroLengthNonVideo {
                    pid: 0x102,
                    stream_id: 0xBD
                }
            )),
            "0xBD non-video zero-length must flag; got {out:?}"
        );
        assert_eq!(r.buffered_bytes(), 0);
    }
}
