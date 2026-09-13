//! Deliberately naive raw MPEG-TS reader for the wire-level oracles.
//!
//! Independence is the point: `verify.rs`'s tally comes from tst-core's
//! demuxer, i.e. the code under test. This reader shares nothing with it —
//! it parses exactly the constructs this crate's generator emits (188-byte
//! packets, single-packet PAT/PMT, adaptation-field PCR, PES headers) and
//! returns an error for anything else, so it can never quietly degrade into
//! the demuxer's view of a stream. Spec references: ITU-T H.222.0 §2.4.3.2
//! (packet header), §2.4.3.5 (adaptation field / PCR), §2.4.4.3-.8 (PAT /
//! PMT), §2.4.3.7 (PES header / PTS).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

const PKT: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
    /// `format_identifier` of a registration_descriptor (tag 0x05), if any.
    pub registration: Option<[u8; 4]>,
    pub descriptor_tags: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PesShape {
    pub stream_ids: BTreeSet<u8>,
    /// First 4 payload bytes after the PES header of the FIRST PES seen.
    pub first_payload_prefix: Option<[u8; 4]>,
}

/// Per-packet header facts, decoded statelessly — the classification the
/// corruption tap (`corrupt.rs`) and the tee coordinate need without
/// owning a `Reader`. Mirrors exactly the header parsing `Reader::packet`
/// does (§2.4.3.2 / §2.4.3.5); kept as one function so the two can never
/// disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub pid: u16,
    pub pusi: bool,
    /// adaptation_field_control (2 bits).
    pub afc: u8,
    pub has_payload: bool,
    /// adaptation_field_length, 0 when there is no adaptation field.
    pub af_len: usize,
    /// Byte offset of the first payload byte (188 when there is none).
    pub payload_off: usize,
    pub pcr_base: Option<u64>,
    pub cc: u8,
}

pub fn classify_packet(p: &[u8; PKT]) -> Result<PacketInfo, String> {
    if p[0] != SYNC {
        return Err(format!("sync byte 0x{:02x}, want 0x47", p[0]));
    }
    let pusi = p[1] & 0x40 != 0;
    let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
    let afc = (p[3] >> 4) & 0x3;
    let cc = p[3] & 0x0F;
    let mut af_len = 0;
    let mut pcr_base = None;
    let mut off = 4;
    if afc & 0x2 != 0 {
        af_len = usize::from(p[4]);
        if 5 + af_len > PKT {
            return Err(format!(
                "adaptation field length {af_len} does not fit in a {PKT}-byte packet"
            ));
        }
        if af_len > 0 && p[5] & 0x10 != 0 {
            // The PCR flag claims 6 bytes (program_clock_reference_
            // base + _extension, §2.4.3.5) follow the flags byte, so
            // the adaptation field must be at least 1 (flags) + 6
            // bytes long. A PCR flag on a shorter adaptation field is
            // malformed — reading `p[6..12]` anyway would silently
            // read bytes outside the declared field (padding, or the
            // next field entirely) as if they were PCR.
            if af_len < 7 {
                return Err(format!(
                    "adaptation field: PCR flag set but adaptation_field_length {af_len} < 7"
                ));
            }
            // program_clock_reference_base: 33 bits, §2.4.3.5.
            let b = &p[6..12];
            pcr_base = Some(
                (u64::from(b[0]) << 25)
                    | (u64::from(b[1]) << 17)
                    | (u64::from(b[2]) << 9)
                    | (u64::from(b[3]) << 1)
                    | u64::from(b[4] >> 7),
            );
        }
        off = 5 + af_len;
    }
    let has_payload = afc & 0x1 != 0 && off < PKT;
    Ok(PacketInfo {
        pid,
        pusi,
        afc,
        has_payload,
        af_len,
        payload_off: if has_payload { off } else { PKT },
        pcr_base,
        cc,
    })
}

