use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tst_core::transport::{SocketStats, Transport, TransportCancel, TransportError};

use crate::impair::XorShift64;
use crate::rawts::{Reader, classify_packet};

use super::coord::{Coord, Injection, LogHeader, LogLine, TAP_VERSION};

pub(super) const PKT: usize = 188;

/// Mixed into the run seed so the tap's PRNG stream is independent of every
/// other seeded component that shares the same run seed (the impairment
/// proxy's `Engine`, the generator's AU sizes). Without a per-component
/// salt, two components seeded identically would draw the same numbers and
/// their "independent" decisions would correlate.
pub const CORRUPT_SALT: u64 = 0xC0FF_EE00_0BAD_F00D;

/// Where [`Class::Header`]'s PID-rewrite sub-kind moves a packet: unassigned,
/// and deliberately not 0x1FFF (the null PID), which a demuxer silently
/// discards instead of reporting. The attribution engine reads it back
/// through `Tracked::moved_to`: whatever a receiver says about the moved
/// packet it says on THIS PID, not on the one the log names.
pub const REWRITTEN_PID: u16 = 0x1FFE;

/// How many receiver packets after a resolved injection an error event may
/// surface and still be blamed on it (spec §4.3), at AU scale 1. Wide
/// enough to cover a demuxer that only notices at the next PES/section
/// boundary, narrow enough that an unrelated event a second later is not
/// silently excused.
///
/// Every packet-count constant here (this, [`RECOVERY_BOUND`],
/// [`DEFAULT_MIN_GAP`], [`APPROX_SLACK`]) was calibrated against the
/// realistic fixtures at `--au-scale 1`: a keyframe spans ~290 packets, a
/// PCR interval ~110, a KLV record arrives every ~450. The events they
/// bound — the next PES boundary, the next PCR, the next packet on a PID —
/// are fixed in TIME, so at `--au-scale F` they sit F× further apart in
/// packets. [`CorruptConfig::geometry_scale`] multiplies all four by F so
/// the tap judges the same stream the same way at every scale; the
/// scaled values travel in the log header and the receiver reads them
/// from there. The first measured stress run (2026-10-02) ended every
/// bitrate axis at scale 4 on exactly this: a `body_flip` inside a 1158-
/// packet keyframe cannot be "recovered within 600 packets".
pub const ATTRIBUTION_WINDOW: u64 = 500;

/// How many receiver packets after a resolved injection the stream must
/// have produced media again for the injection to count as recovered-from
/// (spec §4.3), at AU scale 1 (≈ 2× the packet span of a realistic
/// keyframe — see [`ATTRIBUTION_WINDOW`] for the scaling rule).
pub const RECOVERY_BOUND: u64 = 600;

/// Default `min_gap` at AU scale 1 — the floor on the packet distance
/// between two injections. Keeping injections farther apart than
/// [`ATTRIBUTION_WINDOW`] + [`RECOVERY_BOUND`] is what makes attribution
/// unambiguous: at most one injection's window can ever contain a given
/// event. Scales with the geometry (see [`ATTRIBUTION_WINDOW`]).
pub const DEFAULT_MIN_GAP: u64 = 1000;

/// Extra attribution slack granted to an injection whose PCR base the
/// receiver never saw, so it resolved against the *next* base instead. The
/// error is then bounded by one PCR interval; 128 packets is comfortably
/// more than the ≤100 ms interval H.222.0 §2.4.2.2 allows at AU scale 1
/// (the receiver takes the scaled value from the log header's
/// `approx_slack`; this constant is what pre-scaling logs read as).
pub const APPROX_SLACK: u64 = 128;

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
    /// The sender's `--au-scale`: multiplies every packet-count constant
    /// of the attribution geometry (see [`ATTRIBUTION_WINDOW`]). `1` is
    /// the soak's shape and leaves every number as it always was.
    pub geometry_scale: u32,
}

impl CorruptConfig {
    /// [`ATTRIBUTION_WINDOW`] at this config's geometry scale.
    pub fn attribution_window(&self) -> u64 {
        ATTRIBUTION_WINDOW * u64::from(self.geometry_scale)
    }

