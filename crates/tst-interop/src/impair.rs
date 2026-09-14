//! Deterministic impairment decision engine — the pure-logic core the UDP
//! impairment proxy (a later task) drives once per packet. Given the same
//! seed and [`ImpairConfig`], [`Engine::decide`] produces the exact same
//! sequence of [`Action`]s for the exact same sequence of `elapsed_ms`
//! inputs: determinism is the entire point, so nothing in this module
//! reads wall-clock time itself (no `SystemTime`/`Instant`) — the caller
//! supplies `elapsed_ms`, which is what lets a soak run be replayed and
//! its evidence reproduced byte-for-byte from just the seed + config.

/// Minimal xorshift64* PRNG (Vigna, "An experimental exploration of
/// Marsaglia's xorshift generators, scrambled"). Chosen over the stdlib's
/// `rand` crate so `tst-interop` stays free of an external RNG dependency
/// and so the exact bit-sequence this crate produces is pinned by this
/// file alone, not by an upstream crate's version.
///
/// A xorshift core has a fixed point at state `0` (it maps `0 -> 0`
/// forever, which would make `next_u64` return `0` on every call). Both
/// [`XorShift64::new`] and `next_u64` itself remap a `0` state to a fixed
/// nonzero constant, so the degenerate state can never produce a
/// degenerate sequence even if a caller builds `XorShift64(0)` directly
/// (the tuple field is `pub`).
#[derive(Clone, Debug)]
pub struct XorShift64(pub u64);

/// Fixed nonzero replacement for a zero seed/state — the fractional part
/// of the golden ratio in Q64, a standard "any nonzero bit pattern will
/// do" constant with no small period or obvious structure.
const FIXED_NONZERO_SEED: u64 = 0x9E3779B97F4A7C15;

/// The xorshift64* multiplier constant (Vigna's `2685821657736338717`,
/// i.e. `0x2545_F491_4F6C_DD1D`) used to scramble the raw xorshift output
/// into a value that passes standard statistical test suites.
const XORSHIFT64_STAR_MULTIPLIER: u64 = 0x2545_F491_4F6C_DD1D;

impl XorShift64 {
    /// Build a generator from `seed`, remapping `0` to a fixed nonzero
    /// constant (see the type doc).
    pub fn new(seed: u64) -> Self {
        XorShift64(if seed == 0 { FIXED_NONZERO_SEED } else { seed })
    }

    /// Advance the generator and return the next 64-bit output.
    pub fn next_u64(&mut self) -> u64 {
        if self.0 == 0 {
            self.0 = FIXED_NONZERO_SEED;
        }
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(XORSHIFT64_STAR_MULTIPLIER)
    }

    /// Next value uniform in `[0, 1)`, using the top 53 bits of
    /// [`next_u64`](Self::next_u64) (the standard technique for turning a
    /// 64-bit generator into a double with full mantissa precision).
    pub fn next_f64(&mut self) -> f64 {
        const TWO_POW_53: f64 = 9_007_199_254_740_992.0; // 1u64 << 53
        (self.next_u64() >> 11) as f64 / TWO_POW_53
    }
}