/// One sync-recovery event recorded by a [`Reader`] in resync mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resync {
    /// `Reader::packets()` at the moment the hunt started.
    pub at_packets: u64,
    pub pcr_base: Option<u64>,
    pub skipped_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WireSummary {
    pub programs: BTreeMap<u16, Program>,
    pub pcr: BTreeMap<u16, Vec<u64>>,
    pub pts: BTreeMap<u16, Vec<u64>>,
    pub pes: BTreeMap<u16, PesShape>,
    pub packets_per_pid: BTreeMap<u16, u64>,
    pub packets: u64,
}

pub struct Reader {
    summary: WireSummary,
    /// PMT PID -> program_number, learned from the PAT.
    pmt_pids: BTreeMap<u16, u16>,
    carry: Vec<u8>,
    resync_mode: bool,
    resyncs: Vec<Resync>,
    last_pcr: Option<u64>,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    pub fn new() -> Self {
        Self {
            summary: WireSummary::default(),
            pmt_pids: BTreeMap::new(),
            carry: Vec::new(),
            resync_mode: false,
            resyncs: Vec::new(),
            last_pcr: None,
        }
    }

    pub fn packets(&self) -> u64 {
        self.summary.packets
    }

    pub fn last_pcr(&self) -> Option<u64> {
        self.last_pcr
    }

    pub fn is_pmt_pid(&self, pid: u16) -> bool {
        self.pmt_pids.contains_key(&pid)
    }

    pub fn set_resync_mode(&mut self, on: bool) {
        self.resync_mode = on;
    }

    pub fn resyncs(&self) -> &[Resync] {
        &self.resyncs
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.carry.extend_from_slice(bytes);
        let mut off = 0;
        while off + PKT <= self.carry.len() {
            if self.resync_mode {
                let ahead_ok = off + PKT >= self.carry.len() || self.carry[off + PKT] == SYNC;
                if self.carry[off] != SYNC || !ahead_ok {
                    // `off` is either garbage outright, or a sync byte whose
                    // claimed 188-byte span runs into a byte that is not a
                    // sync byte — typically because an earlier packet lost
                    // bytes (truncation), so every packet after it now
                    // "starts" 188 bytes too early. Either way we cannot
                    // trust reading a whole packet from `off` without first
                    // checking whether a genuinely confirmed packet start
                    // sits CLOSER than `off + PKT`: if one does, `off` was
                    // itself bogus (a splice of the truncated packet's real
                    // header with the next real packet's leading bytes) and
                    // must be abandoned too, not merely skipped past.
                    let start = off;
                    let mut k = off + 1;
                    let mut found = None;
                    while k + PKT <= self.carry.len() {
                        if self.carry[k] == SYNC
                            && (k + PKT >= self.carry.len() || self.carry[k + PKT] == SYNC)
                        {
                            found = Some(k);
                            break;
                        }
                        k += 1;
                    }
                    match found {
                        // `off` itself was never trustworthy (not even a
                        // sync byte), or the nearest confirmed packet start
                        // sits inside `off`'s own claimed 188-byte span:
                        // abandon `off` and resync onto it.
                        Some(k) if self.carry[off] != SYNC || k < off + PKT => {
                            self.resyncs.push(Resync {
                                at_packets: self.summary.packets,
                                pcr_base: self.last_pcr,
                                skipped_bytes: k - start,
                            });
                            off = k;
                            continue;
                        }
                        // `off` is a sync byte and nothing closer contradicts
                        // it — any corruption starts cleanly at `off + PKT`
                        // (e.g. inserted garbage) and the next iteration's
                        // own check finds it there.
                        Some(_) => {}
                        None => {
                            if self.carry[off] != SYNC {
                                break; // keep the tail in carry; decide on the next feed
                            }
                            // Not enough buffered data yet to confirm or
                            // refute `off` — accept it optimistically,
                            // matching the hunt's own "or is the last whole
                            // packet in the carry" leniency.
                        }
                    }
                }
            }
            let pkt: [u8; PKT] = self.carry[off..off + PKT].try_into().expect("PKT bytes");
            self.packet(&pkt)
                .map_err(|e| format!("packet {}: {e}", self.summary.packets))?;
            off += PKT;
        }
        self.carry.drain(..off);
        Ok(())
    }

