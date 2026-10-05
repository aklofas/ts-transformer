//! 188-byte MPEG-TS packet writer.
//!
//! Handles:
//! - Sync byte (0x47), TEI, transport priority
//! - `payload_unit_start_indicator` (PUSI)
//! - 13-bit PID
//! - Per-PID 4-bit continuity counter (increments on payload-bearing packets
//!   per ISO/IEC 13818-1 §2.4.3.3)
//! - Adaptation field: optional PCR (6 bytes), random_access_indicator,
//!   stuffing
//! - Payload fill from caller-supplied bytes
//!
//! Cadence decisions (when to emit PCR, when to emit PSI, when to start a
//! new PES) live in the `Muxer` orchestrator. This module is purely
//! mechanical packet assembly.

use crate::mpegts::common::{Pcr27mhz, TS_PACKET_SIZE, TS_SYNC_BYTE};
use alloc::boxed::Box;

/// Per-PID 4-bit continuity counters.
///
/// Flat `[u8; 8192]` indexed by PID (PIDs are 13-bit; 8192 entries cover
/// the full range). Only the low 4 bits of each byte are used. Boxed so
/// the 8 KB lives on the heap rather than on the stack of containing
/// structs.
#[derive(Debug)]
pub(crate) struct ContinuityCounters {
    counters: Box<[u8; 8192]>,
}

impl ContinuityCounters {
    pub fn new() -> Self {
        Self {
            counters: Box::new([0u8; 8192]),
        }
    }

    /// Get the next continuity counter value for `pid`, incrementing it.
    /// Per spec, this is called only when the packet has a payload.
    fn next(&mut self, pid: u16) -> u8 {
        let idx = (pid & 0x1FFF) as usize;
        let cc = self.counters[idx] & 0x0F;
        self.counters[idx] = (cc + 1) & 0x0F;
        cc
    }
}

impl Default for ContinuityCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// Optional adaptation field contents to attach to a packet.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AdaptationField {
    /// Set the `random_access_indicator` flag — used on the first packet of
    /// an IDR access unit so receivers can find a tune-in point.
    pub random_access: bool,
    /// PCR to embed. None = no PCR field.
    pub pcr: Option<Pcr27mhz>,
}

impl AdaptationField {
    pub fn is_empty(&self) -> bool {
        !self.random_access && self.pcr.is_none()
    }

    /// Number of bytes the adaptation field occupies excluding stuffing,
    /// including the length byte.
    /// = 1 byte length + 1 byte flags + (6 bytes PCR if present)
    fn min_size(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            2 + if self.pcr.is_some() { 6 } else { 0 }
        }
    }
}

/// Result of writing a TS packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriteResult {
    /// Bytes of payload consumed from the input.
    pub payload_consumed: usize,
}