    /// [`RECOVERY_BOUND`] at this config's geometry scale.
    pub fn recovery_bound(&self) -> u64 {
        RECOVERY_BOUND * u64::from(self.geometry_scale)
    }

    /// [`APPROX_SLACK`] at this config's geometry scale.
    pub fn approx_slack(&self) -> u64 {
        APPROX_SLACK * u64::from(self.geometry_scale)
    }

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
        if self.geometry_scale == 0 {
            return Err("--corrupt: the geometry scale (--au-scale) must be >= 1".into());
        }
        if self.min_gap < 2 * self.attribution_window() || self.min_gap <= self.recovery_bound() {
            return Err(format!(
                "--corrupt: min_gap={} is too small; it must be >= {} (2 * attribution window) \
                 and > {} (recovery bound) so at most one injection can explain any event \
                 (both scaled by --au-scale {})",
                self.min_gap,
                2 * self.attribution_window(),
                self.recovery_bound(),
                self.geometry_scale
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
///
/// `geometry_scale` is the sender's `--au-scale`: an omitted `min_gap`
/// defaults to [`DEFAULT_MIN_GAP`] × scale, and an explicit one is
/// validated against the scaled window and recovery bound (see
/// [`ATTRIBUTION_WINDOW`]). `rate` is NOT scaled: it is per packet, so a
/// stream carrying F× the packets takes F× the injections per second —
/// more corruption stress at higher bitrates, which is the point of the
/// bitrate axis — and the scaled `min_gap` keeps each one judgeable.
pub fn parse_corrupt(s: &str, seed: u64, geometry_scale: u32) -> Result<CorruptConfig, String> {
    let mut rate = None;
    let mut min_gap = DEFAULT_MIN_GAP * u64::from(geometry_scale);
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
        geometry_scale,
    };
    cfg.validate()?;
    Ok(cfg)
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
    /// A log line could not be written or flushed. Latched for the run:
    /// the missing injection may be one the receiver had no obligation
    /// to notice (a `Dup`), so nothing downstream would otherwise reveal
    /// that the evidence is incomplete. `report soak` fails
    /// `corruption_coverage_<leg>` on it regardless of the ingestion
    /// floor. `#[serde(default)]` so archived send reports still parse.
    #[serde(default)]
    pub log_write_failed: bool,
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
            attribution_window: me.cfg.attribution_window(),
            recovery_bound: me.cfg.recovery_bound(),
            approx_slack: me.cfg.approx_slack(),
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
                        // REWRITTEN_PID: unassigned, and deliberately not
                        // 0x1FFF (the null PID), which a demuxer would
                        // silently discard instead of reporting.
                        p[1] = (p[1] & 0xE0) | (REWRITTEN_PID >> 8) as u8;
                        p[2] = (REWRITTEN_PID & 0xFF) as u8;
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
                // the run — but it must not be silent either. A missing
                // DETECTABLE injection surfaces later as an unexplained
                // event; a missing `Dup` or body flip never would, so the
                // stats carry the failure for `report soak` to fail on.
                stats.log_write_failed = true;
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
pub(super) fn pes_start(p: &[u8; PKT], info: &crate::rawts::PacketInfo) -> bool {
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
pub(super) fn sensitive_span(
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
pub(super) fn psi_section_body_start(
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
        if let Err(e) = self.log.flush() {
            // Same latch as the per-line write arm: a
            // buffering writer can hold a whole line until this final
            // flush, so a failure HERE loses evidence exactly like a
            // failure there — and a lost `Dup` line is one the receiver
            // had no obligation to notice, so nothing downstream would
            // otherwise reveal that the log is incomplete.
            self.stats
                .lock()
                .expect("corruption stats mutex")
                .log_write_failed = true;
            tracing::error!("corruption log flush failed at close: {e}");
        }
        self.inner.close();
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        self.inner.cancel_handle()
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        self.inner.socket_stats()
    }
}