    /// Consume the reader and return the accumulated [`WireSummary`] —
    /// `Err` iff a trailing partial packet (fewer than 188 bytes) is
    /// still sitting in the carry, naming how many bytes short it is. A
    /// well-formed capture is always a whole number of TS packets; a
    /// trailing fragment means the capture was cut off mid-packet
    /// (e.g. a live recv session closing between transport reads), and
    /// callers surface that as an explicit failure rather than silently
    /// discarding it. In resync mode, that same trailing fragment is
    /// instead recorded as one final [`Resync`] — the whole point of
    /// resync mode is to keep going through corruption/truncation rather
    /// than fail the feed.
    pub fn finish(mut self) -> Result<WireSummary, String> {
        if !self.carry.is_empty() {
            if self.resync_mode {
                self.resyncs.push(Resync {
                    at_packets: self.summary.packets,
                    pcr_base: self.last_pcr,
                    skipped_bytes: self.carry.len(),
                });
            } else {
                return Err(format!(
                    "{} trailing byte(s) short of a {PKT}-byte packet",
                    self.carry.len()
                ));
            }
        }
        Ok(self.summary)
    }

    /// Clear the in-flight byte carry (a partial trailing packet left
    /// over from whatever connection just broke) without discarding the
    /// accumulated [`WireSummary`] or the PAT-learned PMT-PID map. A
    /// managed reconnect's replacement transport starts delivering bytes
    /// at a fresh packet boundary of its own — bytes still sitting in
    /// `carry` from the connection that just died can never be validly
    /// completed by bytes from the new one (they aren't even
    /// guaranteed to be the same TS multiplex position), so feeding them
    /// together would corrupt packet sync rather than merely delay it.
    /// Call this once per reconnect, before the replacement transport's
    /// first `feed`.
    pub fn resync(&mut self) {
        self.carry.clear();
    }

    fn packet(&mut self, p: &[u8; PKT]) -> Result<(), String> {
        let info = classify_packet(p)?;
        self.summary.packets += 1;
        *self.summary.packets_per_pid.entry(info.pid).or_insert(0) += 1;
        if let Some(base) = info.pcr_base {
            self.summary.pcr.entry(info.pid).or_default().push(base);
            self.last_pcr = Some(base);
        }
        if !info.has_payload {
            return Ok(());
        }
        let payload = &p[info.payload_off..];
        if info.pid == PAT_PID {
            if info.pusi {
                self.pat(payload)?;
            }
            return Ok(());
        }
        if let Some(&program_number) = self.pmt_pids.get(&info.pid) {
            if info.pusi {
                self.pmt(info.pid, program_number, payload)?;
            }
            return Ok(());
        }
        if info.pusi && payload.len() >= 9 && payload[..3] == [0, 0, 1] {
            self.pes(info.pid, payload)?;
        }
        Ok(())
    }

    /// One PSI section starting at a pointer field; single-packet
    /// sections only. `body` (once returned) is always exactly `len`
    /// bytes and always at least 9 (`table_id_ext(2) + version/
    /// current_next(1) + section_number(1) + last_section_number(1) +
    /// CRC32(4)` — the minimum shape of ANY PSI section, PAT or PMT,
    /// even with zero loop entries), so callers can rely on indices
    /// `0..9` existing without their own length check.
    fn section(payload: &[u8]) -> Result<(u8, &[u8]), String> {
        const SECTION_HEADER_MIN: usize = 9;
        let &ptr_byte = payload.first().ok_or("PSI section: empty payload")?;
        let sec = payload
            .get(1 + usize::from(ptr_byte)..)
            .ok_or("PSI section: pointer field past packet")?;
        if sec.len() < 3 {
            return Err(format!(
                "PSI section: {} header byte(s) after the pointer field, want >= 3",
                sec.len()
            ));
        }
        let table_id = sec[0];
        let len = (usize::from(sec[1] & 0x0F) << 8) | usize::from(sec[2]);
        if len < SECTION_HEADER_MIN {
            return Err(format!(
                "PSI section_length {len}, want >= {SECTION_HEADER_MIN} (table_id_ext + \
                 version/current_next + section_number + last_section_number + CRC32)"
            ));
        }
        let body = sec
            .get(3..3 + len)
            .ok_or("PSI section spans packets (unsupported)")?;
        // body = [table_id_ext(2), ver/cni(1), sec_num(1), last_sec(1), ..., crc(4)]
        Ok((table_id, body))
    }