/// Impairment knobs for one [`Engine`] run. Percent fields are `0.0..=100.0`.
///
/// `Default` yields a fully transparent config (all probabilities `0.0`,
/// no jitter, no outage) — [`Engine::decide`] then always returns
/// `Action::Forward { delay_ms: 0 }`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImpairConfig {
    /// Probability (percent) a packet is dropped.
    pub loss_pct: f64,
    /// Probability (percent) a packet is forwarded twice.
    pub dup_pct: f64,
    /// Probability (percent) a packet gets the reorder delay bump (see
    /// [`Engine::decide`] for how `reorder_hold` is applied).
    pub reorder_pct: f64,
    /// Extra delay, in milliseconds, applied to a packet selected for
    /// reorder. Named for the config-author-facing intent ("hold this
    /// packet back long enough for N packets behind it to overtake it"),
    /// but the engine has no notion of packet rate, so it applies the
    /// value directly as milliseconds — the proxy driving this engine
    /// (Task 9) is responsible for choosing a value that corresponds to
    /// roughly N packet intervals for its own traffic rate. The engine
    /// itself only ever emits a `delay_ms` on [`Action::Forward`] /
    /// [`Action::DupForward`]; the actual out-of-order delivery is a side
    /// effect of the proxy's timing wheel releasing a delayed packet
    /// after later, non-delayed ones.
    pub reorder_hold: u32,
    /// Upper bound (inclusive) of a uniform `0..=jitter_ms_max`
    /// millisecond delay added to every non-dropped packet.
    pub jitter_ms_max: u32,
    /// Constant delay, in milliseconds, added to every non-dropped
    /// packet — models a link's base one-way latency (WAN lag), on top
    /// of which `jitter_ms_max` varies and `reorder_hold` bumps. `0`
    /// (the default) preserves the pre-existing decision sequence
    /// exactly: no RNG draw is consumed for it.
    pub base_delay_ms: u32,
    /// RNG seed. `0` is remapped to a fixed nonzero constant (see
    /// [`XorShift64`]).
    pub seed: u64,
    /// Period, in seconds, between the start of successive outage
    /// windows. `None` disables periodic outages entirely.
    pub outage_period_s: Option<u64>,
    /// Duration, in seconds, of each outage window.
    pub outage_dur_s: u64,
}

/// Salt mixed into the seed before generating a phase schedule, so a run's
/// schedule and its per-packet decision stream are driven by two
/// independent generators: `Engine`'s own RNG is `XorShift64::new(seed)`
/// and the schedule's is `XorShift64::new(seed ^ SCHEDULE_SALT)`. Without
/// the salt the two would share a state trajectory and the schedule would
/// be correlated with the first few packet decisions.
pub const SCHEDULE_SALT: u64 = 0x5C4E_D01E_0000_0D0D;

/// Mean run length of a burst drawn uniformly from `(3, 8)` — `(3+8)/2`.
/// A burst phase divides its configured `loss_pct` by this to get the
/// per-packet draw threshold, so that firing one burst of ~5.5 drops per
/// crossing reproduces the configured *effective* loss rate rather than
/// 5.5× it. See [`Phase::draw_pct`].
const BURST_MEAN_RUN: f64 = 5.5;

/// One phase of a scheduled impairment run: the impairment knobs that are
/// in force for one `phase_s`-long slice of the wall clock. A scheduled
/// run walks through a `Vec<Phase>` as time passes, which is what makes a
/// soak exercise a *changing* link rather than one fixed impairment level.
///
/// `Serialize`/`Deserialize` because the proxy echoes the schedule it ran
/// into its stats file, so a run's evidence records the exact phases that
/// produced it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Phase {
    /// Position of this phase in the schedule, `0`-based.
    pub index: u32,
    /// The *effective* loss rate (percent) this phase targets — the
    /// fraction of packets expected to be dropped once burst runs are
    /// accounted for. This is the number to compare an observed drop rate
    /// against; it is NOT the threshold the RNG draw is compared with
    /// (that is [`draw_pct`](Self::draw_pct)).
    pub loss_pct: f64,
    /// Whether losses in this phase arrive in bursts (consecutive runs of
    /// drops, as a real congested or fading link loses packets) rather
    /// than independently per packet.
    pub burst: bool,
    /// Inclusive `(lo, hi)` bounds of a burst's run length: `(3, 8)` in a
    /// burst phase, `(1, 1)` otherwise.
    pub burst_run: (u32, u32),
    /// The threshold the per-packet loss draw is actually compared
    /// against: `loss_pct` in a non-burst phase, and
    /// `loss_pct / BURST_MEAN_RUN` in a burst phase — because each
    /// crossing there costs ~5.5 packets instead of one.
    pub draw_pct: f64,
    /// Per-phase override of [`ImpairConfig::jitter_ms_max`].
    pub jitter_ms_max: u32,
    /// Per-phase override of [`ImpairConfig::reorder_pct`].
    pub reorder_pct: f64,
    /// Per-phase override of [`ImpairConfig::reorder_hold`].
    pub reorder_hold: u32,
    /// Per-phase override of [`ImpairConfig::base_delay_ms`].
    pub base_delay_ms: u32,
}

