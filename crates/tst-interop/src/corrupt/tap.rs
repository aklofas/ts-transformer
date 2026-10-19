use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tst_core::transport::{SocketStats, Transport, TransportCancel, TransportError};

use crate::impair::XorShift64;
use tst_test_helpers::rawts::{Reader, classify_packet};

use super::coord::{Coord, Injection, LogHeader, LogLine, TAP_VERSION};

const PKT: usize = 188;

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
        info: &tst_test_helpers::rawts::PacketInfo,
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
            msg.len().is_multiple_of(PKT),
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
fn pes_start(p: &[u8; PKT], info: &tst_test_helpers::rawts::PacketInfo) -> bool {
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
    info: &tst_test_helpers::rawts::PacketInfo,
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
    info: &tst_test_helpers::rawts::PacketInfo,
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
fn psi_body_range(
    p: &[u8; PKT],
    info: &tst_test_helpers::rawts::PacketInfo,
) -> Option<(usize, usize)> {
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::corrupt::testing;
    use crate::corrupt::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tst_core::transport::{Transport, TransportError};

    /// In-memory transport that records every `send_bytes` payload
    /// separately — the per-push view the splitting test needs.
    pub(in crate::corrupt) struct Capture(Arc<Mutex<Vec<Vec<u8>>>>);
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

    pub(in crate::corrupt) fn baseline_bytes(seconds: f64, tag: &str) -> Vec<u8> {
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
    pub(in crate::corrupt) fn run_tap(
        bytes: &[u8],
        cfg: CorruptConfig,
    ) -> (Vec<u8>, String, CorruptionStats) {
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

    pub(in crate::corrupt) fn cfg(classes: &[Class], rate: u32, min_gap: u64) -> CorruptConfig {
        CorruptConfig {
            rate_per_10k: rate,
            min_gap,
            classes: classes.to_vec(),
            seed: 7,
            geometry_scale: 1,
        }
    }

    /// A log writer that refuses exactly one whole line: the `nth`
    /// JSON write (1 = the header, 2 = the first injection). `write_line`
    /// writes the JSON then the newline, so failing the JSON write drops
    /// the whole line cleanly — no torn line, no trace in the file.
    struct FailOneLine {
        inner: testing::VecWriter,
        nth: usize,
        lines: usize,
    }
    impl Write for FailOneLine {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if b != b"\n" {
                self.lines += 1;
                if self.lines == self.nth {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "disk full",
                    ));
                }
            }
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// A log writer whose WRITES always succeed and whose `flush` fails
    /// only after `arm`ing — the close-time shape. `write_line` flushes
    /// per line, so leaving it disarmed until the run is over is what
    /// isolates `Corrupter::close`'s own flush.
    struct FailFlushOnClose {
        inner: testing::VecWriter,
        armed: Arc<Mutex<bool>>,
    }
    impl Write for FailFlushOnClose {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if *self.armed.lock().unwrap() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "disk full",
                ));
            }
            self.inner.flush()
        }
    }

    /// Close-time half: a buffering writer can hold a whole
    /// line until the final flush, so `Corrupter::close` discarding that
    /// flush error loses evidence exactly like the per-line write arm
    /// would. It must latch the same flag — and `report soak` must fail
    /// `corruption_coverage` on it even though every line the receiver
    /// DID read parses and resolves.
    #[test]
    fn a_close_time_flush_failure_is_latched_and_fails_coverage() {
        let b = baseline_bytes(6.0, "close-flush");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let armed = Arc::new(Mutex::new(false));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Dup], 10_000, 1000),
            Box::new(FailFlushOnClose {
                inner: testing::VecWriter(Arc::clone(&log)),
                armed: Arc::clone(&armed),
            }),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        // Every per-line flush succeeded, so nothing is latched yet.
        assert!(
            !tap.stats().log_write_failed,
            "the run itself wrote cleanly: {:?}",
            tap.stats()
        );
        let injections = tap.stats().injections;
        assert!(injections >= 1, "6 s of baseline injects at least once");

        *armed.lock().unwrap() = true;
        tap.close();

        let stats = tap.stats();
        assert!(
            stats.log_write_failed,
            "the tap must record that the final flush failed: {stats:?}"
        );
        // …and the log on disk still looks complete, which is exactly why
        // the latch has to carry the failure rather than the receiver
        // tripping over a gap.
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let (_, logged) = parse_log(&text).expect("the written lines still parse");
        assert_eq!(logged.len() as u64, injections);
    }

    /// A lost injection line is not necessarily an
    /// unexplained event later — a `Dup` obliges the receiver to notice
    /// nothing — so the tap must latch the failure into its own stats
    /// rather than rely on the receiver to trip over the gap.
    #[test]
    fn corruption_log_write_failure_is_latched_in_the_stats() {
        let b = baseline_bytes(6.0, "log-fail");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Dup], 10_000, 1000),
            Box::new(FailOneLine {
                inner: testing::VecWriter(Arc::clone(&log)),
                nth: 2,
                lines: 0,
            }),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        // `garbage_inserts_1_to_300_non_sync_bytes…` proves 6 s of compact
        // baseline yields at least one injection at this rate/min_gap;
        // the failing line is the FIRST injection's (JSON write #2), so
        // one injection is all the latch needs.
        assert!(
            stats.injections >= 1,
            "6 s of baseline at min_gap 1000 injects at least once"
        );
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let (_, logged) = parse_log(&text).expect("the surviving lines still parse");
        assert_eq!(
            logged.len() as u64,
            stats.injections - 1,
            "exactly one line is missing"
        );
        assert!(
            stats.log_write_failed,
            "the tap must record that a line was lost: {stats:?}"
        );
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
            tst_test_helpers::rawts::classify_packet(&pat).unwrap().pid,
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
        let c = parse_corrupt("rate=5,min_gap=1000", 3, 1).unwrap();
        assert_eq!(
            c,
            CorruptConfig {
                rate_per_10k: 5,
                min_gap: 1000,
                classes: Class::ALL.to_vec(),
                seed: 3,
                geometry_scale: 1,
            }
        );
        let c = parse_corrupt("rate=50,min_gap=1200,classes=header+psi_flip", 3, 1).unwrap();
        assert_eq!(c.classes, vec![Class::Header, Class::PsiFlip]);
        assert!(
            parse_corrupt("rate=5,min_gap=999", 3, 1).is_err(),
            "min_gap < 2*window"
        );
        assert!(parse_corrupt("rate=5,min_gap=1000,classes=bogus", 3, 1).is_err());
        assert!(
            parse_corrupt("rate=0,min_gap=1000", 3, 1).is_err(),
            "rate 0 is 'disabled', reject"
        );
        assert!(
            parse_corrupt("min_gap=1000", 3, 1).is_err(),
            "rate required"
        );
        assert!(
            parse_corrupt("rate=5,min_gap=1000,bogus=1", 3, 1).is_err(),
            "unknown keys fail closed"
        );
    }

    /// The packet geometry follows `--au-scale`: the phenomena it bounds
    /// (next PES boundary, next PCR, next packet on a PID) are fixed in
    /// time, so at scale F they are F× further apart in packets. An
    /// omitted `min_gap` scales with it; an explicit one is held to the
    /// scaled floor, so the soak's literal `min_gap=1000` cannot be
    /// carried unchanged into a scaled run and silently make its
    /// attribution ambiguous.
    #[test]
    fn parse_corrupt_scales_the_packet_geometry_with_the_au_scale() {
        let c = parse_corrupt("rate=5", 3, 4).unwrap();
        assert_eq!(c.geometry_scale, 4);
        assert_eq!(c.min_gap, 4000, "default min_gap scales");
        assert_eq!(c.attribution_window(), 2000);
        assert_eq!(c.recovery_bound(), 2400);
        assert_eq!(c.approx_slack(), 512);
        assert_eq!(parse_corrupt("rate=5", 3, 1).unwrap().min_gap, 1000);
        let e = parse_corrupt("rate=5,min_gap=1000", 3, 4).unwrap_err();
        assert!(
            e.contains("4000") && e.contains("2400") && e.contains("--au-scale 4"),
            "the floor must name the scaled numbers: {e}"
        );
        assert!(parse_corrupt("rate=5,min_gap=4000", 3, 4).is_ok());
        assert!(
            parse_corrupt("rate=5", 3, 0).is_err(),
            "scale 0 is not a geometry"
        );
    }

    /// What the tap writes is what the receiver judges with: the header
    /// carries the SCALED geometry, not the scale-1 constants.
    #[test]
    fn the_log_header_carries_the_scaled_geometry() {
        let b = baseline_bytes(10.0, "geometry");
        let mut c = cfg(&Class::ALL, 10_000, 4000);
        c.geometry_scale = 4;
        let (_, text, _) = run_tap(&b, c);
        let first = text.lines().next().unwrap();
        let LogLine::Header(h) = serde_json::from_str::<LogLine>(first).unwrap() else {
            panic!("first line is not a header: {first}");
        };
        assert_eq!(
            (
                h.min_gap,
                h.attribution_window,
                h.recovery_bound,
                h.approx_slack
            ),
            (4000, 2000, 2400, 512)
        );
        // Scale 1 is byte-for-byte the geometry every archived log used.
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let LogLine::Header(h) =
            serde_json::from_str::<LogLine>(text.lines().next().unwrap()).unwrap()
        else {
            panic!("no header");
        };
        assert_eq!(
            (
                h.min_gap,
                h.attribution_window,
                h.recovery_bound,
                h.approx_slack
            ),
            (1000, 500, 600, 128)
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
                let info = tst_test_helpers::rawts::classify_packet(&pkt).unwrap();
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
        let info = tst_test_helpers::rawts::classify_packet(&pat).unwrap();
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
            let info = tst_test_helpers::rawts::classify_packet(&pkt).unwrap();
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
        let info = tst_test_helpers::rawts::classify_packet(&pat).unwrap();
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
        let mut r = tst_test_helpers::rawts::Reader::new();
        r.set_resync_mode(true);
        r.feed(&wire).unwrap();
        assert_eq!(r.resync_count(), stats.injections);
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
            let info = tst_test_helpers::rawts::classify_packet(&pkt).unwrap();
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

    #[test]
    fn psi_flip_defers_until_a_pat_or_pmt_packet_and_never_touches_the_crc() {
        let b = baseline_bytes(6.0, "psi");
        let (w, text, _) = run_tap(&b, cfg(&[Class::PsiFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(!inj.is_empty());
        for i in &inj {
            assert!(i.psi && i.detectable && matches!(i.pid, 0 | 0x1000));
            let pkt = &w[i.ordinal as usize * 188..][..188];
            let info = tst_test_helpers::rawts::classify_packet(pkt.try_into().unwrap()).unwrap();
            // Section body = after pointer field + 3-byte header, excluding the 4 CRC bytes.
            let sec = info.payload_off + 1 + usize::from(pkt[info.payload_off]);
            let len = (usize::from(pkt[sec + 1] & 0x0F) << 8) | usize::from(pkt[sec + 2]);
            let body = sec + 3..sec + 3 + len - 4;
            assert!(i.offsets.iter().all(|o| body.contains(o)), "{i:?}");
        }
    }

    #[test]
    fn psi_flip_drawn_on_a_media_packet_defers_to_the_next_psi_packet() {
        let b = baseline_bytes(60.0, "defer");
        let pid_at = |n: usize| -> u16 {
            tst_test_helpers::rawts::classify_packet(b[n * 188..][..188].try_into().unwrap())
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
        let info = tst_test_helpers::rawts::classify_packet(&pkt)
            .expect("the reconstructed packet parses");
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
                let info = tst_test_helpers::rawts::classify_packet(&pkt).unwrap();
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
        let info = tst_test_helpers::rawts::classify_packet(&pkt).expect("parses");
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
}
