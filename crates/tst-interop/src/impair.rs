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
pub struct Engine {
    rng: XorShift64,
    cfg: ImpairConfig,
}

impl Engine {
    pub fn new(cfg: ImpairConfig) -> Self {
        let rng = XorShift64::new(cfg.seed);
        Engine { rng, cfg }
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
    /// pre-empts every other per-packet impairment) — then
    /// **duplication** — then **loss** — then **reorder** — then
    /// **jitter**. Outside of outage, all four RNG draws (dup, loss,
    /// reorder, jitter) happen unconditionally and in that fixed order on
    /// every call, so the draw pattern per decision never depends on
    /// which branch ultimately wins — this keeps the sequence easy to
    /// reason about and replay.
    pub fn decide(&mut self, elapsed_ms: u64) -> Action {
        if self.in_outage(elapsed_ms) {
            return Action::Drop;
        }

        let dup_roll = self.rng.next_f64() * 100.0;
        let loss_roll = self.rng.next_f64() * 100.0;
        let reorder_roll = self.rng.next_f64() * 100.0;
        let jitter_roll = self.rng.next_f64();

        let jitter_delay = ((jitter_roll * (self.cfg.jitter_ms_max as f64 + 1.0)) as u32)
            .min(self.cfg.jitter_ms_max);
        let reorder_bump = if reorder_roll < self.cfg.reorder_pct {
            self.cfg.reorder_hold
        } else {
            0
        };
        let delay_ms = self
            .cfg
            .base_delay_ms
            .saturating_add(jitter_delay)
            .saturating_add(reorder_bump);

        if dup_roll < self.cfg.dup_pct {
            Action::DupForward { delay_ms }
        } else if loss_roll < self.cfg.loss_pct {
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
}