/// Write one 188-byte TS packet into `out`, packing as much of `payload` as
/// fits given the requested adaptation field.
///
/// `out.len()` must be exactly 188; panics otherwise (private API, caller
/// responsibility).
///
/// Returns the number of payload bytes consumed. The caller advances its
/// payload cursor and calls again with the remainder.
///
/// `payload_unit_start` should be true on the first packet of a new PES or
/// PSI section.
pub(crate) fn write_packet(
    out: &mut [u8; 188],
    pid: u16,
    payload_unit_start: bool,
    adaptation: AdaptationField,
    payload: &[u8],
    counters: &mut ContinuityCounters,
) -> WriteResult {
    debug_assert!(pid <= 0x1FFF, "PID out of range");

    // Header byte 0: sync byte
    out[0] = TS_SYNC_BYTE;
    // Header byte 1: TEI (0) | PUSI | TP (0) | PID high 5 bits
    out[1] = ((payload_unit_start as u8) << 6) | ((pid >> 8) as u8 & 0x1F);
    // Header byte 2: PID low 8 bits
    out[2] = (pid & 0xFF) as u8;

    // Capacity bookkeeping. `af_min` is the AF byte count when content is
    // present (length + flags + optional PCR). When af_min == 0 but payload
    // can't fill 184 bytes, we still need a stuffing-only AF.
    //
    // ISO/IEC 13818-1 §2.4.3.4: when adaptation_field_length > 0, the AF
    // body must begin with the flags byte. So a stuffing-only AF needs
    // 2 bytes overhead (length + 0x00 flags) — except in the exact 183-byte
    // payload case, where af_length=0 (length byte alone, no flags) is the
    // spec-compliant choice.
    let af_min = adaptation.min_size();
    let no_af_payload_capacity: usize = TS_PACKET_SIZE - 4; // 184
    let needs_padding_only_af = af_min == 0 && payload.len() < no_af_payload_capacity;
    let af_overhead = if af_min > 0 {
        af_min
    } else if needs_padding_only_af {
        if payload.len() == no_af_payload_capacity - 1 {
            1 // exact fit with af_length=0
        } else {
            2 // length + 0x00 flags byte, then stuffing
        }
    } else {
        0
    };
    let payload_capacity = TS_PACKET_SIZE - 4 - af_overhead;
    let to_copy = payload_capacity.min(payload.len());
    let stuffing = payload_capacity - to_copy;

    // adaptation_field_control: 11 if AF present, 01 if payload-only.
    // continuity_counter is incremented because we always carry payload here.
    let cc = counters.next(pid);
    let afc = if af_overhead > 0 { 0b11 } else { 0b01 };
    out[3] = (afc << 4) | (cc & 0x0F);

    let mut cursor = 4;

    if afc == 0b11 {
        // adaptation_field_length = bytes after the length byte itself.
        // = (af_overhead - 1) for the structural part (flags + optional PCR
        //   when af_min > 0; flags alone when stuffing-only with af_overhead=2;
        //   nothing when af_overhead=1, the af_length=0 case)
        // + stuffing
        let af_body_after_length = (af_overhead - 1) + stuffing;
        out[cursor] = af_body_after_length as u8;
        cursor += 1;

        if af_min > 0 {
            // Flags byte: discontinuity=0, random_access, ES_priority=0,
            // PCR_flag, OPCR=0, splicing=0, transport_private=0, AF_extension=0
            let flags: u8 =
                ((adaptation.random_access as u8) << 6) | ((adaptation.pcr.is_some() as u8) << 4);
            out[cursor] = flags;
            cursor += 1;

            if let Some(pcr) = adaptation.pcr {
                write_pcr(&mut out[cursor..cursor + 6], pcr);
                cursor += 6;
            }
        } else if af_overhead == 2 {
            // Stuffing-only AF with non-zero length — write 0x00 flags byte.
            out[cursor] = 0x00;
            cursor += 1;
        }

        // Stuffing bytes (0xFF per spec).
        for byte in &mut out[cursor..cursor + stuffing] {
            *byte = 0xFF;
        }
        cursor += stuffing;
    }

    // Payload.
    out[cursor..cursor + to_copy].copy_from_slice(&payload[..to_copy]);
    cursor += to_copy;

    debug_assert_eq!(cursor, TS_PACKET_SIZE);

    WriteResult {
        payload_consumed: to_copy,
    }
}

/// Write one 188-byte adaptation-field-only TS packet carrying just a PCR
/// (no payload). Used by the muxer to satisfy H.222.0 Annex D's 100ms
/// max-PCR-interval when the configured PCR PID hasn't received any
/// caller-pushed payload within the configured `pcr_interval_ms`.
///
/// Per H.222.0 §2.4.3.3, the continuity_counter is NOT incremented on
/// adaptation-only packets — this function leaves `counters` alone (it is
/// passed only to read the current CC value, which the spec requires the
/// PCR-only packet to repeat from the last payload-bearing packet on the
/// same PID).
///
/// `out.len()` must be exactly 188; panics otherwise (private API).
pub(crate) fn write_pcr_only_packet(
    out: &mut [u8; 188],
    pid: u16,
    pcr: Pcr27mhz,
    counters: &ContinuityCounters,
) {
    debug_assert!(pid <= 0x1FFF, "PID out of range");

    // TS header.
    out[0] = TS_SYNC_BYTE;
    out[1] = (pid >> 8) as u8 & 0x1F; // TEI=0, PUSI=0, TP=0, PID hi
    out[2] = (pid & 0xFF) as u8;

    // adaptation_field_control = 0b10 (AF only, no payload).
    // continuity_counter: repeat the last payload CC without incrementing
    // (H.222.0 §2.4.3.3 — afc='10' packets do not increment the counter).
    // `ContinuityCounters::next` increments AFTER returning the emitted
    // value, so `counters.counters[idx]` holds the NEXT value to emit on a
    // payload packet; `wrapping_sub(1)` recovers the last emitted payload
    // CC. Consecutive heartbeats correctly repeat the same value because
    // this function is non-mutating. The pre-first-payload edge (counters
    // slot == 0 → wrapping_sub gives 15) is benign — the receiver doesn't
    // validate CC on adaptation-only packets before first PUSI.
    let idx = (pid & 0x1FFF) as usize;
    let cc = counters.counters[idx].wrapping_sub(1) & 0x0F;
    out[3] = (0b10 << 4) | cc;

    // adaptation_field_length: 188 - 4 (header) - 1 (length byte itself) = 183.
    out[4] = 183;
    // Flags byte: discontinuity=0, random_access=0, ES_priority=0,
    // PCR_flag=1, OPCR=0, splicing=0, transport_private=0, AF_extension=0.
    out[5] = 0x10;
    write_pcr(&mut out[6..12], pcr);
    // Stuffing (0xFF) fills the remainder per ITU-T H.222.0 §2.4.3.4.
    for byte in &mut out[12..188] {
        *byte = 0xFF;
    }
}

