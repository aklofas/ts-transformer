//! [`RistStats`] — sender + receiver stats projections.

use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};

use tst_core::transport::SocketStats;

/// Run a closure exactly once when dropped — including on unwind. Used to
/// free librist resources on *every* constructor/trampoline exit path (normal,
/// early-return, panic), outside the `catch_unwind` boundary.
///
/// Call [`OnDrop::disarm`] to prevent the closure from running on a success
/// path where ownership has been transferred (e.g., a `rist_ctx` pointer
/// moved into a struct).
pub(crate) struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    pub(crate) fn new(f: F) -> Self {
        OnDrop(Some(f))
    }

    /// Cancel the deferred action. After `disarm()`, dropping this guard is a
    /// no-op. Use this on the success path once ownership is transferred.
    pub(crate) fn disarm(&mut self) {
        let _ = self.0.take();
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// librist stats-callback interval, milliseconds. 1 s matches librist's own
/// default cadence and is ample for cumulative counters polled on demand.
pub(crate) const STATS_INTERVAL_MS: i32 = 1000;

/// Cumulative stats for a single RIST transport handle.
///
/// librist exposes much richer counters via its `rist_stats` callback (sent /
/// received / retransmitted / dropped / RTT / bandwidth) — those are surfaced
/// here as a flat struct after periodic polling.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RistStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_retransmitted: u64,
    /// Packets that never reached the application. Aggregates librist's
    /// wire-loss count (the `lost` counter from the receiver stats callback)
    /// with packets librist delivered but this transport dropped at the app
    /// boundary (a datagram larger than the caller's buffer, or a malformed
    /// null-payload block). These are disjoint sets, so there is no
    /// double-counting; the wire-loss term normally dominates.
    pub packets_dropped: u64,
    /// Smoothed bandwidth, kbps.
    pub bandwidth_kbps: u32,
    /// Smoothed RTT, microseconds.
    pub rtt_us: u32,
}

impl RistStats {
    /// Project to the workspace-uniform [`SocketStats`].
    ///
    /// `SocketStats` is `#[non_exhaustive]`. Both the struct-expression-with-
    /// `..Default::default()` form and the bare default-and-field-assign form
    /// work from this crate (we're outside `tst-core`'s defining scope, but
    /// `..Default::default()` is still permitted at the boundary). The
    /// default-and-assign form is more concise here.
    pub fn to_socket_stats(&self) -> SocketStats {
        let mut s = SocketStats::default();
        s.bytes_sent = self.bytes_sent;
        s.packets_sent = self.packets_sent;
        s.bytes_received = self.bytes_received;
        s.packets_received = self.packets_received;
        s.packets_retransmitted = self.packets_retransmitted;
        s.rtt_us = self.rtt_us;
        // bandwidth is role-neutral → link_bandwidth_bps; packets_dropped is
        // only ever non-zero on the receiver (sender stats never set it), so
        // packets_dropped_recv is the correct sink for both roles.
        s.link_bandwidth_bps = (self.bandwidth_kbps as u64) * 1000;
        s.packets_dropped_recv = self.packets_dropped;
        s
    }
}

/// Fold one librist `rist_stats` sample into the cumulative [`RistStats`].
///
/// This is the pure, panic-free core of the stats callback — separated from
/// [`stats_trampoline`] so it can be unit-tested directly against stack-built
/// `rist_stats` data. The trampoline frees the librist-owned container via an
/// [`OnDrop`] guard that lives outside `catch_unwind`, covering all exit paths
/// (normal, early-return, panic). The trampoline can therefore only be
/// exercised against a genuine librist-owned container in a live session —
/// that path is verified by code review against the librist contract, not a
/// unit test.
///
/// Counter semantics (librist contract): the per-packet fields
/// (`retransmitted` / `lost`) are **per-interval deltas** that librist zeroes
/// after each callback, so we **accumulate** them with `+=`; `bandwidth` and
/// `rtt` are **gauges**, so we **overwrite**. We deliberately do NOT touch
/// `packets_sent` / `packets_received` / `bytes_*` here — those stay exact on
/// the inline data path, so touching them here would double-count. `rtt` is
/// integer milliseconds in librist, so sub-millisecond RTT truncates to 0 µs.
fn accumulate_stats(acc: &mut RistStats, s: &rist_sys::rist_stats) {
    match s.stats_type {
        rist_sys::rist_stats_type_RIST_STATS_SENDER_PEER => {
            // SAFETY: stats_type discriminates the union; sender_peer is active.
            let sp = unsafe { s.stats.sender_peer };
            acc.packets_retransmitted = acc.packets_retransmitted.wrapping_add(sp.retransmitted);
            acc.bandwidth_kbps = (sp.bandwidth / 1000) as u32;
            acc.rtt_us = sp.rtt.saturating_mul(1000);
        }
        rist_sys::rist_stats_type_RIST_STATS_RECEIVER_FLOW => {
            // SAFETY: stats_type discriminates the union; receiver_flow is active.
            let rf = unsafe { s.stats.receiver_flow };
            acc.packets_dropped = acc.packets_dropped.wrapping_add(rf.lost as u64);
            acc.bandwidth_kbps = (rf.bandwidth / 1000) as u32;
            acc.rtt_us = rf.rtt.saturating_mul(1000);
        }
        _ => {}
    }
}