    fn pat(&mut self, payload: &[u8]) -> Result<(), String> {
        let (tid, body) = Self::section(payload)?;
        if tid != 0 {
            return Err(format!("PAT table_id 0x{tid:02x}"));
        }
        // Redundant with `section()`'s own >= 9 minimum today (a PAT's
        // minimum shape IS the generic PSI minimum: 5 header bytes + 4
        // CRC bytes, zero program entries), but named explicitly so a
        // PAT-shaped error survives independently of that shared check.
        if body.len() < 5 + 4 {
            return Err(format!(
                "PAT section {} byte(s), want >= 9 (5 header + CRC32)",
                body.len()
            ));
        }
        let loop_bytes = &body[5..body.len() - 4];
        for e in loop_bytes.chunks_exact(4) {
            let program_number = (u16::from(e[0]) << 8) | u16::from(e[1]);
            let pmt_pid = (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]);
            if program_number != 0 {
                self.pmt_pids.insert(pmt_pid, program_number);
            }
        }
        Ok(())
    }

    fn pmt(&mut self, pmt_pid: u16, program_number: u16, payload: &[u8]) -> Result<(), String> {
        let (tid, body) = Self::section(payload)?;
        if tid != 2 {
            return Err(format!("PMT table_id 0x{tid:02x}"));
        }
        // `body[5..9]` (PCR_PID + program_info_length) is covered by
        // `section()`'s own >= 9 minimum; `info_len` is only known AFTER
        // reading those bytes, so the program-info + ES loop + CRC space
        // needs its own check here — a PMT declaring more program-info
        // bytes than the section actually has room for must not silently
        // slice into (or past) the CRC.
        let pcr_pid = (u16::from(body[5] & 0x1F) << 8) | u16::from(body[6]);
        let info_len = (usize::from(body[7] & 0x0F) << 8) | usize::from(body[8]);
        let min_len = 9 + info_len + 4;
        if body.len() < min_len {
            return Err(format!(
                "PMT section {} byte(s), want >= {min_len} (5 header + pcr_pid + \
                 program_info_length + {info_len} program_info byte(s) + CRC32)",
                body.len()
            ));
        }
        let es = &body[9 + info_len..body.len() - 4];
        let mut streams = Vec::new();
        let mut k = 0;
        while k + 5 <= es.len() {
            let stream_type = es[k];
            let pid = (u16::from(es[k + 1] & 0x1F) << 8) | u16::from(es[k + 2]);
            let es_info_len = (usize::from(es[k + 3] & 0x0F) << 8) | usize::from(es[k + 4]);
            let desc = es
                .get(k + 5..k + 5 + es_info_len)
                .ok_or("ES info past section")?;
            let mut registration = None;
            let mut descriptor_tags = Vec::new();
            let mut m = 0;
            while m + 2 <= desc.len() {
                let tag = desc[m];
                let len = usize::from(desc[m + 1]);
                let data = desc
                    .get(m + 2..m + 2 + len)
                    .ok_or("descriptor past ES info")?;
                descriptor_tags.push(tag);
                if tag == 0x05 && data.len() >= 4 {
                    registration = Some([data[0], data[1], data[2], data[3]]);
                }
                m += 2 + len;
            }
            streams.push(Stream {
                pid,
                stream_type,
                registration,
                descriptor_tags,
            });
            k += 5 + es_info_len;
        }
        self.summary.programs.insert(
            program_number,
            Program {
                program_number,
                pmt_pid,
                pcr_pid,
                streams,
            },
        );
        Ok(())
    }

    fn pes(&mut self, pid: u16, payload: &[u8]) -> Result<(), String> {
        let stream_id = payload[3];
        let shape = self.summary.pes.entry(pid).or_default();
        shape.stream_ids.insert(stream_id);
        // §2.4.3.7: byte 6 = '10' + flags, byte 7 = PTS_DTS_flags.., byte 8 = header_data_length.
        let pts_dts = payload[7] >> 6;
        let hdr_len = usize::from(payload[8]);
        if pts_dts & 0x2 != 0 {
            let q = payload.get(9..14).ok_or("PTS past packet")?;
            let pts = (u64::from((q[0] >> 1) & 0x7) << 30)
                | (u64::from(q[1]) << 22)
                | (u64::from((q[2] >> 1) & 0x7F) << 15)
                | (u64::from(q[3]) << 7)
                | u64::from(q[4] >> 1);
            self.summary.pts.entry(pid).or_default().push(pts);
        }
        if shape.first_payload_prefix.is_none() {
            if let Some(d) = payload.get(9 + hdr_len..9 + hdr_len + 4) {
                shape.first_payload_prefix = Some([d[0], d[1], d[2], d[3]]);
            }
        }
        Ok(())
    }
}

