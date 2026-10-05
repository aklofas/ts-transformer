//! Background thread emitting RR (receiver-side) or SR (sender-side)
//! at randomized `RTCP_INTERVAL` per RFC 3550 §6.2 + §6.3.1.
//!
//! Simplification: we don't run the full RFC 3550 transmission-interval
//! algorithm (which scales with session size); we use a fixed-base 5 s
//! interval with ±50% randomization per §6.3.1. Single-source point-to-point
//! flows don't benefit from the full algorithm.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// State for the deterministic-jitter fallback path used when `getrandom`
/// fails. The counter advances by the golden-ratio additive constant
/// (0x9E3779B1 ≈ 2³²/φ, a Weyl-sequence step) so successive values are
/// well-distributed across the u32 range, keeping the jitter behaviour close
/// to the uniform distribution that the RFC 3550 §6.3.1 interval algorithm
/// expects.
struct FallbackState {
    /// Set on the first `getrandom` failure; prevents the warning from
    /// flooding the log on every subsequent iteration.
    warned: bool,
    /// Wrapping counter advanced each time the fallback path is taken.
    counter: u32,
}

impl FallbackState {
    const fn new() -> Self {
        Self {
            warned: false,
            counter: 0,
        }
    }
}

/// Core jitter-word selection, split out so tests can drive the fallback
/// branch without making `getrandom` fail globally.
///
/// - `getrandom_word` is `Some(word)` when `getrandom` succeeded, `None`
///   when it failed.
/// - On `None`: emits a `tracing::warn!` the first time (guarded by
///   `state.warned`), then advances the wrapping counter and returns it.
fn apply_jitter_word(getrandom_word: Option<u32>, state: &mut FallbackState) -> u32 {
    match getrandom_word {
        Some(w) => w,
        None => {
            if !state.warned {
                tracing::warn!(
                    target: "tst_rtp",
                    "getrandom failed in RTCP reporter; \
                     falling back to deterministic jitter — \
                     RTCP will keep running but intervals are not cryptographically random"
                );
                state.warned = true;
            }
            // Golden-ratio (Weyl sequence) additive step: adding 0x9E3779B1
            // (≈ 2³²/φ) each iteration maps sequential counters to
            // well-spread u32 values, giving jitter behaviour that spans the
            // full [0.5, 1.5] × base range across iterations.
            state.counter = state.counter.wrapping_add(2_654_435_761);
            state.counter
        }
    }
}

/// Obtain the next jitter word, falling back gracefully if `getrandom` fails.
fn next_jitter_word(state: &mut FallbackState) -> u32 {
    let mut buf = [0u8; 4];
    let word = getrandom::getrandom(&mut buf)
        .ok()
        .map(|()| u32::from_le_bytes(buf));
    apply_jitter_word(word, state)
}

/// Base RTCP transmission interval, RFC 3550 §6.2 says 5 s for the
/// reduced minimum (after the initial RTCP_BANDWIDTH backoff). It is
/// not scaled by session size.
pub const RTCP_BASE_INTERVAL: Duration = Duration::from_secs(5);

/// Compute a randomized interval per RFC 3550 §6.3.1 — `interval *
/// uniform(0.5, 1.5)`.
pub fn jitter_interval(base: Duration, urandom: u32) -> Duration {
    // urandom is treated as 0..u32::MAX; scale to 0.5..1.5
    let factor = 0.5 + (urandom as f64 / u32::MAX as f64);
    Duration::from_secs_f64(base.as_secs_f64() * factor)
}