/// librist stats callback. Runs on librist's internal protocol thread.
///
/// `arg` is a leaked `Arc<Mutex<RistStats>>` ref (reclaimed in the transport's
/// `close()` after `rist_destroy`, which joins the protocol thread so no
/// callback can be in flight). This thin wrapper only BORROWS the Arc (`&*ptr`,
/// never drops it), folds the sample in via [`accumulate_stats`], then frees the
/// container via an [`OnDrop`] guard that lives **outside** `catch_unwind`:
/// once a callback is registered librist transfers ownership of the
/// heap-allocated container to us and skips its own `rist_stats_free`, so we
/// MUST free it on every exit path.
///
/// Exit paths and free semantics:
/// 1. Normal completion — guard drops → free.
/// 2. `stats == null` → no guard created, no free (nothing to free).
/// 3. `arg == null` early-return inside `catch_unwind` — guard drops after
///    `catch_unwind` returns → free.
/// 4. Panic in `accumulate_stats` or lock — caught by `catch_unwind`, guard
///    drops after → free.
pub(crate) extern "C" fn stats_trampoline(
    arg: *mut c_void,
    stats: *const rist_sys::rist_stats,
) -> c_int {
    // Free the librist-owned container on EVERY exit path (normal,
    // null-arg early return, panic), but only when `stats` is non-null.
    // The guard lives OUTSIDE `catch_unwind` so it drops after the unwind
    // boundary — the free is the documented librist ownership contract.
    // SAFETY: librist transferred ownership of the heap container to this
    // callback; `stats` is `*const`, matching `rist_stats_free`.
    let _free = if stats.is_null() {
        None
    } else {
        Some(OnDrop::new(move || unsafe {
            rist_sys::rist_stats_free(stats);
        }))
    };

    let _ = std::panic::catch_unwind(|| {
        if arg.is_null() || stats.is_null() {
            return;
        }
        // SAFETY: `arg` is the pointer from `Arc::into_raw` at registration,
        // still live (reclaimed only at close, after rist_destroy joins this
        // thread). Borrow — do NOT reconstruct/drop the Arc.
        let lock = unsafe { &*(arg as *const Mutex<RistStats>) };
        // SAFETY: librist passes a valid container for the duration of this call.
        let s = unsafe { &*stats };
        if let Ok(mut acc) = lock.lock() {
            accumulate_stats(&mut acc, s);
        }
        // NB: NO free here — the `_free` guard above owns the free on all paths.
    });
    0
}

/// Create the shared stats accumulator and register the librist stats callback
/// on `ctx`.
///
/// Call this BEFORE `rist_start`. librist >= 0.2.20's protocol thread re-reads
/// the stats interval lock-free on every loop tick (deliberately, so a
/// late registration takes effect), while `rist_stats_callback_set` writes it
/// under librist's `stats_lock` — registering after the thread is running is
/// a data race that TSan reports on the tst-rist loopback tests. Pre-start
/// registration is also how every librist reference tool
/// orders it; the interval propagates to receiver flows created later.
///
/// Returns `(arc, raw)`: store `arc` in the transport for [`stats()`] snapshots,
/// and reclaim `raw` (`Arc::from_raw`) EXACTLY ONCE in `close()` AFTER
/// `rist_destroy` (which joins the protocol thread, so no callback can be in
/// flight). One Arc ref is leaked into librist as the callback `arg`; a non-zero
/// rc from `rist_stats_callback_set` is non-fatal — stats just won't populate,
/// and the leaked ref is still reclaimed at close.
///
/// [`stats()`]: crate::transport::RistTransport::stats
pub(crate) fn register_stats_callback(
    ctx: *mut rist_sys::rist_ctx,
) -> (Arc<Mutex<RistStats>>, *mut c_void) {
    let stats = Arc::new(Mutex::new(RistStats::default()));
    let stats_arg = Arc::into_raw(stats.clone()) as *mut c_void;
    // SAFETY: ctx is a live rist_ctx (created, not yet started — see the doc
    // above); stats_arg is a live leaked Arc ref that
    // outlives registration (reclaimed only at close, after rist_destroy).
    let _rc = unsafe {
        rist_sys::rist_stats_callback_set(ctx, STATS_INTERVAL_MS, Some(stats_trampoline), stats_arg)
    };
    (stats, stats_arg)
}