impl Phase {
    /// The single implicit phase of a fixed-mode run: the config's own
    /// knobs, never bursty, drawing directly against `loss_pct`. Keeping
    /// fixed mode expressible as a one-phase schedule is what lets
    /// [`Engine::decide`] have exactly one code path — and the fixed-mode
    /// decision sequence is pinned by
    /// `fixed_mode_decision_sequence_is_unchanged`.
    fn from_fixed(cfg: &ImpairConfig) -> Self {
        Phase {
            index: 0,
            loss_pct: cfg.loss_pct,
            burst: false,
            burst_run: (1, 1),
            draw_pct: cfg.loss_pct,
            jitter_ms_max: cfg.jitter_ms_max,
            reorder_pct: cfg.reorder_pct,
            reorder_hold: cfg.reorder_hold,
            base_delay_ms: cfg.base_delay_ms,
        }
    }
}

/// Generate a deterministic `phases`-long impairment schedule from `seed`.
///
/// **Determinism contract.** The generator is
/// `XorShift64::new(seed ^ SCHEDULE_SALT)` and each phase consumes
/// **exactly six** [`XorShift64::next_f64`] draws, in this fixed order:
///
/// 1. `loss`    — `0.5 + r * 3.5`   → `0.5..4.0` percent effective loss
/// 2. `burst`   — `r < 0.3`         → ~30% of phases are bursty
/// 3. `jitter`  — `5 + (r * 36)`    → `5..=40` ms
/// 4. `reorder` — `r * 2.0`         → `0.0..2.0` percent
/// 5. `hold`    — `100 + (r * 201)` → `100..=300` ms
/// 6. `delay`   — `10 + (r * 51)`   → `10..=60` ms
///
/// Changing that order, the draw count, or any range changes every
/// schedule ever generated — archived evidence quotes its seed, not its
/// phases, so the mapping from seed to schedule must stay stable.
pub fn generate_schedule(seed: u64, phases: u32) -> Vec<Phase> {
    let mut rng = XorShift64::new(seed ^ SCHEDULE_SALT);
    (0..phases)
        .map(|index| {
            let loss_pct = 0.5 + rng.next_f64() * 3.5;
            let burst = rng.next_f64() < 0.3;
            let jitter_ms_max = 5 + (rng.next_f64() * 36.0) as u32;
            let reorder_pct = rng.next_f64() * 2.0;
            let reorder_hold = 100 + (rng.next_f64() * 201.0) as u32;
            let base_delay_ms = 10 + (rng.next_f64() * 51.0) as u32;
            let (burst_run, draw_pct) = if burst {
                ((3, 8), loss_pct / BURST_MEAN_RUN)
            } else {
                ((1, 1), loss_pct)
            };
            Phase {
                index,
                loss_pct,
                burst,
                burst_run,
                draw_pct,
                jitter_ms_max,
                reorder_pct,
                reorder_hold,
                base_delay_ms,
            }
        })
        .collect()
}

/// One decision for a single packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Drop the packet.
    Drop,
    /// Forward the packet after an additional `delay_ms`.
    Forward { delay_ms: u32 },
    /// Forward the packet twice (duplication), each copy delayed by
    /// `delay_ms`.
    DupForward { delay_ms: u32 },
}