/// Encode a PCR into 6 bytes per ISO/IEC 13818-1 §2.4.3.5:
///   33-bit base (90 kHz) | 6 reserved bits (set to 1) | 9-bit extension (27 MHz mod 300)
fn write_pcr(out: &mut [u8], pcr: Pcr27mhz) {
    debug_assert!(out.len() == 6);
    let base = pcr.base();
    let ext = pcr.extension() as u32;
    out[0] = (base >> 25) as u8;
    out[1] = (base >> 17) as u8;
    out[2] = (base >> 9) as u8;
    out[3] = (base >> 1) as u8;
    out[4] = (((base & 0x1) << 7) as u8) | 0x7E | ((ext >> 8) & 0x01) as u8;
    out[5] = (ext & 0xFF) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_basic() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let payload = vec![0xAB; 200]; // larger than capacity
        let result = write_packet(
            &mut buf,
            0x1011,
            true,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        assert_eq!(buf[0], 0x47);
        // PID 0x1011 high-5 = 0b10000 = 0x10. With PUSI bit = 0x40.
        // byte1 = 0x40 | 0x10 = 0x50.
        assert_eq!(buf[1], 0x50);
        assert_eq!(buf[2], 0x11);
        // afc = 01 (no adaptation), cc = 0 first time
        assert_eq!(buf[3], 0b01 << 4);
        // 184 bytes of payload should be copied (all 0xAB).
        assert_eq!(result.payload_consumed, 184);
        assert!(buf[4..].iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn continuity_counter_increments_per_pid() {
        let mut cc = ContinuityCounters::new();
        let payload = vec![0u8; 200];
        let mut buf = [0u8; 188];
        for expected_cc in 0..16 {
            write_packet(
                &mut buf,
                0x100,
                false,
                AdaptationField::default(),
                &payload,
                &mut cc,
            );
            assert_eq!(buf[3] & 0x0F, expected_cc, "iteration {}", expected_cc);
        }
        // After 16 increments, counter wraps back to 0.
        write_packet(
            &mut buf,
            0x100,
            false,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        assert_eq!(buf[3] & 0x0F, 0);
    }

    #[test]
    fn continuity_counter_separate_per_pid() {
        let mut cc = ContinuityCounters::new();
        let payload = vec![0u8; 200];
        let mut buf = [0u8; 188];

        // PID 0x100 advances to 1
        write_packet(
            &mut buf,
            0x100,
            false,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        // PID 0x200 should still be at 0
        write_packet(
            &mut buf,
            0x200,
            false,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        assert_eq!(buf[3] & 0x0F, 0);
    }

    #[test]
    fn adaptation_random_access_flag() {
        let mut cc = ContinuityCounters::new();
        let payload = vec![0u8; 200];
        let mut buf = [0u8; 188];
        let af = AdaptationField {
            random_access: true,
            pcr: None,
        };
        write_packet(&mut buf, 0x1011, true, af, &payload, &mut cc);
        // afc = 11
        assert_eq!(buf[3] >> 4, 0b11);
        // AF length = 1 (just the flags byte)
        assert_eq!(buf[4], 1);
        // Flags byte: random_access bit (0x40)
        assert_eq!(buf[5], 0x40);
    }

    #[test]
    fn adaptation_with_pcr_layout() {
        let mut cc = ContinuityCounters::new();
        let payload = vec![0u8; 200];
        let mut buf = [0u8; 188];
        let pcr = Pcr27mhz::new(0); // base=0, ext=0
        let af = AdaptationField {
            random_access: false,
            pcr: Some(pcr),
        };
        write_packet(&mut buf, 0x1011, false, af, &payload, &mut cc);
        // afc = 11
        assert_eq!(buf[3] >> 4, 0b11);
        // AF length = 1 flag byte + 6 PCR bytes = 7
        assert_eq!(buf[4], 7);
        // Flags byte: PCR_flag (0x10)
        assert_eq!(buf[5], 0x10);
        // PCR bytes: all zero base+ext, but byte 4 has the reserved 0x7E
        // bits set per spec.
        assert_eq!(buf[6], 0x00);
        assert_eq!(buf[7], 0x00);
        assert_eq!(buf[8], 0x00);
        assert_eq!(buf[9], 0x00);
        assert_eq!(buf[10], 0x7E); // (base_low=0)<<7 | 0x7E reserved | ext_high=0
        assert_eq!(buf[11], 0x00);
    }

    #[test]
    fn pcr_encoding_round_trip() {
        // Build a PCR with known base and extension, encode it, decode it,
        // verify match.
        let base: u64 = 0x1_2345_6789;
        let ext: u32 = 250;
        let pcr = Pcr27mhz::new(base * 300 + ext as u64);
        let mut buf = [0u8; 6];
        write_pcr(&mut buf, pcr);
        let dec_base = ((buf[0] as u64) << 25)
            | ((buf[1] as u64) << 17)
            | ((buf[2] as u64) << 9)
            | ((buf[3] as u64) << 1)
            | ((buf[4] as u64) >> 7);
        let dec_ext = (((buf[4] & 0x01) as u32) << 8) | (buf[5] as u32);
        assert_eq!(dec_base, base);
        assert_eq!(dec_ext, ext);
        // Reserved bits in byte 4: 0x7E bits should be set.
        assert_eq!(buf[4] & 0x7E, 0x7E);
    }

    #[test]
    fn stuffing_when_payload_short() {
        // ISO/IEC 13818-1 §2.4.3.4: when adaptation_field_length > 0, the
        // first byte after the length is the AF flags byte. A bare-stuffing
        // AF must therefore write a 0x00 flags byte before any 0xFF
        // stuffing — otherwise a strict decoder reads the leading 0xFF as
        // PCR_flag=1 and expects a PCR that isn't there.
        let mut cc = ContinuityCounters::new();
        let payload = vec![0xAA; 50];
        let mut buf = [0u8; 188];
        write_packet(
            &mut buf,
            0x1011,
            true,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        // afc = 11 (adaptation field present)
        assert_eq!(buf[3] >> 4, 0b11);
        // 188 - 4 (TS header) - 1 (af length) - 1 (af flags) = 182 bytes
        // available for AF stuffing + payload. payload is 50; stuffing is
        // 182 - 50 = 132. af_length = 1 (flags) + 132 (stuffing) = 133.
        assert_eq!(buf[4], 133);
        // Flags byte: 0x00 — no flags set.
        assert_eq!(buf[5], 0x00);
        // 132 bytes of 0xFF stuffing.
        for &b in &buf[6..6 + 132] {
            assert_eq!(b, 0xFF);
        }
        // Then payload starting at byte 138.
        for &b in &buf[138..] {
            assert_eq!(b, 0xAA);
        }
    }

    #[test]
    fn stuffing_zero_length_af_no_flags_byte() {
        // Edge case: when payload is exactly 183 bytes, the stuffing path
        // emits a zero-length AF (just the length byte = 0, no flags byte,
        // no stuffing). ISO/IEC 13818-1 §2.4.3.4 explicitly allows this —
        // adaptation_field_length=0 means the AF consists of the length
        // byte alone.
        let mut cc = ContinuityCounters::new();
        let payload = vec![0xAA; 183];
        let mut buf = [0u8; 188];
        write_packet(
            &mut buf,
            0x1011,
            true,
            AdaptationField::default(),
            &payload,
            &mut cc,
        );
        assert_eq!(buf[3] >> 4, 0b11);
        // af_length = 0 — no flags, no stuffing, payload starts at byte 5.
        assert_eq!(buf[4], 0);
        for &b in &buf[5..] {
            assert_eq!(b, 0xAA);
        }
    }

    /// PCR-only heartbeat must repeat the last payload CC.
    ///
    /// H.222.0 §2.4.3.3: the continuity_counter is NOT incremented on
    /// adaptation-field-only packets (afc='10'). The heartbeat must carry the
    /// same CC as the previous payload packet on the same PID.
    #[test]
    fn pcr_only_heartbeat_repeats_last_payload_cc() {
        let mut cc = ContinuityCounters::new();
        let payload = [0xBBu8; 50];
        let mut buf = [0u8; 188];
        let pcr = Pcr27mhz::new(27_000_000);

        // Emit 3 payload packets on PID 0x200 so the CC cycles to 2.
        for _ in 0..3 {
            write_packet(
                &mut buf,
                0x200,
                false,
                AdaptationField::default(),
                &payload,
                &mut cc,
            );
        }
        // The last emitted payload CC was 2 (0, 1, 2 in order).
        assert_eq!(buf[3] & 0x0F, 2, "last payload CC should be 2");

        // A PCR-only heartbeat on the same PID must repeat CC = 2.
        let mut hb = [0u8; 188];
        write_pcr_only_packet(&mut hb, 0x200, pcr, &cc);
        // afc = 0b10 (adaptation-only, no payload)
        assert_eq!(hb[3] >> 4, 0b10, "afc must be adaptation-only (0b10)");
        assert_eq!(
            hb[3] & 0x0F,
            2,
            "heartbeat must repeat the last payload CC (2), not the next (3)"
        );

        // A second consecutive heartbeat must also repeat CC = 2 (non-mutating).
        let mut hb2 = [0u8; 188];
        write_pcr_only_packet(&mut hb2, 0x200, pcr, &cc);
        assert_eq!(
            hb2[3] & 0x0F,
            2,
            "consecutive heartbeats must all repeat same CC"
        );
    }
}