#[cfg(test)]
mod free_guard_tests {
    use super::OnDrop;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn guard_fires_once_on_normal_exit() {
        let n = AtomicUsize::new(0);
        {
            let _g = OnDrop::new(|| {
                n.fetch_add(1, Ordering::SeqCst);
            });
            // normal scope exit
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_fires_once_when_inner_returns_early() {
        let n = AtomicUsize::new(0);
        let run = |early: bool| {
            let _g = OnDrop::new(|| {
                n.fetch_add(1, Ordering::SeqCst);
            });
            if !early {
                // ... rest of body that we skip
            }
        };
        run(true);
        assert_eq!(
            n.load(Ordering::SeqCst),
            1,
            "early return still drops the guard"
        );
    }

    #[test]
    fn guard_fires_once_on_panic() {
        let n = AtomicUsize::new(0);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _g = OnDrop::new(|| {
                n.fetch_add(1, Ordering::SeqCst);
            });
            panic!("boom");
        }));
        assert_eq!(
            n.load(Ordering::SeqCst),
            1,
            "panic-unwind still drops the guard exactly once"
        );
    }
}

#[cfg(test)]
mod trampoline_tests {
    use super::*;

    #[test]
    fn sender_deltas_accumulate_gauges_overwrite() {
        let mk = |retrans: u64, bw_bps: usize, rtt_ms: u32| {
            let mut sp: rist_sys::rist_stats_sender_peer = unsafe { core::mem::zeroed() };
            sp.retransmitted = retrans;
            sp.bandwidth = bw_bps;
            sp.rtt = rtt_ms;
            let mut s: rist_sys::rist_stats = unsafe { core::mem::zeroed() };
            s.stats_type = rist_sys::rist_stats_type_RIST_STATS_SENDER_PEER;
            s.stats.sender_peer = sp;
            s
        };
        let mut acc = RistStats::default();
        accumulate_stats(&mut acc, &mk(3, 8_000, 12));
        accumulate_stats(&mut acc, &mk(2, 16_000, 7));
        assert_eq!(acc.packets_retransmitted, 5); // deltas accumulate
        assert_eq!(acc.bandwidth_kbps, 16); // gauge overwrites
        assert_eq!(acc.rtt_us, 7_000); // gauge overwrites (ms→µs)
        assert_eq!(acc.packets_sent, 0); // inline-only, untouched
    }

    #[test]
    fn receiver_lost_accumulates_into_dropped() {
        let mk = |lost: u32, bw_bps: usize, rtt_ms: u32| {
            let mut rf: rist_sys::rist_stats_receiver_flow = unsafe { core::mem::zeroed() };
            rf.lost = lost;
            rf.bandwidth = bw_bps;
            rf.rtt = rtt_ms;
            let mut s: rist_sys::rist_stats = unsafe { core::mem::zeroed() };
            s.stats_type = rist_sys::rist_stats_type_RIST_STATS_RECEIVER_FLOW;
            s.stats.receiver_flow = rf;
            s
        };
        let mut acc = RistStats::default();
        accumulate_stats(&mut acc, &mk(4, 1_000, 3));
        accumulate_stats(&mut acc, &mk(1, 2_000, 5));
        assert_eq!(acc.packets_dropped, 5); // lost deltas accumulate
        assert_eq!(acc.bandwidth_kbps, 2); // gauge overwrites
        assert_eq!(acc.rtt_us, 5_000); // gauge overwrites (ms→µs)
        assert_eq!(acc.packets_received, 0); // inline-only, untouched
    }

    #[test]
    fn unknown_stats_type_is_a_noop() {
        let mut s: rist_sys::rist_stats = unsafe { core::mem::zeroed() };
        s.stats_type = 99; // neither SENDER_PEER nor RECEIVER_FLOW → no-op arm
        let mut acc = RistStats::default();
        accumulate_stats(&mut acc, &s);
        // accumulate_stats only ever writes these four fields; the unknown arm
        // touches nothing, so all stay at their default zero. (RistStats has no
        // PartialEq — checking these four against an all-zero start is the full
        // "unchanged" proof without widening the public API.)
        assert_eq!(acc.packets_retransmitted, 0);
        assert_eq!(acc.packets_dropped, 0);
        assert_eq!(acc.bandwidth_kbps, 0);
        assert_eq!(acc.rtt_us, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection() {
        let r = RistStats {
            bytes_sent: 1000,
            packets_sent: 5,
            packets_retransmitted: 1,
            rtt_us: 12_345,
            bandwidth_kbps: 8,
            packets_dropped: 4,
            ..RistStats::default()
        };
        let s = r.to_socket_stats();
        assert_eq!(s.bytes_sent, 1000);
        assert_eq!(s.packets_sent, 5);
        assert_eq!(s.packets_retransmitted, 1);
        assert_eq!(s.rtt_us, 12_345);
        assert_eq!(s.link_bandwidth_bps, 8_000); // kbps → bps
        assert_eq!(s.packets_dropped_recv, 4);
    }

    #[test]
    fn default_is_all_zeros() {
        let r = RistStats::default();
        assert_eq!(r.bytes_sent, 0);
        assert_eq!(r.packets_dropped, 0);
        assert_eq!(r.rtt_us, 0);
    }
}