/// A handle to a running RTCP reporter thread. Dropping this handle
/// signals cancel and joins the thread.
pub struct RtcpReporterHandle {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RtcpReporterHandle {
    /// Spawn a reporter thread that calls `emit` at randomized intervals.
    /// The closure receives no args; the caller (the RTP sender or
    /// receiver transport) snapshots its own state inside the closure.
    pub fn spawn(emit: impl FnMut() + Send + 'static) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let mut emit = emit;
        let thread = std::thread::Builder::new()
            .name("rtcp-reporter".to_string())
            .spawn(move || {
                let mut last_emit = std::time::Instant::now();
                let mut fallback = FallbackState::new();
                while !cancel_thread.load(Ordering::Relaxed) {
                    let interval =
                        jitter_interval(RTCP_BASE_INTERVAL, next_jitter_word(&mut fallback));
                    // Wake every 100 ms to check the cancel flag — same shape as
                    // the transport's cancel-handle pattern.
                    let deadline = last_emit + interval;
                    while std::time::Instant::now() < deadline
                        && !cancel_thread.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if cancel_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    emit();
                    last_emit = std::time::Instant::now();
                }
            });
        // Degrade gracefully: a thread-spawn failure (OS resource exhaustion)
        // must not panic — the FFI bindings do not catch unwinds, so a panic
        // would abort the host process. If the reporter thread can't start,
        // RTCP reporting is simply disabled (it is opt-in/experimental anyway).
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(
                    target: "tst_rtp",
                    error = %e,
                    "failed to spawn rtcp-reporter thread; RTCP reporting disabled"
                );
                None
            }
        };
        Self { cancel, thread }
    }

    /// Signal the reporter thread to stop. Does not wait for the thread
    /// to exit — drop the handle to join.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for RtcpReporterHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── apply_jitter_word fallback path ────────────────────────────────────

    /// Successive fallback words (getrandom_word=None) must differ from each
    /// other, confirming the counter actually advances.
    #[test]
    fn fallback_successive_words_differ() {
        let mut state = FallbackState::new();
        let w1 = apply_jitter_word(None, &mut state);
        let w2 = apply_jitter_word(None, &mut state);
        let w3 = apply_jitter_word(None, &mut state);
        assert_ne!(w1, w2, "first and second fallback words must differ");
        assert_ne!(w2, w3, "second and third fallback words must differ");
        assert_ne!(w1, w3, "first and third fallback words must differ");
    }

    /// Weyl-sequence fallback words are well-spread across the jitter band.
    ///
    /// The bounds check (`interval ∈ [0.5, 1.5] × base`) is already
    /// guaranteed by `jitter_interval`'s own construction — asserting it
    /// would be vacuous. Instead this test checks the "well-distributed"
    /// property the comment claims: the spread of the 16 deterministic words
    /// across the `[0.5, 1.5] × base` band must exceed half the band width.
    ///
    /// The golden-ratio additive step (0x9E3779B1) produces values that span
    /// ~91 % of the u32 range in the first 16 iterations, yielding a spread
    /// of ~91 % of the band. The threshold (50 %) is generous enough to
    /// tolerate small changes to the constant while still failing a constant
    /// or slowly-incrementing counter generator.
    #[test]
    fn fallback_words_spread_across_jitter_band() {
        let base = RTCP_BASE_INTERVAL;
        let band_width = base.as_secs_f64(); // (1.5 - 0.5) × base
        let mut state = FallbackState::new();
        let intervals: Vec<f64> = (0..16)
            .map(|_| {
                let word = apply_jitter_word(None, &mut state);
                jitter_interval(base, word).as_secs_f64()
            })
            .collect();
        let min = intervals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = intervals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let spread = max - min;
        assert!(
            spread > band_width * 0.5,
            "fallback jitter spread {spread:.3} s is less than 50 % \
             of the band width {band_width:.3} s — \
             the Weyl-sequence counter is not well-distributed"
        );
    }

    /// The warn-once guard: `warned` is false before the first failure and
    /// true afterwards; calling apply_jitter_word with Some(_) never sets it.
    #[test]
    fn warn_once_guard_transitions() {
        let mut state = FallbackState::new();
        assert!(!state.warned, "warned must start false");

        // Some(_) path must not touch the warned flag.
        apply_jitter_word(Some(42), &mut state);
        assert!(!state.warned, "Some(_) must not set warned");

        // First None: warned becomes true.
        apply_jitter_word(None, &mut state);
        assert!(state.warned, "first None must set warned");

        // Second None: still true (no toggle).
        apply_jitter_word(None, &mut state);
        assert!(state.warned, "subsequent None must leave warned true");
    }

    /// When getrandom succeeds (Some path), apply_jitter_word returns the
    /// exact word passed in and does not touch the fallback counter.
    #[test]
    fn success_path_returns_exact_word() {
        let mut state = FallbackState::new();
        let counter_before = state.counter;
        let result = apply_jitter_word(Some(0xDEAD_BEEF), &mut state);
        assert_eq!(result, 0xDEAD_BEEF);
        assert_eq!(
            state.counter, counter_before,
            "counter must be unchanged on success path"
        );
    }

    // ── jitter_interval contract ───────────────────────────────────────────

    #[test]
    fn jitter_interval_min_max_bounds() {
        let base = Duration::from_secs(10);
        // urandom=0 → factor=0.5 → 5 s
        assert_eq!(jitter_interval(base, 0), Duration::from_secs_f64(5.0));
        // urandom=u32::MAX → factor=1.5 → 15 s
        let v = jitter_interval(base, u32::MAX);
        assert!(
            v >= Duration::from_secs_f64(14.999) && v <= Duration::from_secs_f64(15.001),
            "got {:?}",
            v
        );
    }

    #[test]
    fn spawn_emits_at_least_once() {
        use std::sync::atomic::AtomicUsize;
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let handle = RtcpReporterHandle::spawn(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });
        // Wait up to 10 s for the first emission (base = 5 s + ±50%, so worst-case 7.5 s).
        let start = std::time::Instant::now();
        while counter.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(counter.load(Ordering::Relaxed) >= 1);
        drop(handle);
    }

    #[test]
    fn drop_cancels_promptly() {
        let handle = RtcpReporterHandle::spawn(|| {});
        let start = std::time::Instant::now();
        drop(handle);
        // Cancel + join should be < 500 ms (100 ms wake + slack).
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