/// Deterministic per-packet impairment decision engine. See the module
/// doc for the determinism contract.
///
/// An engine always runs a schedule of one or more [`Phase`]s: fixed mode
/// ([`Engine::new`]) is the degenerate one-phase case whose phase never
/// ends, and scheduled mode ([`Engine::with_schedule`]) walks the phases
/// on the wall clock the caller supplies.
pub struct Engine {
    rng: XorShift64,
    cfg: ImpairConfig,
    phases: Vec<Phase>,
    /// Wall-clock duration of one phase, in milliseconds.
    phase_ms: u64,
    /// Drops still owed to an in-flight burst run, not counting the drop
    /// that started it. `0` outside a burst.
    burst_left: u32,
}

impl Engine {
    /// Fixed-mode engine: the config's knobs apply for the whole run, as
    /// one implicit phase that never ends.
    pub fn new(cfg: ImpairConfig) -> Self {
        // `u64::MAX / 1000` seconds is ~584 million years; multiplied back
        // up in `with_schedule` it saturates near `u64::MAX`, so
        // `phase_index` is 0 for every `elapsed_ms` a real run can reach.
        Engine::with_schedule(cfg, vec![Phase::from_fixed(&cfg)], u64::MAX / 1000)
    }

    /// Scheduled engine: `phases` in force for `phase_s` seconds each,
    /// clamping to the last phase once the schedule is exhausted.
    ///
    /// `cfg`'s `dup_pct`, `seed` and outage fields still apply across
    /// every phase — duplication and outages model the transport and the
    /// link coming and going, not the varying link quality a phase
    /// describes. An empty `phases` falls back to the fixed-mode phase so
    /// the engine always has one to decide against.
    pub fn with_schedule(cfg: ImpairConfig, phases: Vec<Phase>, phase_s: u64) -> Self {
        let phases = if phases.is_empty() {
            vec![Phase::from_fixed(&cfg)]
        } else {
            phases
        };
        Engine {
            rng: XorShift64::new(cfg.seed),
            cfg,
            phases,
            phase_ms: phase_s.saturating_mul(1000),
            burst_left: 0,
        }
    }

    /// The phases this engine walks. Always non-empty.
    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// Index into [`phases`](Self::phases) in force at `elapsed_ms`,
    /// clamped to the last phase so a run that outlives its schedule
    /// simply stays on the final phase rather than panicking.
    ///
    /// Takes `&self` — like [`in_outage`](Self::in_outage) it never
    /// touches the RNG, so it can be probed freely.
    pub fn phase_index(&self, elapsed_ms: u64) -> usize {
        let last = (self.phases.len() - 1) as u64;
        if self.phase_ms == 0 {
            // Degenerate config: zero-length phases are all already in the
            // past at any `elapsed_ms`, so the clamp is the whole answer.
            return last as usize;
        }
        (elapsed_ms / self.phase_ms).min(last) as usize
    }

    /// Whether `elapsed_ms` falls inside an outage window. Windows repeat
    /// every `outage_period_s` seconds and last `outage_dur_s` seconds,
    /// i.e. `elapsed_ms` is in outage exactly when it falls in
    /// `[k * period_ms, k * period_ms + dur_ms)` for some `k >= 0`.
    ///
    /// Takes `&self` (not `&mut self`) — it never touches the RNG, so a
    /// caller can probe outage windows without perturbing the decision
    /// sequence [`decide`](Self::decide) would otherwise produce.
    pub fn in_outage(&self, elapsed_ms: u64) -> bool {
        let Some(period_s) = self.cfg.outage_period_s else {
            return false;
        };
        let dur_ms = self.cfg.outage_dur_s.saturating_mul(1000);
        if period_s == 0 {
            // A zero period is degenerate config (nothing to modulo by);
            // treat it as "always in outage" iff any outage was
            // configured at all, rather than panicking on `% 0`.
            return dur_ms > 0;
        }
        let period_ms = period_s.saturating_mul(1000);
        (elapsed_ms % period_ms) < dur_ms
    }