pub fn summarize_file(path: &Path) -> io::Result<WireSummary> {
    let bytes = std::fs::read(path)?;
    let mut r = Reader::new();
    r.feed(&bytes).map_err(io::Error::other)?;
    r.finish().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn summary_of(profile: &str, seconds: f64) -> WireSummary {
        let p = crate::profiles::by_name(profile).unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-{profile}-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(p, seconds, &path).unwrap();
        let s = summarize_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        s
    }

    #[test]
    fn baseline_pmt_and_pes_shapes_match_the_measured_wire() {
        let s = summary_of("baseline", 3.0);
        let prog = &s.programs[&1];
        assert_eq!(prog.pmt_pid, 0x1000);
        assert_eq!(prog.pcr_pid, 0x1011);
        let video = prog.streams.iter().find(|st| st.pid == 0x1011).unwrap();
        assert_eq!(video.stream_type, 0x1B);
        assert_eq!(video.registration, None);
        let klv = prog.streams.iter().find(|st| st.pid == 0x1031).unwrap();
        assert_eq!(klv.stream_type, 0x06);
        assert_eq!(klv.registration, Some(*b"KLVA"));
        assert_eq!(s.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
        assert_eq!(s.pes[&0x1011].first_payload_prefix, Some([0, 0, 0, 1]));
        assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xBD]));
        assert_eq!(s.pts[&0x1011].len(), 90);
        assert_eq!(s.pcr[&0x1011].len(), 45);
        assert_eq!(s.packets, 180);
    }

    #[test]
    fn klv_sync_pmt_carries_0x15_with_metadata_descriptors() {
        let s = summary_of("klv-sync", 3.0);
        let klv = s.programs[&1]
            .streams
            .iter()
            .find(|st| st.pid == 0x1031)
            .unwrap();
        assert_eq!(klv.stream_type, 0x15);
        assert_eq!(klv.registration, Some(*b"KLVA"));
        assert!(klv.descriptor_tags.contains(&0x26) && klv.descriptor_tags.contains(&0x27));
        assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xFC]));
    }

    #[test]
    fn av1_modes_differ_only_in_pes_stream_id_and_prefix() {
        let a = summary_of("av1-klv-a", 3.0);
        let b = summary_of("av1-klv-b", 3.0);
        assert_eq!(
            a.programs[&1]
                .streams
                .iter()
                .map(|s| (s.pid, s.stream_type, s.registration))
                .collect::<Vec<_>>(),
            b.programs[&1]
                .streams
                .iter()
                .map(|s| (s.pid, s.stream_type, s.registration))
                .collect::<Vec<_>>()
        );
        assert_eq!(a.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
        assert_eq!(b.pes[&0x1011].stream_ids, BTreeSet::from([0xBD]));
        assert_eq!(
            &b.pes[&0x1011].first_payload_prefix.unwrap()[..3],
            &[0, 0, 1]
        );
        assert_ne!(
            &a.pes[&0x1011].first_payload_prefix.unwrap()[..3],
            &[0, 0, 1]
        );
    }

    #[test]
    fn two_program_summary_has_both_programs_with_distinct_pids() {
        let s = summary_of("two-program", 3.0);
        assert_eq!(s.programs.len(), 2);
        assert_eq!(s.programs[&2].pmt_pid, 0x1100);
        assert!(s.packets_per_pid[&0x1111] > 0 && s.packets_per_pid[&0x1131] > 0);
    }

    #[test]
    fn pts_rollover_stream_shows_one_raw_pts_decrease_at_seven_seconds() {
        let s = summary_of("pts-rollover", 7.0);
        let v = &s.pts[&0x1011];
        let decreases = v.windows(2).filter(|w| w[1] < w[0]).count();
        assert_eq!(decreases, 1);
    }

    #[test]
    fn feed_tolerates_arbitrary_chunking_and_rejects_sync_loss() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-chunk-{}.ts", std::process::id()));
        crate::r#gen::run(p, 2.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        for chunk in bytes.chunks(101) {
            r.feed(chunk).unwrap();
        }
        assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
        let mut bad = bytes.clone();
        bad[188 * 5] = 0x00;
        let mut r = Reader::new();
        let e = r.feed(&bad).unwrap_err();
        assert!(e.contains("sync"), "{e}");
    }

    #[test]
    fn resync_discards_a_stale_partial_packet_without_losing_the_summary() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-resync-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(p, 2.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let mut r = Reader::new();
        // A partial packet (100 bytes < 188) left over from a connection
        // that just broke — never completes on its own.
        r.feed(&bytes[..100]).unwrap();
        r.resync();
        // A whole, independent valid stream from the "reconnected"
        // transport must feed clean (no sync-loss error) and its packet
        // count must reflect only THIS stream — the discarded partial
        // packet contributed nothing.
        r.feed(&bytes).unwrap();
        assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
    }

    #[test]
    fn finish_rejects_a_trailing_partial_packet() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-trailing-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(p, 2.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let cut = 3 * PKT + 100;
        assert!(bytes.len() > cut, "fixture too short for this test");

        let mut r = Reader::new();
        r.feed(&bytes[..cut]).unwrap();
        let e = r.finish().unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn feed_rejects_a_pat_pointer_field_past_the_payload() {
        // A minimal PID-0 (PAT) packet, PUSI set, adaptation_field_control
        // = payload-only (afc=01 -> byte 3 = 0x10), pointer_field = 255 —
        // nowhere near valid for a 184-byte payload, so `section()` must
        // reject it rather than reading (or panicking on) bytes past the
        // packet.
        let mut pkt = [0xFFu8; PKT];
        pkt[0] = SYNC;
        pkt[1] = 0x40; // PUSI set, PID high bits = 0
        pkt[2] = 0x00; // PID low byte = 0 (PAT)
        pkt[3] = 0x10; // afc = payload only
        pkt[4] = 0xFF; // pointer_field = 255

        let mut r = Reader::new();
        let e = r.feed(&pkt).unwrap_err();
        assert!(e.contains("pointer field"), "{e}");
    }

    #[test]
    fn feed_rejects_a_pcr_flag_with_a_too_short_adaptation_field() {
        // adaptation_field_control = adaptation-field-only (afc=10 ->
        // byte 3 = 0x20), adaptation_field_length = 3 (too short to hold
        // the flags byte + a 6-byte PCR), PCR flag set anyway — malformed,
        // must error rather than reading past the declared field.
        let mut pkt = [0xFFu8; PKT];
        pkt[0] = SYNC;
        pkt[1] = 0x00;
        pkt[2] = 0x11; // an arbitrary non-PAT/PMT PID
        pkt[3] = 0x20; // afc = adaptation field only
        pkt[4] = 3; // adaptation_field_length
        pkt[5] = 0x10; // PCR flag set

        let mut r = Reader::new();
        let e = r.feed(&pkt).unwrap_err();
        assert!(e.contains('7'), "{e}");
    }

    #[test]
    fn classify_packet_reports_pid_pusi_pcr_and_payload_offset() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-cls-{}.ts", std::process::id()));
        crate::r#gen::run(p, 1.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let first: [u8; PKT] = bytes[..PKT].try_into().unwrap();
        let info = classify_packet(&first).unwrap();
        assert_eq!(info.pid, 0, "first packet of a fresh mux is the PAT");
        assert!(info.pusi);
        assert!(info.has_payload);
        assert_eq!(info.payload_off, 4);
        // Find a PCR-bearing packet and check the base decodes.
        let pcr_pkt = bytes
            .chunks_exact(PKT)
            .find(|c| c[3] & 0x20 != 0 && c[4] > 0 && c[5] & 0x10 != 0)
            .unwrap();
        let info = classify_packet(pcr_pkt.try_into().unwrap()).unwrap();
        assert!(info.pcr_base.is_some());
        assert_eq!(info.payload_off, 5 + usize::from(pcr_pkt[4]));
        let mut bad = first;
        bad[0] = 0x00;
        assert!(classify_packet(&bad).unwrap_err().contains("sync"));
    }

    #[test]
    fn reader_exposes_packet_count_last_pcr_and_pmt_pids() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-acc-{}.ts", std::process::id()));
        crate::r#gen::run(p, 1.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        assert_eq!(r.packets(), 0);
        assert_eq!(r.last_pcr(), None);
        r.feed(&bytes).unwrap();
        assert_eq!(r.packets() as usize, bytes.len() / PKT);
        assert!(r.last_pcr().is_some());
        assert!(r.is_pmt_pid(0x1000));
        assert!(!r.is_pmt_pid(0x1011));
    }

    #[test]
    fn resync_mode_hunts_forward_and_records_each_resync() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-hunt-{}.ts", std::process::id()));
        crate::r#gen::run(p, 2.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // Truncate packet 5 to 100 bytes and insert 37 garbage bytes after packet 20.
        let mut bad = Vec::new();
        for (i, pkt) in bytes.chunks_exact(PKT).enumerate() {
            if i == 5 {
                bad.extend_from_slice(&pkt[..100]);
            } else {
                bad.extend_from_slice(pkt);
            }
            if i == 20 {
                bad.extend(std::iter::repeat(0x55u8).take(37));
            }
        }
        let mut r = Reader::new();
        r.set_resync_mode(true);
        for chunk in bad.chunks(1316) {
            r.feed(chunk).unwrap();
        }
        assert_eq!(r.resyncs().len(), 2, "{:?}", r.resyncs());
        assert_eq!(r.resyncs()[0].at_packets, 5);
        assert_eq!(r.resyncs()[0].skipped_bytes, 100);
        assert_eq!(r.resyncs()[1].at_packets, 20);
        assert_eq!(r.resyncs()[1].skipped_bytes, 37);
        // Every other packet was accepted.
        let s = r.finish().unwrap();
        assert_eq!(s.packets as usize, bytes.len() / PKT - 1);
    }

    #[test]
    fn resync_mode_treats_a_trailing_partial_packet_as_a_resync_not_an_error() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-tail-{}.ts", std::process::id()));
        crate::r#gen::run(p, 1.0, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        r.set_resync_mode(true);
        r.feed(&bytes[..3 * PKT + 50]).unwrap();
        let s = r.finish().unwrap();
        assert_eq!(s.packets, 3);
    }
}