    /// Decide the [`Action`] for the next packet, `elapsed_ms`
    /// milliseconds into the run.
    ///
    /// Precedence (first match wins): **outage** (all packets Drop, and
    /// no RNG draw happens — outage models a total link failure, which
    /// pre-empts every other per-packet impairment) — then an
    /// **in-flight burst run** — then **duplication** — then **loss** —
    /// then **reorder** — then **jitter**. Outside of outage, all four
    /// RNG draws (dup, loss, reorder, jitter) happen unconditionally and
    /// in that fixed order on every call, so the draw pattern per
    /// decision never depends on which branch ultimately wins — this
    /// keeps the sequence easy to reason about and replay.
    ///
    /// The impairment levels come from the [`Phase`] in force at
    /// `elapsed_ms`; only `dup_pct` and the outage windows are per-run.
    ///
    /// A burst run pre-empts duplication: once a burst is in flight every
    /// packet it covers is dropped, so a packet cannot be duplicated
    /// while the link is in the middle of losing a run. It still performs
    /// its four draws first, so bursts never perturb the draw sequence.
    pub fn decide(&mut self, elapsed_ms: u64) -> Action {
        if self.in_outage(elapsed_ms) {
            return Action::Drop;
        }

        let dup_roll = self.rng.next_f64() * 100.0;
        let loss_roll = self.rng.next_f64() * 100.0;
        let reorder_roll = self.rng.next_f64() * 100.0;
        let jitter_roll = self.rng.next_f64();

        let p = self.phases[self.phase_index(elapsed_ms)];

        let jitter_delay =
            ((jitter_roll * (p.jitter_ms_max as f64 + 1.0)) as u32).min(p.jitter_ms_max);
        let reorder_bump = if reorder_roll < p.reorder_pct {
            p.reorder_hold
        } else {
            0
        };
        let delay_ms = p
            .base_delay_ms
            .saturating_add(jitter_delay)
            .saturating_add(reorder_bump);

        if self.burst_left > 0 {
            self.burst_left -= 1;
            return Action::Drop;
        }

        if dup_roll < self.cfg.dup_pct {
            Action::DupForward { delay_ms }
        } else if loss_roll < p.draw_pct {
            if p.burst {
                // Run length comes from the low digits of the loss draw
                // that just fired — NOT a fresh draw, which would make the
                // number of draws per decision depend on the outcome and
                // break the replay contract. `loss_roll` is in
                // `[0, 100)`, so `loss_roll * 1e6` is well inside `u32`.
                let (lo, hi) = p.burst_run;
                let run = lo + ((loss_roll * 1e6) as u32 % (hi - lo + 1));
                // This packet is the first drop of the run.
                self.burst_left = run.saturating_sub(1);
            }
            Action::Drop
        } else {
            Action::Forward { delay_ms }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (a) Two `Engine`s built from the same seed + config produce
    /// identical 10_000-decision sequences.
    #[test]
    fn same_seed_same_config_is_deterministic() {
        let cfg = ImpairConfig {
            loss_pct: 5.0,
            dup_pct: 3.0,
            reorder_pct: 4.0,
            reorder_hold: 20,
            jitter_ms_max: 15,
            base_delay_ms: 0,
            seed: 12345,
            outage_period_s: Some(10),
            outage_dur_s: 1,
        };
        let mut e1 = Engine::new(cfg);
        let mut e2 = Engine::new(cfg);

        let seq1: Vec<Action> = (0..10_000u64).map(|i| e1.decide(i * 7)).collect();
        let seq2: Vec<Action> = (0..10_000u64).map(|i| e2.decide(i * 7)).collect();

        assert_eq!(seq1, seq2);
    }

    /// (b) Different seeds produce different sequences (same config
    /// otherwise).
    #[test]
    fn different_seeds_diverge() {
        let base = ImpairConfig {
            loss_pct: 5.0,
            dup_pct: 3.0,
            reorder_pct: 4.0,
            reorder_hold: 20,
            jitter_ms_max: 15,
            base_delay_ms: 0,
            seed: 1,
            outage_period_s: None,
            outage_dur_s: 0,
        };
        let mut e1 = Engine::new(ImpairConfig { seed: 1, ..base });
        let mut e2 = Engine::new(ImpairConfig { seed: 2, ..base });

        let seq1: Vec<Action> = (0..1_000u64).map(|i| e1.decide(i * 7)).collect();
        let seq2: Vec<Action> = (0..1_000u64).map(|i| e2.decide(i * 7)).collect();

        assert_ne!(seq1, seq2);
    }

    /// (c) loss_pct=2.0 over 100_000 decisions yields 1.6-2.4% drops.
    /// Fixed seed (`777`) — the bounds cover the RNG's distribution
    /// around the configured probability, not run-to-run variance, so
    /// they can't flake: the exact decision sequence for this seed is
    /// pinned forever by this test.
    #[test]
    fn loss_pct_matches_configured_rate_within_tolerance() {
        let cfg = ImpairConfig {
            loss_pct: 2.0,
            dup_pct: 0.0,
            reorder_pct: 0.0,
            reorder_hold: 0,
            jitter_ms_max: 0,
            base_delay_ms: 0,
            seed: 777,
            outage_period_s: None,
            outage_dur_s: 0,
        };
        let mut engine = Engine::new(cfg);

        let total = 100_000u64;
        let drops = (0..total)
            .filter(|&i| matches!(engine.decide(i), Action::Drop))
            .count();

        let pct = drops as f64 / total as f64 * 100.0;
        assert!(
            (1.6..=2.4).contains(&pct),
            "observed drop rate {pct}% out of tolerance (drops={drops}/{total})"
        );
    }

    /// (d) `in_outage` is true exactly inside `[k*period, k*period+dur)`
    /// windows (period=5s, dur=2s -> ms windows `[0,2000)`, `[5000,7000)`,
    /// `[10000,12000)`, ...).
    #[test]
    fn in_outage_matches_periodic_windows_exactly() {
        let cfg = ImpairConfig {
            outage_period_s: Some(5),
            outage_dur_s: 2,
            ..ImpairConfig::default()
        };
        let engine = Engine::new(cfg);

        // Inside window k=0: [0, 2000)
        assert!(engine.in_outage(0));
        assert!(engine.in_outage(1999));
        // Outside, before window k=1
        assert!(!engine.in_outage(2000));
        assert!(!engine.in_outage(4999));
        // Inside window k=1: [5000, 7000)
        assert!(engine.in_outage(5000));
        assert!(engine.in_outage(6999));
        // Outside, before window k=2
        assert!(!engine.in_outage(7000));
        assert!(!engine.in_outage(9999));
        // Inside window k=2: [10000, 12000)
        assert!(engine.in_outage(10_000));
        assert!(engine.in_outage(11_999));
        assert!(!engine.in_outage(12_000));
    }

    /// (e) Every decision made during an outage window is `Drop`,
    /// regardless of the other impairment knobs (here all set to 0 so a
    /// non-Drop result could only come from the outage check being
    /// bypassed).
    #[test]
    fn decisions_during_outage_are_all_drop() {
        let cfg = ImpairConfig {
            loss_pct: 0.0,
            dup_pct: 0.0,
            reorder_pct: 0.0,
            reorder_hold: 0,
            jitter_ms_max: 0,
            base_delay_ms: 0,
            seed: 42,
            outage_period_s: Some(5),
            outage_dur_s: 2,
        };
        let mut engine = Engine::new(cfg);

        for elapsed_ms in [0u64, 500, 1999, 5000, 6500, 10_000, 11_999] {
            assert_eq!(engine.decide(elapsed_ms), Action::Drop);
        }
    }

    /// (g) `base_delay_ms` (constant one-way link latency, e.g. a WAN
    /// RTT's worth of lag) is applied to EVERY non-dropped packet, on
    /// top of jitter/reorder — with every probabilistic knob at 0 it is
    /// the exact delay of every decision.
    #[test]
    fn base_delay_ms_applies_to_every_non_dropped_packet() {
        let cfg = ImpairConfig {
            base_delay_ms: 40,
            seed: 7,
            ..ImpairConfig::default()
        };
        let mut engine = Engine::new(cfg);
        for i in 0..1_000u64 {
            assert_eq!(engine.decide(i * 3), Action::Forward { delay_ms: 40 });
        }
    }

    /// (h) `base_delay_ms` composes additively with the reorder hold:
    /// a packet selected for reorder is delayed `base + hold` (jitter 0
    /// here so the sum is exact).
    #[test]
    fn base_delay_ms_composes_with_reorder_hold() {
        let cfg = ImpairConfig {
            base_delay_ms: 30,
            reorder_pct: 100.0,
            reorder_hold: 200,
            seed: 7,
            ..ImpairConfig::default()
        };
        let mut engine = Engine::new(cfg);
        for i in 0..100u64 {
            assert_eq!(engine.decide(i * 3), Action::Forward { delay_ms: 230 });
        }
    }

    /// (f) loss_pct=0, jitter=0 (and everything else at its transparent
    /// default) -> every decision is `Forward { delay_ms: 0 }`.
    #[test]
    fn fully_transparent_config_always_forwards_with_no_delay() {
        let cfg = ImpairConfig::default();
        let mut engine = Engine::new(cfg);

        for i in 0..10_000u64 {
            assert_eq!(engine.decide(i * 7), Action::Forward { delay_ms: 0 });
        }
    }

    /// Fixed-mode replay pin: the decision sequence for seed 1 +
    /// `soak.sh`'s impairment constants must stay byte-identical forever.
    /// Archived soak evidence is only reproducible from seed + config if
    /// this exact sequence replays, so any change to the engine that
    /// perturbs fixed-mode decisions — a reordered draw, an extra draw,
    /// a changed threshold comparison — breaks this test by design.
    ///
    /// The expected digest was captured on the unmodified engine (before
    /// the phase-schedule work) by running this exact loop and copying
    /// the observed hex in; see the commit that introduced it.
    #[test]
    fn fixed_mode_decision_sequence_is_unchanged() {
        use sha2::Digest;
        let cfg = ImpairConfig {
            loss_pct: 2.0,
            dup_pct: 0.0,
            reorder_pct: 1.0,
            reorder_hold: 200,
            jitter_ms_max: 20,
            base_delay_ms: 30,
            seed: 1,
            outage_period_s: Some(21600),
            outage_dur_s: 90,
        };
        let mut e = Engine::new(cfg);
        let mut h = sha2::Sha256::new();
        for i in 0..200_000u64 {
            h.update(format!("{:?}", e.decide(100_000 + i * 3)).as_bytes());
        }
        let hex = crate::verify::to_hex(&h.finalize());
        assert_eq!(
            hex,
            "41b14964082f8aa911bbceee178f7806c6dd5d5d4884aa66d4b2fa66a6502360"
        );
    }

    /// A schedule is a pure function of its seed, differs between seeds,
    /// and every generated phase lands inside the documented ranges.
    #[test]
    fn generate_schedule_is_deterministic_and_in_range() {
        let a = generate_schedule(9, 6);
        assert_eq!(a, generate_schedule(9, 6));
        assert_ne!(a, generate_schedule(10, 6));
        assert_eq!(a.len(), 6);
        for (i, p) in a.iter().enumerate() {
            assert_eq!(p.index as usize, i);
            assert!((0.5..=4.0).contains(&p.loss_pct));
            assert!((5..=40).contains(&p.jitter_ms_max));
            assert!((0.0..=2.0).contains(&p.reorder_pct));
            assert!((100..=300).contains(&p.reorder_hold));
            assert!((10..=60).contains(&p.base_delay_ms));
            if p.burst {
                assert_eq!(p.burst_run, (3, 8));
                assert!((p.draw_pct - p.loss_pct / 5.5).abs() < 1e-9);
            } else {
                assert_eq!(p.burst_run, (1, 1));
                assert_eq!(p.draw_pct, p.loss_pct);
            }
        }
        assert!(
            a.iter().any(|p| p.burst) || generate_schedule(11, 6).iter().any(|p| p.burst),
            "p=0.3 per phase: some seed bursts"
        );
    }

    /// The engine switches phases on the caller's wall clock, each phase
    /// reproduces its own effective loss rate (including the burst phase,
    /// whose `draw_pct` is pre-divided by the mean run length), and
    /// `phase_index` clamps past the end of the schedule.
    #[test]
    fn scheduled_engine_switches_phase_on_the_wall_clock_and_matches_each_phase_rate() {
        let mut phases = generate_schedule(3, 3);
        phases[0].burst = false;
        phases[0].burst_run = (1, 1);
        phases[0].loss_pct = 1.0;
        phases[0].draw_pct = 1.0;
        phases[1].burst = true;
        phases[1].burst_run = (3, 8);
        phases[1].loss_pct = 4.0;
        phases[1].draw_pct = 4.0 / 5.5;
        phases[2].burst = false;
        phases[2].burst_run = (1, 1);
        phases[2].loss_pct = 0.5;
        phases[2].draw_pct = 0.5;
        let mut e = Engine::with_schedule(
            ImpairConfig {
                seed: 5,
                ..ImpairConfig::default()
            },
            phases.clone(),
            100,
        );
        let mut drops = [0u64; 3];
        let n = 200_000u64;
        for k in 0..3u64 {
            for i in 0..n {
                let t = k * 100_000 + (i % 100_000);
                if matches!(e.decide(t), Action::Drop) {
                    drops[k as usize] += 1;
                }
            }
        }
        for k in 0..3 {
            let pct = drops[k] as f64 / n as f64 * 100.0;
            assert!(
                (pct - phases[k].loss_pct).abs() <= phases[k].loss_pct * 0.15 + 0.1,
                "phase {k}: observed {pct}% vs {}%",
                phases[k].loss_pct
            );
        }
        assert_eq!(e.phase_index(250_000), 2);
        assert_eq!(e.phase_index(999_999_999), 2, "clamped to the last phase");
    }

    /// Burst-phase drops arrive in consecutive runs, not independently —
    /// the whole point of burst mode.
    #[test]
    fn burst_drops_come_in_runs_of_3_to_8() {
        let mut phases = generate_schedule(3, 1);
        phases[0].burst = true;
        phases[0].burst_run = (3, 8);
        phases[0].loss_pct = 2.0;
        phases[0].draw_pct = 2.0 / 5.5;
        let mut e = Engine::with_schedule(
            ImpairConfig {
                seed: 8,
                ..ImpairConfig::default()
            },
            phases,
            3600,
        );
        let seq: Vec<bool> = (0..100_000u64)
            .map(|i| matches!(e.decide(i), Action::Drop))
            .collect();
        let mut runs = Vec::new();
        let mut cur = 0;
        for d in seq {
            if d {
                cur += 1
            } else if cur > 0 {
                runs.push(cur);
                cur = 0
            }
        }
        assert!(!runs.is_empty());
        // Adjacent bursts can merge; every run is at least 3.
        assert!(runs.iter().all(|&r| r >= 3), "{runs:?}");
        assert!(
            runs.iter().filter(|&&r| (3..=8).contains(&r)).count() * 10 >= runs.len() * 8,
            "most runs are a single burst: {runs:?}"
        );
    }
}
