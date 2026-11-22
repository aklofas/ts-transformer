//! `ManagedTransport<T>` — Transport decorator with reconnect + gap buffer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Wraps any inner Transport (most commonly `SrtTransport`); on send
//! failure with `Broken` semantics, queues the bytes in a fixed-size
//! gap buffer and attempts to re-establish the inner transport with
//! configurable backoff, either on the caller's thread
//! (`ReconnectMode::Blocking`, the default) or on a dedicated
//! per-outage worker thread (`ReconnectMode::Background`). On
//! reconnect success, drains the gap buffer before resuming new sends.

mod background;
mod gap_buffer;
mod silent_peer;
pub(crate) mod state;

pub use gap_buffer::{GapBuffer, OverflowPolicy};

use silent_peer::SilentPeer;
use state::{
    Install, Latch, ManagedShared, Shutdown, broken_errno, install_fresh_inner, retire_silent,
    sample_after_send,
};

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackoffStrategy {
    /// Fixed wait between attempts.
    Constant(Duration),
    /// Exponential: wait = base * 2^(attempt-1), capped at max.
    Exponential { base: Duration, max: Duration },
}

impl Default for BackoffStrategy {
    fn default() -> Self {
        BackoffStrategy::Exponential {
            base: Duration::from_millis(100),
            max: Duration::from_secs(10),
        }
    }
}

/// How `ManagedTransport` runs its reconnect loop after the inner
/// transport breaks. Send-side only: `ManagedRecvTransport` /
/// `ManagedDemuxReceiver` log a warning and behave as `Blocking` if
/// handed `Background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ReconnectMode {
    /// Reconnect on the caller's thread (the pre-0.6 behavior and the
    /// default). Simple; a sink outage blocks the caller inside
    /// `send_bytes` until reconnect succeeds or `max_attempts` runs out.
    #[default]
    Blocking,
    /// Reconnect on a background worker thread. `send_bytes` never waits
    /// on backoff, a factory call, or an in-flight inner send: while the
    /// inner transport is down it enqueues to the gap buffer under
    /// `overflow_policy`, waiting only for the buffer's own (short)
    /// critical sections. `Ok(())` means *accepted*, not *delivered* —
    /// pair with [`ManagedTransport::stats_handle`] for drop/reconnect
    /// visibility.
    Background,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReconnectPolicy {
    /// Maximum reconnect attempts before giving up. None = retry forever.
    /// Default: `Some(10)`.
    ///
    /// What one budget covers depends on the wrapper:
    ///
    /// - [`ManagedTransport`] (send): the factory calls of one outage. In
    ///   `Blocking` mode every `send_bytes` call that meets a broken
    ///   connection runs a budget of its own and returns at its first
    ///   successful reconnect. The `Background` worker starts a fresh
    ///   budget after every successful reconnect.
    ///
    ///   An armed sender (see [`ManagedTransport::with_silent_peer_timeout`])
    ///   counts every attempt of an outage until a new connection has
    ///   answered: a peer that accepts connections and never answers runs
    ///   out the budget like one that refuses them. A give-up, or an answer,
    ///   starts the next outage with a fresh budget.
    /// - `ManagedRecvTransport` (receive): every rebuild attempt of one
    ///   `recv_bytes` call, the ones that connected included. A connection
    ///   that breaks before it delivers anything uses one attempt, and the
    ///   count starts again with the next call.
    ///
    /// `None` with a zero backoff (a zero `Constant`, or an `Exponential`
    /// with a zero base or a zero cap) is refused by
    /// [`ReconnectPolicy::validate`] and by every managed constructor: it
    /// would retry in a tight loop with no pacing. A non-zero but tiny wait
    /// is accepted and spins: one retry per wait.
    pub max_attempts: Option<u32>,

    /// Backoff strategy between attempts. Default: exponential 100ms..=10s.
    pub backoff: BackoffStrategy,

    /// Gap-buffer capacity in messages. Default 256.
    ///
    /// Under [`OverflowPolicy::DropOldest`] the buffer may hold
    /// `capacity + 1` for the duration of one send: the message the
    /// background drain worker is currently writing is pinned against
    /// eviction, so when it is the only queued entry a new message is
    /// accepted rather than dropping bytes already handed to the
    /// transport. The next completed send brings the length back within
    /// `capacity`.
    ///
    /// A capacity of 0 buffers nothing, which only shows while the
    /// connection is down (a live connection is sent to directly). A
    /// `Blocking` send that meets the outage waits for the reconnect and
    /// then sends its message on the fresh connection, under either
    /// overflow policy: it is neither refused nor counted as dropped, and
    /// a reconnect that gives up returns `Broken` with the message still
    /// the caller's. A `Background` send is dropped and counted as it arrives
    /// under `DropOldest`, and refused with `Backpressure` under
    /// [`OverflowPolicy::Reject`]; the worker reconnects in both cases.
    pub gap_buffer_capacity: usize,

    /// What to do when gap buffer is full and a new message arrives.
    /// Default: drop oldest message.
    pub overflow_policy: OverflowPolicy,

    /// Reconnect-loop placement. Default: `ReconnectMode::Blocking`
    /// (reconnect runs on the caller's thread — pre-0.6 behavior).
    pub mode: ReconnectMode,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: Some(10),
            backoff: BackoffStrategy::default(),
            gap_buffer_capacity: 256,
            overflow_policy: OverflowPolicy::DropOldest,
            mode: ReconnectMode::Blocking,
        }
    }
}

/// Why a [`ReconnectPolicy`] was refused by [`ReconnectPolicy::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReconnectPolicyError {
    /// `max_attempts` is `None` and the backoff waits zero time (a zero
    /// `Constant`, or an `Exponential` with a zero base or a zero cap).
    #[error(
        "zero backoff with unlimited attempts would retry without pacing; set max_attempts or a non-zero backoff"
    )]
    UnpacedUnlimitedRetry,
}

impl ReconnectPolicy {
    /// Sets `max_attempts`; see the field's documentation.
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Sets `backoff`; see the field's documentation.
    #[must_use]
    pub fn with_backoff(mut self, backoff: BackoffStrategy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sets `gap_buffer_capacity`; see the field's documentation.
    #[must_use]
    pub fn with_gap_buffer_capacity(mut self, gap_buffer_capacity: usize) -> Self {
        self.gap_buffer_capacity = gap_buffer_capacity;
        self
    }

    /// Sets `overflow_policy`; see the field's documentation.
    #[must_use]
    pub fn with_overflow_policy(mut self, overflow_policy: OverflowPolicy) -> Self {
        self.overflow_policy = overflow_policy;
        self
    }

    /// Sets `mode`; see the field's documentation.
    #[must_use]
    pub fn with_mode(mut self, mode: ReconnectMode) -> Self {
        self.mode = mode;
        self
    }

    /// Check the policy is usable.
    ///
    /// # Errors
    ///
    /// [`ReconnectPolicyError::UnpacedUnlimitedRetry`] when `max_attempts` is
    /// `None` and the backoff is `Constant(0)` or an `Exponential` with a
    /// zero `base` or a zero `max`. A zero backoff with a bounded `max_attempts` is allowed.
    pub fn validate(&self) -> Result<(), ReconnectPolicyError> {
        let zero_wait = match &self.backoff {
            BackoffStrategy::Constant(d) => d.is_zero(),
            // `next_delay` caps every wait at `max`, so a zero cap is as
            // unpaced as a zero base.
            BackoffStrategy::Exponential { base, max } => base.is_zero() || max.is_zero(),
        };
        if self.max_attempts.is_none() && zero_wait {
            return Err(ReconnectPolicyError::UnpacedUnlimitedRetry);
        }
        Ok(())
    }

    /// Compute the wait before the next reconnect attempt, or `None` if the
    /// budget is exhausted.
    ///
    /// `attempt` is the 1-based index of the attempt about to be made (i.e.
    /// the very first reconnect after a transport break is `attempt = 1`).
    /// When `max_attempts == Some(n)`, returns `None` once `attempt > n`.
    /// When `max_attempts == None`, retries forever (always returns `Some`).
    ///
    /// Used by both `ManagedTransport` (send side) and
    /// `ManagedRecvTransport` (receive side) so the backoff math lives in
    /// one place.
    pub fn next_delay(&self, attempt: u32) -> Option<Duration> {
        if let Some(max) = self.max_attempts
            && attempt > max
        {
            return None;
        }
        let wait = match &self.backoff {
            BackoffStrategy::Constant(d) => *d,
            BackoffStrategy::Exponential { base, max } => {
                let exp = (*base).saturating_mul(1 << attempt.saturating_sub(1).min(20));
                if exp > *max { *max } else { exp }
            }
        };
        Some(wait)
    }
}

use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use tracing::{debug, info, warn};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{BrokenCause, Transport, TransportCancel, TransportError};

tst_core::binding_record! {
    /// Frozen reconnect and gap telemetry snapshot of a managed
    /// (auto-reconnect) sender.
    ///
    /// `reconnecting` is only ever true under the background reconnect mode.
    /// It is always false in the blocking mode, since that mode's reconnect
    /// loop runs synchronously inside the call that observed the break rather
    /// than on a separate worker this flag could observe as active.
    ///
    /// <!-- rust-only -->
    /// Snapshot of `ManagedTransport`'s reconnect/gap telemetry.
    ///
    /// **Stability: Stable** — see the
    /// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
    #[derive(Debug, Clone)]
    #[non_exhaustive]
    pub struct ManagedTransportStats {
        /// Total factory invocations across all reconnect cycles, successful and
        /// failed.
        pub reconnect_attempts: u64,
        /// Factory calls that returned a transport, which was installed as the
        /// new inner connection.
        pub reconnect_successes: u64,
        /// Messages currently queued in the gap buffer, awaiting drain once the
        /// inner transport reconnects.
        pub gap_len: u64,
        /// Messages evicted by the drop-oldest overflow policy, plus
        /// oversized-after-reconnect drops discovered during drain.
        pub gap_messages_dropped: u64,
        /// Bytes evicted by the same.
        pub gap_bytes_dropped: u64,
        /// True while a background reconnect worker is active.
        pub reconnecting: bool,
    }
}

/// Cloneable, `Send + Sync` observer for [`ManagedTransportStats`].
/// Obtain via [`ManagedTransport::stats_handle`] **before** moving the
/// transport into a sender shell (mirrors the `cancel_handle()` pattern).
#[derive(Clone)]
pub struct ManagedStatsHandle {
    gap: Arc<Mutex<GapBuffer>>,
    shared: Arc<ManagedShared>,
}

impl ManagedStatsHandle {
    /// Snapshot. Returns `None` only if the gap-buffer lock is poisoned
    /// (matches `socket_stats`'s None-on-poison shape — a read-only
    /// telemetry path must not panic).
    ///
    /// Takes only the gap-buffer lock, and neither the background worker
    /// nor the `Blocking` replay that follows a reconnect holds that
    /// across an inner send (locking invariant 4), so a snapshot is never
    /// queued behind a stalled sink in either mode. `gap_len` counts a
    /// message the drain is currently sending: it is queued until the
    /// send is acknowledged.
    ///
    /// # C ABI
    ///
    /// `tst_managed_sender_get_reconnect_stats` /
    /// `tst_managed_mux_sender_get_reconnect_stats` /
    /// `tst_managed_raw_sender_get_reconnect_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn stats(&self) -> Option<ManagedTransportStats> {
        use std::sync::atomic::Ordering;
        let gap = self.gap.lock().ok()?;
        Some(ManagedTransportStats {
            reconnect_attempts: self.shared.reconnect_attempts.load(Ordering::Relaxed),
            // `Acquire`, pairing with the `Release` increment in
            // `install_fresh_inner`: a caller that waits for this counter to
            // move and then sends must see the ceiling that reconnect
            // published, not the previous inner's.
            reconnect_successes: self.shared.reconnect_successes.load(Ordering::Acquire),
            gap_len: gap.len() as u64,
            gap_messages_dropped: gap.messages_dropped,
            gap_bytes_dropped: gap.bytes_dropped,
            reconnecting: self.shared.bg_active.load(Ordering::Acquire),
        })
    }
}

/// Decorator that wraps an inner `Transport` with reconnect + gap-buffer
/// behavior.
///
/// On `send_bytes` returning `TransportError::Broken`, the bytes go into
/// the gap buffer (subject to the configured overflow policy) and the
/// inner transport is rebuilt via the user-supplied factory closure.
/// After the inner transport reconnects, the gap buffer is drained
/// before resuming new sends.
///
/// A message stays in the gap buffer only while the wrapper still owes
/// its delivery. In `Blocking` mode that is the span of one `send_bytes`
/// call: `Ok(())` means it was delivered, and an `Err` hands it back to
/// the caller (nothing stays queued for a later call to deliver). In
/// `Background` mode `Ok(())` means accepted, and accepted messages stay
/// queued across calls until the worker delivers or evicts them.
///
/// `ManagedTransport` itself implements `Transport`, so all three sender
/// shells (`MuxSender`, `Sender`, `RawSender`) compose with it
/// transparently:
///
/// ```ignore
/// let factory = || SrtTransport::connect(...);
/// let inner = factory()?;
/// let managed = ManagedTransport::new(inner, factory, ReconnectPolicy::default()).expect("the policy validates");
/// let sender = MuxSender::new(managed, config)?;
/// // sender now silently reconnects on transport breakage
/// ```
///
/// # Reconnect modes
///
/// [`ReconnectPolicy::mode`] selects where the reconnect loop runs:
///
/// - **`ReconnectMode::Blocking`** (default; the pre-0.6 behavior) — the
///   reconnect loop runs synchronously on the caller's thread, with the
///   configured backoff, inline inside the `send_bytes` call that first
///   observed `Broken`. That call blocks until reconnect succeeds or the
///   policy's `max_attempts` budget is exhausted. The gap buffer holds
///   that one call's message and is empty again when the call returns,
///   so `gap_buffer_capacity` / `overflow_policy` do not matter here. At
///   capacity 0 nothing is staged: the call reconnects, then sends its
///   message on the fresh connection.
/// - **`ReconnectMode::Background`** — a per-outage worker thread owns
///   the factory/backoff/drain loop instead. While that worker is active,
///   or the gap buffer is non-empty, `send_bytes` never touches the inner
///   transport, never waits on backoff or a factory call, and enqueues
///   under `overflow_policy`. It never waits on the worker's in-flight
///   inner send either — that call is unbounded against a peer that stops
///   draining — only on the gap buffer's own short critical sections; the
///   same is true of [`ManagedStatsHandle::stats`]. **`Ok(())` in this
///   mode means the bytes were *accepted* into the gap buffer, not that
///   they were *delivered*** —
///   pair `Background` with [`Self::stats_handle`] to observe
///   `reconnecting` / `gap_len` / `gap_messages_dropped`. That counter
///   also counts a queued message that no longer fits the *rebuilt*
///   transport's `max_payload` — dropped during drain rather than
///   wedging it forever; `Blocking` mode would instead return
///   `TooLarge` synchronously to the caller for that same message.
///   `max_attempts` bounds one continuous outage of this sender (the
///   worker's budget resets after every successful reconnect, exactly as
///   it does per-call in `Blocking` mode, except for an armed sender, whose
///   outage lasts until a new connection has answered; a managed RECEIVER
///   counts differently, see [`ReconnectPolicy::max_attempts`]). If the worker
///   exhausts that budget — or terminates
///   abnormally (an unwind inside the factory, or an unrecoverable
///   poisoned lock) — the give-up is reported exactly once, as a single
///   `TransportError::Broken` on the next `send_bytes` call; that call's
///   own bytes are **not** queued — the caller sees the error and owns
///   the resend decision. [`Self::is_alive`] returns `true` while a
///   background worker is actively recovering (never `false`, which
///   would read as permanently dead rather than "recovering") — and
///   `false` once `close()` or `cancel()` has latched, regardless of
///   what the inner transport or a still-exiting worker reports.
///
/// # Silent-peer detection
///
/// [`Self::with_silent_peer_timeout`] arms a window. After a successful
/// inner send the wrapper reads the inner's [`Transport::peer_activity`]
/// (at every send until the connection has answered once, then every
/// quarter window, and at every send once a send has gone a whole window
/// unanswered); a reading that has not moved while sends kept going for a
/// whole window trips. The tripped connection is taken out,
/// closed with [`Transport::abandon`] once `inner` is released, and
/// replaced as if its send had failed with `Broken`. Unarmed, the send
/// path reads nothing.
///
/// The message whose send found the silence was accepted by the abandoned
/// connection; it is queued for the new connection under the gap buffer's
/// rules (handed back in `Blocking` mode if the reconnect gives up). A trip
/// turns a send the connection accepted into a failure: under a latched
/// cancel the cancel is reported and the caller keeps the message, since
/// nothing will send it again. A gap of half the window or more between two
/// sends restarts the count, so a sender that slow never trips.
///
/// # Locking
///
/// 1. Lock order where both are held: `inner` → `gap` (matches
///    `drain_gap_if_alive`). No code path acquires `inner` while holding
///    `gap`.
/// 2. `bg_active` transitions AND the send-path enqueue decision happen
///    under the `gap` lock (linearizes worker-exit vs. pump-enqueue; no
///    stranded bytes).
/// 3. Never call `spawn_worker()` while holding the `gap` lock (it joins
///    the previous worker, which may be blocked acquiring `gap` in its
///    exit path).
/// 4. Neither the background worker nor the `Blocking` drain holds `gap`
///    across an inner send, and nothing on the producer's path takes
///    `inner`: neither
///    `send_bytes`'s size pre-check nor [`Transport::max_payload`] —
///    which `RawSender`/`MuxSender` call on every send — both read the
///    cached ceiling instead. One inner send is unbounded against a peer
///    that stops draining, so any of those would stall the producer (or
///    `stats()`) for the length of that send. The front message is
///    pinned against `DropOldest` eviction by the gap buffer's own
///    in-flight mark (`begin_send` / `finish_send` / `abort_send`), not
///    by the lock. `inner` IS held across the drain send — the transport
///    needs `&mut` — which is exactly why the producer's Background path
///    must not take it.
/// 5. The cancel path takes NEITHER lock: it fires the `active` cancel
///    slot, which publishes the live inner's wake handle. A cancel is only
///    useful while a send is in flight — i.e. exactly while `inner` is
///    held — so reading the handle out of `inner` would queue the cancel
///    behind the call it was asked to interrupt.
/// 6. The silent-peer rule's lock (see "Silent-peer detection") is a leaf:
///    taken only while `inner` is held, and no lock is taken while it is.
///    A poisoned one is recovered: the rule's arithmetic cannot panic.
///    The Background enqueue branch takes neither `inner` nor this lock.
///
/// # Closing
///
/// [`Self::close`] (via the `Transport::close` trait method), `Drop`, and
/// [`Self::cancel_handle`]'s `cancel()` all run the same terminal
/// transition: latch closed, wake a backoff wait, and fire the live
/// inner's wake handle. `close()` then joins any active background
/// worker; that join is bounded by an in-flight `factory()` call, or an
/// in-flight [`Transport::abandon`] of a connection the worker gave up on
/// (which must not wait), at most — a drain send parked in the inner
/// transport is interrupted by the fired handle, and backoff waits are
/// interruptible in both modes.
/// `Drop` never blocks: it fires the same signal and detaches, leaving
/// the worker to observe it and exit on its own.
///
/// An inner transport that reports `TransportError::ExplicitClose` runs the
/// same transition. An inner says that only when it was cancelled, and if
/// the wrapper's latch is not set yet the cancel did not come through the
/// wrapper: a handle taken from the inner before it was wrapped, or a
/// process-exit path that closes every open socket directly. Neither is an
/// outage, so no reconnect follows. Whichever of the direct send, the drain
/// after a reconnect, the background worker's drain, or the factory itself
/// reports it, the wrapper latches as if its own handle had been fired and
/// reports `ExplicitClose` from then on. `Blocking`: the call that ran into
/// it returns the error and its message goes back to the caller.
/// `Background`: the worker exits and the next `send_bytes` reports it;
/// messages accepted earlier stay queued and undelivered (`gap_len` keeps
/// counting them; they are not counted as dropped), as they do after a
/// cancel through the wrapper's own handle.
///
/// # Lock poisoning policy
///
/// - **Inner-transport lock** (poisoned mid-mutation):
///   - `send_bytes`: returns `TransportError::Broken { .. }`. Caller can
///     rebuild the wrapper.
///   - `max_payload`: takes no `inner` lock at all — poison-immune by
///     construction (it reads the cached ceiling of the last installed
///     inner; see locking invariant 4). Same for `send_bytes`'s size
///     pre-check.
///   - `is_alive`: returns `false` (the same "no live transport" default
///     — no panic).
///   - `close`: silent no-op; the `closed` flag is already latched before
///     the lock attempt, so all subsequent operations exit cleanly — no
///     panic.
///   - `cancel_handle` (and the returned handle's `cancel()`): clones
///     `Arc`s only — no `inner` lock taken, poison-immune by construction.
///     The `active` cancel slot it fires recovers its own lock on poison.
///   - `socket_stats`: returns `None` on poison (pre-existing
///     shape; `lock().ok()` silently swallows the error the same way
///     `None` inner is handled).
/// - **Gap-accumulator lock** (poisoned mid-mutation): `send_managed` /
///   `drain_gap_if_alive` panic with `BUG: gap lock poisoned ...` because
///   the gap buffer holds bytes queued for replay — silently routing past
///   the poison would lose those bytes. `tst-c`'s `ffi_catch` wraps to
///   `TST_E_PANIC_CAUGHT` (-11).
pub struct ManagedTransport<T: Transport> {
    inner: Arc<Mutex<Option<T>>>,
    factory: Arc<dyn Fn() -> Result<T, TransportError> + Send + Sync>,
    policy: ReconnectPolicy,
    gap: Arc<Mutex<GapBuffer>>,
    /// The `closed`/`cancelled` latch pair (see `Latch`): `closed` is set by
    /// `cancel_handle().cancel()`, `close()` and `Drop`; `cancelled` only by
    /// a cancel, so a cancel is reported as `ExplicitClose` and an own close
    /// as `Closed`. The reconnect loop checks `closed` each iteration so a
    /// cancel mid-retry breaks out instead of waiting through the backoff.
    latch: Arc<Latch>,
    /// Wakes any backoff wait (blocking loop or background worker) when
    /// `close()` / `cancel()` / `Drop` latch shutdown. `closed` stays the
    /// semantic flag; this is the wakeup channel.
    shutdown: Arc<Shutdown>,
    /// Reconnect/gap telemetry and background-worker coordination flags,
    /// read by any `ManagedStatsHandle` obtained via `stats_handle()`.
    shared: Arc<ManagedShared>,
    /// Most recent background worker, for `close()` to join. One worker
    /// per outage — spawned on break, exits when the gap drains or the
    /// budget exhausts.
    bg_thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// The cancel handle that can currently unblock a send: the live
    /// inner's, installed at construction and on every successful rebuild
    /// (either mode), cleared when the inner is torn down. Published here
    /// rather than read back out of `inner` because every send path holds
    /// `inner` across the inner `send_bytes` — see locking invariant 5.
    /// The slot latches, so a cancel racing a reconnect fires the fresh
    /// inner's handle the moment it is installed.
    active: Arc<CancelSlot>,
    /// The silent-peer rule, `None` while unarmed. Its lock is a leaf
    /// (locking invariant 6). Not in `ManagedShared`: a stats handle may
    /// already share that when the sender is armed.
    silent: Option<Arc<Mutex<SilentPeer>>>,
}

impl<T: Transport + 'static> ManagedTransport<T> {
    /// Wrap `inner`, rebuilding it with `factory` under `policy`.
    ///
    /// When a reconnect attempt fails, the error `factory` returned is
    /// written to the log as text (at `warn`, for every failed attempt, and
    /// the latest one again when the reconnect gives up). It must not carry
    /// a secret: no passphrase, no URL with credentials in its message
    /// (`tst_core::url::redact_url` strips them from a URL).
    ///
    /// # Errors
    ///
    /// [`ReconnectPolicyError::UnpacedUnlimitedRetry`] when the policy has
    /// zero backoff and unlimited attempts; `inner` is dropped and nothing
    /// else is allocated.
    pub fn new<F>(
        inner: T,
        factory: F,
        policy: ReconnectPolicy,
    ) -> Result<Self, ReconnectPolicyError>
    where
        F: Fn() -> Result<T, TransportError> + Send + Sync + 'static,
    {
        policy.validate()?;
        let gap = GapBuffer::new(policy.gap_buffer_capacity, policy.overflow_policy);
        let active = Arc::new(CancelSlot::new());
        if let Some(h) = inner.cancel_handle() {
            active.install(h);
        }
        let shared = Arc::new(ManagedShared::default());
        // Publish the initial ceiling for the lock-free size pre-check;
        // `install_fresh_inner` republishes it on every rebuild.
        shared
            .max_payload
            .store(inner.max_payload(), std::sync::atomic::Ordering::Relaxed);
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(inner))),
            factory: Arc::new(factory),
            policy,
            gap: Arc::new(Mutex::new(gap)),
            latch: Arc::new(Latch::new()),
            shutdown: Arc::new(Shutdown::new()),
            shared,
            bg_thread: Mutex::new(None),
            active,
            silent: None,
        })
    }

    /// Arm silent-peer detection: when none of this sender's sends has
    /// been answered by its peer for `window`, the connection is treated as
    /// broken, abandoned without waiting for its unsent data
    /// ([`Transport::abandon`]) and replaced through the reconnect this
    /// wrapper already has.
    ///
    /// **Stability: Provisional** — see the
    /// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
    ///
    /// What counts as an answer is a change of the inner's
    /// [`Transport::peer_activity`] reading, taken after successful sends.
    /// An inner whose reading is `None` cannot tell: the sender never trips,
    /// and keeps its ordinary reconnect budget once the wrapper has read a
    /// new connection after a successful send on it. Not every send is
    /// followed by a reading: the replay of the gap buffer right after a
    /// `Blocking` reconnect is not, so a connection that takes the replay
    /// and breaks at the next send was never read, and its attempts count
    /// on, as for a connection never heard from.
    ///
    /// **Call it before the first send.** A `Background` worker started by
    /// an earlier send carries no rule, and drains unchecked until the next
    /// outage.
    ///
    /// `Duration::ZERO` leaves the sender unarmed. There is no floor here
    /// (the `x-silentpeertimeout` URL key's is 1 000 ms); a window shorter
    /// than a few send periods never trips (see the gap rule below).
    ///
    /// Limits:
    ///
    /// - Silence is judged against sends that succeeded: nothing is
    ///   detected while the caller sends nothing, while it is parked inside
    ///   the inner send, or while its sends return `Backpressure` (those are
    ///   not sampled).
    /// - A gap of `window / 2` or more between two sends (a burst every
    ///   `window / 2` or more, or a send that itself blocks `window / 2` or
    ///   more) restarts the count, so such a sender never trips.
    /// - Silence is detected, not low throughput: a peer that answers just
    ///   often enough keeps the connection.
    /// - What the abandoned connection still held is lost: everything sent
    ///   between the moment the peer went silent and the trip, about
    ///   `window` plus a fraction of it. No statistic counts it.
    /// - **The window must be longer than the link's round trip** plus one
    ///   send period: an answer arrives a round trip (plus the peer's
    ///   acknowledgement interval and jitter) after a send. With a shorter
    ///   window every new connection is abandoned before its first answer
    ///   can arrive, a working link is re-dialled every window, and, as no
    ///   connection answers, `max_attempts` ends it with a give-up (with
    ///   `None`, never).
    /// - A window at or above the inner's own break detection never fires
    ///   first (libsrt breaks a silent connection after about 5 s unless
    ///   its peer-idle timeout is raised).
    /// - A peer that answers the first packet of every connection and then
    ///   goes silent: in `Background` mode, and in `Blocking` mode at gap
    ///   capacity 0, the first send on each new connection is read, its
    ///   answer is seen, the budget is given back, and the sender re-dials
    ///   about every window plus one backoff step without end:
    ///   `max_attempts` does not end it. In `Blocking` mode with a gap
    ///   buffer the replay after the reconnect is not read: when its answer
    ///   is already in at the caller's next send (a round trip shorter than
    ///   the send period), it is that connection's starting value, the
    ///   connection never answered, its attempts count on and `max_attempts`
    ///   ends it with a give-up; with a longer round trip the answer is seen
    ///   and the re-dials are endless as in the other modes.
    /// - A transport whose reading is `None` once its connection has broken
    ///   (SRT's is) can read `None` at a new connection's first reading if
    ///   the connection broke just after a successful send: that counts as
    ///   "cannot tell" and gives the reconnect budget back once.
    #[must_use]
    pub fn with_silent_peer_timeout(mut self, window: Duration) -> Self {
        self.silent = (!window.is_zero()).then(|| Arc::new(Mutex::new(SilentPeer::new(window))));
        self
    }

    /// Obtain a cloneable stats observer. Call **before** moving this
    /// transport into `MuxSender`/`Sender`/`RawSender` — the shell takes
    /// ownership, but the handle keeps reading live counters (same
    /// pattern as `cancel_handle()`).
    pub fn stats_handle(&self) -> ManagedStatsHandle {
        ManagedStatsHandle {
            gap: Arc::clone(&self.gap),
            shared: Arc::clone(&self.shared),
        }
    }

    /// Shared handle to the factory-call counter — every `factory()`
    /// invocation, either mode, successful or not (the value
    /// [`ManagedTransportStats::reconnect_attempts`] snapshots), exposed as
    /// a lock-free `Arc` so a binding can read it after this transport has
    /// moved into a sender shell. Obtain **before** the move;
    /// read with `.load(Ordering::Relaxed)`, the ordering the bump and
    /// [`ManagedStatsHandle::stats`] already use for it — the counter is a
    /// statistic and publishes nothing, unlike
    /// [`Self::reconnects_handle`]. The counter lives outside both the
    /// `inner` and `gap` mutexes, so — unlike [`ManagedStatsHandle::stats`],
    /// which takes the gap lock — reading through this handle can never
    /// queue behind a send, however long the parked one runs. Receive-side
    /// twin: [`crate::ManagedRecvTransport::attempts_handle`].
    #[must_use]
    pub fn attempts_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.shared.reconnect_attempts)
    }

    /// Shared handle to the successful-rebuild counter
    /// ([`ManagedTransportStats::reconnect_successes`]), lock-free like
    /// [`Self::attempts_handle`] but read with `.load(Ordering::Acquire)`:
    /// unlike the attempts counter this one is a publication point (a
    /// reader that sees the bump also sees the rebuilt transport's payload
    /// ceiling). Receive-side twin
    /// [`crate::ManagedRecvTransport::reconnects_handle`].
    #[must_use]
    pub fn reconnects_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.shared.reconnect_successes)
    }

    /// Shared handle to the "background worker active" flag
    /// ([`ManagedTransportStats::reconnecting`]): `true` only while a
    /// [`ReconnectMode::Background`] worker owns reconnect + drain. Always
    /// `false` in `Blocking` mode — the inline loop runs on the caller's
    /// thread, so there is no window in which another thread could observe
    /// it. Same contract as [`Self::attempts_handle`].
    #[must_use]
    pub fn reconnecting_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.shared.bg_active)
    }

    /// Try to send via the inner transport. On Broken/Closed, queue bytes
    /// and attempt reconnect.
    ///
    /// Pre-checks `bytes.len() > max_payload` against the last published
    /// inner ceiling before any state mutation, so oversized messages
    /// never enter the gap buffer (where they'd block drain forever).
    /// The check reads a published atomic rather than the `inner` lock —
    /// see locking invariant 4.
    ///
    /// `Blocking` mode: an `Err` from this call never leaves the call's
    /// message in the gap buffer. Whether the drain that follows a
    /// reconnect was refused (`Backpressure`, `TooLarge`), broke again, or
    /// the reconnect gave up, the message is taken back out before the
    /// error is returned — the caller that was told it failed is its only
    /// owner, so a retry (the caller's, or the one `MuxSender` / `Sender`
    /// make on their own) cannot put it on the wire twice.
    ///
    /// A cancel that interrupts the direct inner send is reported by this
    /// call as `ExplicitClose`, in either mode, and the interrupted bytes
    /// are not queued. A send that completed before the cancel landed
    /// returns `Ok(())`; the next call reports the cancel.
    ///
    /// A cancel that lands mid-drain, right after a factory reconnect,
    /// surfaces here as `Err(TransportError::ExplicitClose)` rather than
    /// the drain's own wire-looking `Broken`. In `Background` a cancel that
    /// lands after the entry gate let this call in, and before it reaches
    /// the gap buffer, is reported by this call too, and its bytes are not
    /// queued. A cancel that instead lands
    /// after the drain has already fully succeeded also returns
    /// `ExplicitClose` on this call — the entry gate above would return it
    /// on the next one anyway. The wrapper's OWN `close()`/`Drop` reports
    /// `Closed` at every one of those exits (`Latch::error`).
    ///
    /// # Panics
    ///
    /// Panics with `"BUG: gap lock poisoned — gap buffer is invariant-critical"`
    /// if the internal gap-buffer mutex has been poisoned by a previous panic.
    /// The poison signals a corrupted gap-buffer invariant (length tracking,
    /// ring cursors); proceeding would silently drop queued bytes. Caught by
    /// `tst-c`'s `ffi_catch` as `TST_E_PANIC_CAUGHT` (-11). The
    /// recoverable-path lock poison (the inner-transport mutex) instead
    /// returns `Err(TransportError::Broken { .. })`.
    fn send_managed(&self, bytes: &[u8]) -> Result<(), TransportError> {
        if self.latch.is_closed() {
            return Err(self.latch.error());
        }
        if self.policy.mode == ReconnectMode::Background {
            // Report a completed give-up cycle exactly once. This call's
            // bytes are NOT queued — the caller saw the error and owns the
            // resend decision. The report refers to the most recently
            // exhausted cycle; the next call starts a fresh one.
            if self
                .shared
                .gave_up
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                // Abnormal (worker unwound, or hit an unrecoverable inner
                // poison) vs. the normal budget-exhausted give-up: a
                // max_attempts: None policy can still abnormally give up
                // (a panic ignores the budget entirely), so this must not
                // default to the budget-phrased message.
                #[cfg(test)]
                cancel_window::run();
                // A cancel that landed since the entry gate is what this
                // call reports; the give-up was already consumed, and the
                // next call reports the cancel as well.
                if self.latch.is_closed() {
                    return Err(self.latch.error());
                }
                let abnormal = self
                    .shared
                    .gave_up_abnormal
                    .swap(false, std::sync::atomic::Ordering::AcqRel);
                let msg = if abnormal {
                    "background reconnect aborted (worker terminated abnormally)".to_string()
                } else {
                    let max = self.policy.max_attempts.unwrap_or(0);
                    format!("reconnect gave up after {max} attempts")
                };
                return Err(TransportError::Broken {
                    msg,
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
        }
        // Pre-check size before queuing — oversized messages would
        // otherwise sit in the gap buffer and fail every drain.
        //
        // Read from the published ceiling, NOT the `inner` lock: the
        // background worker holds `inner` across one drain send, and that
        // one send is unbounded against a peer that stops draining, so
        // locking here would make the Background contract ("send always
        // enqueues") false exactly when it matters. The value is
        // republished at construction and on every successful install; a
        // ceiling that shrank after a message was queued is caught by the
        // drain's own `TooLarge` handling: in `Blocking` the error reaches
        // the caller, who owns the message and re-splits it; in
        // `Background` the worker drops it and counts the drop.
        let max = self
            .shared
            .max_payload
            .load(std::sync::atomic::Ordering::Relaxed);
        if bytes.len() > max {
            return Err(TransportError::TooLarge {
                len: bytes.len(),
                max,
            });
        }

        if self.policy.mode == ReconnectMode::Background {
            // Background invariant: worker active or gap non-empty =>
            // always enqueue, never touch inner. Preserves FIFO (a direct
            // send would leapfrog queued bytes) and keeps send latency
            // independent of reconnect/drain. Checked under the gap lock
            // to linearize with the worker's Empty-exit (invariant 2).
            #[cfg(test)]
            before_gap_lock::run();
            let need_worker = {
                let mut gap = self
                    .gap
                    .lock()
                    .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                // The entry gate ran before this lock was taken, and the
                // cancel path takes no lock (invariant 5), so the latch can
                // have been set in between. This load, under the gap lock,
                // is what the call reports on: a cancel observed here is
                // reported by this call; a cancel that lands after it is
                // reported by the next call, and (on the enqueue branch)
                // this message stays queued undelivered — the same outcome
                // as a cancel landing just after `send_bytes` returned. The
                // canceller takes no lock, so nothing stronger (real-time
                // ordering of enqueue against the latch) is promised.
                // Without this check the message went to a worker that only
                // sees the latch and exits, and the caller was told
                // `Ok(())`. Ahead of the enqueue, so it also outranks a full
                // `Reject` buffer's `Backpressure` (same order as the
                // enqueue after the direct send, below).
                if self.latch.is_closed() {
                    return Err(self.latch.error());
                }
                let worker_active = self
                    .shared
                    .bg_active
                    .load(std::sync::atomic::Ordering::Acquire);
                if !worker_active && gap.is_empty() {
                    None // fall through to the direct path below
                } else {
                    // Admission and recovery are separate: a message refused
                    // by a full `Reject` buffer still leaves a backlog that
                    // needs its worker.
                    let refused = matches!(
                        gap.enqueue(bytes.to_vec()),
                        Err(gap_buffer::GapBufferError::Full)
                    );
                    let restart = if worker_active {
                        false
                    } else {
                        // Backlog with no worker (post-give-up): start a
                        // fresh cycle. Flag set under the gap lock.
                        self.shared
                            .bg_active
                            .store(true, std::sync::atomic::Ordering::Release);
                        true
                    };
                    Some((restart, refused))
                }
            }; // gap lock dropped — never spawn while holding it (invariant 3)
            if let Some((restart, refused)) = need_worker {
                if restart {
                    self.spawn_worker();
                }
                if refused {
                    #[cfg(test)]
                    cancel_window::run();
                    // A cancel that landed since the check under the gap lock
                    // is what this refused call reports.
                    if self.latch.is_closed() {
                        return Err(self.latch.error());
                    }
                    return Err(TransportError::Backpressure {
                        msg: "gap buffer full".into(),
                        errno_code: None,
                    });
                }
                return Ok(());
            } // else: direct path
        }

        // Drain any queued bytes first. If drain breaks the transport
        // mid-flight (Broken), the caller's `bytes` would be lost without
        // queuing. Capture that case and fall through to enqueue+reconnect.
        match self.drain_gap_if_alive() {
            Ok(()) => {}
            Err(TransportError::Broken { .. }) | Err(TransportError::Closed) => {
                // Fall through to enqueue + reconnect — the new bytes get
                // queued alongside whatever's still in the gap buffer.
            }
            Err(e) => return Err(e),
        }

        // Try the new bytes if the transport is still alive after drain.
        // A poisoned inner lock means a previous panic happened while
        // another caller held this lock. Route to TransportError::Broken so
        // the caller's reconnect logic (or shell-level error propagation)
        // tears down the wrapper.
        //
        // Scope-wrap: transport_guard MUST drop before any path reaches
        // self.reconnect_and_drain() further down — that function also
        // acquires self.inner.lock(), and std::sync::Mutex is not
        // reentrant. Holding it deadlocks only a successful-reconnect path,
        // which a test with an always-failing factory never takes.
        let mut inner_cancelled = false;
        // A connection the silent-peer rule gave up on, carried out of the
        // lock scope to be abandoned there.
        let mut retired = None;
        {
            let mut transport_guard = self.inner.lock().map_err(|_| TransportError::Broken {
                msg: "reconnect: inner lock poisoned during in-line send peek".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })?;
            if let Some(transport) = transport_guard.as_mut() {
                match transport.send_bytes(bytes) {
                    Ok(()) => {
                        let Some(trip) = self
                            .silent
                            .as_deref()
                            .and_then(|s| sample_after_send(s, &*transport, &self.shared))
                        else {
                            return Ok(());
                        };
                        // The peer stopped answering: this send counts as one
                        // that broke the connection. Un-publish its wake
                        // handle with it, as the drain does for a broken one.
                        retired = transport_guard.take().map(|t| (t, trip));
                        self.active.clear();
                    }
                    Err(TransportError::Backpressure { errno_code, .. }) => {
                        // Backpressure is recoverable without reconnect — propagate.
                        // Caller may retry the same bytes. Forward the inner
                        // errno_code: the wrapper does not have its
                        // own libsrt origin, so the only meaningful errno_code on
                        // this path is the one the inner transport supplied.
                        return Err(TransportError::Backpressure {
                            msg: "inner backpressure".into(),
                            errno_code,
                        });
                    }
                    Err(TransportError::TooLarge { len, max }) => {
                        return Err(TransportError::TooLarge { len, max });
                    }
                    Err(TransportError::Broken { .. }) | Err(TransportError::Closed) => {
                        // Fall through to reconnect path.
                    }
                    Err(TransportError::ExplicitClose) => {
                        // The inner was cancelled: terminal, not an outage.
                        // Settled below, once `inner` is released.
                        inner_cancelled = true;
                    }
                    Err(_) => {
                        // Unknown future variant — treat as broken and reconnect.
                        // Fall through to reconnect path.
                    }
                }
            }
        } // transport_guard dropped here — inner lock released before reconnect_and_drain
        if let Some((old, trip)) = retired {
            retire_silent(old, trip);
        }
        if inner_cancelled {
            // Nothing is queued: the caller is told the send failed and
            // keeps its message, in either mode.
            return Err(self.inner_cancelled());
        }

        // Inner is broken/closed. Queue this message and attempt reconnect.
        //
        // `OverflowPolicy::Reject` is a correctness-over-freshness contract —
        // when the gap buffer is full, the caller has explicitly asked us to
        // refuse new bytes rather than evict queued ones. Surface
        // `GapBufferError::Full` as
        // `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` so the caller's
        // shell maps it to `ShellErrorKind::Backpressure` (and `tst-c` to
        // `TST_E_BUFFER_FULL`) instead of silently dropping the bytes.
        //
        // The refusal is about admission only; the dead inner is replaced
        // all the same. On this path the buffer is empty (`Blocking` empties
        // it before every call returns; `Background` comes here only with an
        // empty buffer and no worker), so `Full` means capacity 0, and a
        // refusal that skipped the reconnect would meet the same dead inner
        // on every later call. `Background`: a worker reconnects, finds
        // nothing to drain and leaves; this call is refused.
        //
        // `OverflowPolicy::DropOldest` never refuses: `enqueue` evicts and
        // pushes, and at capacity 0 (reached here in `Background` only) it
        // drops and counts the incoming message itself.
        //
        // `Blocking` at capacity 0 stages nothing, under either policy: the
        // call reconnects and sends its own message on the fresh inner.
        // `Ok(())` means delivered in this mode, which rules out both
        // answers the buffer has for a message it cannot hold (`DropOldest`
        // would drop it behind an `Ok(())`, `Reject` would refuse it without
        // a reconnect).
        if self.policy.mode == ReconnectMode::Blocking && self.policy.gap_buffer_capacity == 0 {
            // `reconnect_and_drain` checks the latch before it dials, so a
            // cancel still outranks everything here.
            self.reconnect_and_drain()?;
            return self.send_after_reconnect(bytes);
        }
        let queued = {
            // The gap-accumulator is invariant-critical. A poisoned lock
            // means a previous panic happened while modifying the buffer's
            // invariants (length tracking, ring cursors); proceeding would
            // silently lose bytes. Panic with BUG: prefix per the FFI
            // panic-isolation convention; tst-c's ffi_catch wraps to
            // TST_E_PANIC_CAUGHT (-11). See enclosing send_managed's
            // /// # Panics rustdoc for the contract.
            let mut gap = self
                .gap
                .lock()
                .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
            // The inner send above is where a caller-side cancel lands: the
            // fired wake handle returns the parked send, and an inner that
            // answers with anything but `ExplicitClose` (a socket closed
            // under a write reports `Broken`) reads as "inner is down" to
            // the match above. It is not an outage. The invocation the cancel
            // interrupted reports it, and nothing is queued (or, in
            // `Background`, handed to a worker that would only see the
            // latch and exit) behind a wrapper that will never send again.
            // Ahead of the enqueue, so it also outranks a full `Reject`
            // buffer's `Backpressure`.
            if self.latch.is_closed() {
                return Err(self.latch.error());
            }
            let queued = gap.enqueue_tracked(bytes.to_vec());
            if self.policy.mode == ReconnectMode::Background {
                // Under the gap lock (invariant 2); spawn happens after
                // the lock drops (invariant 3). Also for a refused message:
                // the worker is what replaces the inner.
                self.shared
                    .bg_active
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            queued
        };
        let gap_full = || TransportError::Backpressure {
            msg: "gap buffer full".into(),
            errno_code: None,
        };
        if self.policy.mode == ReconnectMode::Background {
            self.spawn_worker();
            return match queued {
                Ok(_) => Ok(()),
                Err(gap_buffer::GapBufferError::Full) => {
                    #[cfg(test)]
                    cancel_window::run();
                    // As for the refusal under the gap lock above: a cancel
                    // that landed since the check is reported by this call.
                    if self.latch.is_closed() {
                        return Err(self.latch.error());
                    }
                    Err(gap_full())
                }
            };
        }
        // `Blocking` never meets `Full` here: capacity 0 left by the branch
        // above, and any other buffer is empty at this point.
        let queued = queued.map_err(|gap_buffer::GapBufferError::Full| gap_full())?;
        let result = self.reconnect_and_drain();
        if let (Err(_), Some(seq)) = (&result, queued) {
            // An error hands the message back to the caller, whichever
            // error it is. `Backpressure` says so outright (not consumed,
            // offer the same bytes again), and `MuxSender` / `Sender`
            // retain what they were refused and re-offer it after ANY
            // transport error — a give-up's `Broken` included. Leaving this
            // call's message queued as well would put it on the wire twice:
            // once from the gap buffer, once from the re-offer. Take it
            // back so the caller is its only owner. (After a cancel the
            // wrapper never sends again, so there it only keeps `gap_len`
            // honest.)
            //
            // Named by sequence number rather than "the back entry", though
            // in this mode they are the same thing: `send_bytes` is `&mut
            // self`, there is no worker, and the stats observer only reads,
            // so nothing else enqueues between the push above and here. A
            // message the drain already delivered is no longer queued and
            // is left alone: the drain is FIFO and this is the last entry,
            // so that drain ended `Ok`, and the only error that can follow
            // it is the close latch's.
            self.gap
                .lock()
                .expect("BUG: gap lock poisoned — gap buffer is invariant-critical")
                .reclaim(seq);
        }
        result
    }

    /// Drain the gap buffer if the inner transport is alive.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Broken` if the inner lock is poisoned (a
    /// previous panic left it in an unknown state).
    ///
    /// # Panics
    ///
    /// Panics with `"BUG: gap lock poisoned — gap buffer is invariant-critical"`
    /// if the gap-buffer mutex has been poisoned. See `send_managed`'s
    /// `# Panics` section for the full rationale.
    fn drain_gap_if_alive(&self) -> Result<(), TransportError> {
        // A poisoned inner lock means a previous panic happened while
        // another caller held this lock. Route to TransportError::Broken so
        // the reconnect loop or higher shell tears down the wrapper.
        let mut transport_guard = self.inner.lock().map_err(|_| TransportError::Broken {
            msg: "reconnect: inner lock poisoned during drain peek".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })?;
        let Some(transport) = transport_guard.as_mut() else {
            return Ok(()); // can't drain without a transport
        };
        // `inner` stays held for the whole drain; `gap` is taken only to
        // mark the front message in flight and, after the send, to settle
        // it (locking invariant 4, the same protocol as the background
        // worker). The message stays queued — counted in `gap_len` — until
        // it is settled, and every arm below settles before it returns.
        //
        // A panic inside the inner send leaves the mark set, but it also
        // poisons `inner`: every later path to this drain (here, the
        // in-line send, the reconnect install) reads the poison as
        // `Broken` before it could mark another message, so the stale mark
        // is never met.
        //
        // The gap-accumulator is invariant-critical. See send_managed's
        // `# Panics` rustdoc for the rationale.
        let lock_gap = || {
            self.gap
                .lock()
                .expect("BUG: gap lock poisoned — gap buffer is invariant-critical")
        };
        loop {
            let Some((seq, msg)) = lock_gap().begin_send() else {
                return Ok(());
            }; // gap lock dropped — never held across the send below
            let outcome = transport.send_bytes(&msg);
            let mut gap = lock_gap();
            match outcome {
                Ok(()) => {
                    gap.finish_send(seq);
                }
                Err(TransportError::Backpressure { errno_code, .. }) => {
                    gap.abort_send(seq);
                    // Forward the inner errno_code.
                    return Err(TransportError::Backpressure {
                        msg: "drain backpressure".into(),
                        errno_code,
                    });
                }
                Err(TransportError::Broken {
                    errno_code, cause, ..
                }) => {
                    gap.abort_send(seq);
                    drop(gap);
                    // Forward the inner errno_code (and its cause);
                    // the wrapper doesn't have its own SRT origin.
                    // Un-publish the dead inner's wake handle along with the
                    // inner it belongs to: nothing can be woken until the
                    // reconnect installs the replacement.
                    *transport_guard = None;
                    self.active.clear();
                    return Err(TransportError::Broken {
                        msg: "transport broken during drain".into(),
                        errno_code,
                        cause,
                    });
                }
                Err(TransportError::Closed) => {
                    gap.abort_send(seq);
                    drop(gap);
                    // Inner reported Closed (no errno surface) — surface as
                    // Broken so the caller's shell maps to TransportBroken.
                    *transport_guard = None;
                    self.active.clear();
                    return Err(TransportError::Broken {
                        msg: "transport broken during drain".into(),
                        errno_code: None,
                        cause: BrokenCause::Unspecified,
                    });
                }
                Err(TransportError::ExplicitClose) => {
                    // The inner was cancelled: terminal for the wrapper.
                    // Settle, then both locks go, so the wake handle is
                    // never fired under `inner`.
                    gap.abort_send(seq);
                    drop(gap);
                    drop(transport_guard);
                    return Err(self.inner_cancelled());
                }
                Err(e) => {
                    gap.abort_send(seq);
                    return Err(e);
                }
            }
        }
    }

    fn reconnect_and_drain(&self) -> Result<(), TransportError> {
        // An armed sender's cycle starts where its outage stands: attempts
        // on connections that never answered still count (see
        // `ReconnectPolicy::max_attempts`). Read once, never inside the run
        // of failed dials below.
        let armed = self.silent.is_some();
        let mut attempt: u32 = if armed {
            self.shared
                .unanswered_attempts
                .load(std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        };
        // Why the latest factory call of this budget failed, for the
        // give-up line.
        let mut last_error: Option<TransportError> = None;
        loop {
            if self.latch.is_closed() {
                return Err(self.latch.error());
            }
            attempt += 1;
            let Some(wait) = self.policy.next_delay(attempt) else {
                let max = self.policy.max_attempts.unwrap_or(0);
                // attempts_made = attempt - 1 because this iteration never
                // got past the budget check (no factory call happened on
                // this turn).
                warn!(
                    target: "tst_pipeline::reconnect",
                    attempts_made = attempt - 1,
                    max_attempts = max,
                    last_error = last_error.as_ref().map(tracing::field::display),
                    "reconnect gave up — propagating final error to caller",
                );
                if armed {
                    // A give-up ends the outage: the next one starts afresh.
                    self.shared
                        .unanswered_attempts
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                }
                return Err(TransportError::Broken {
                    msg: format!("reconnect gave up after {max} attempts"),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            };
            info!(
                target: "tst_pipeline::reconnect",
                attempt,
                // Recorded only when there is a limit.
                max_attempts = self.policy.max_attempts,
                backoff_ms = wait.as_millis() as u64,
                "reconnect attempt",
            );
            if !wait.is_zero() {
                debug!(
                    target: "tst_pipeline::reconnect",
                    backoff_ms = wait.as_millis() as u64,
                    "backoff before next attempt",
                );
            }
            if self.shutdown.wait_timeout(wait) {
                // close()/cancel() latched during the backoff wait — same
                // exit as the loop-top closed check, just prompt.
                return Err(self.latch.error());
            }
            self.shared
                .reconnect_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match (self.factory)() {
                Ok(new_inner) => {
                    match install_fresh_inner(
                        &self.inner,
                        &self.active,
                        &self.latch,
                        &self.shared,
                        self.silent.as_deref(),
                        new_inner,
                    ) {
                        Install::Installed => {
                            if armed {
                                // Recovered only once the new peer answers.
                                self.shared
                                    .unanswered_attempts
                                    .store(attempt, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        Install::Closed => return Err(self.latch.error()),
                        // A poisoned inner lock means a previous panic left
                        // the wrapper in an unknown state. Route to
                        // TransportError::Broken; the caller's shell propagates
                        // the error and may surface a TST_E_TRANSPORT (-8).
                        Install::InnerPoisoned => {
                            return Err(TransportError::Broken {
                                msg: "reconnect: inner lock poisoned during new-inner install"
                                    .into(),
                                errno_code: None,
                                cause: BrokenCause::Unspecified,
                            });
                        }
                    }
                    // Drain the gap buffer, then re-check the latch:
                    // a cancel that lands DURING this drain makes a real
                    // socket fail the in-flight send with a wire-looking
                    // `Broken` — the caller asked for the close, so report
                    // the close. (Same guard as the receive side's post-error
                    // `cancelled` check.) `Latch::error` names WHICH close:
                    // `ExplicitClose` for a cancel, `Closed` for the
                    // wrapper's own close()/Drop. A cancel that lands after a
                    // fully successful drain is reported here too — the next
                    // call would anyway, via send_managed's entry gate.
                    let drained = self.drain_gap_if_alive();
                    if self.latch.is_closed() {
                        return Err(self.latch.error());
                    }
                    return drained;
                }
                Err(TransportError::ExplicitClose) => {
                    // The dial was cancelled, not refused: stop dialling.
                    return Err(self.inner_cancelled());
                }
                Err(e) => {
                    warn!(
                        target: "tst_pipeline::reconnect",
                        attempt,
                        max_attempts = self.policy.max_attempts,
                        errno_code = broken_errno(&e),
                        error = %e,
                        "reconnect attempt failed",
                    );
                    last_error = Some(e);
                    continue; // try again
                }
            }
        }
    }

    /// `Blocking` with nothing staged in the gap buffer: put the call's own
    /// message on the inner `reconnect_and_drain` just installed and report
    /// what that inner answers, so `Ok(())` means delivered. One call, one
    /// reconnect: a fresh inner that breaks here is reported, and the next
    /// call reconnects.
    fn send_after_reconnect(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let outcome = {
            let mut transport_guard = self.inner.lock().map_err(|_| TransportError::Broken {
                msg: "reconnect: inner lock poisoned during the send after reconnect".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })?;
            match transport_guard.as_mut() {
                Some(transport) => {
                    let sent = transport.send_bytes(bytes);
                    if sent.is_ok()
                        && let Some(silent) = self.silent.as_deref()
                    {
                        // The first sample on an inner just installed (its
                        // rule was reset there), so it cannot trip.
                        let trip = sample_after_send(silent, &*transport, &self.shared);
                        debug_assert!(trip.is_none(), "a fresh inner tripped at its first send");
                    }
                    sent
                }
                None => Err(TransportError::Closed),
            }
        }; // `inner` released: a wake handle is never fired under it
        match outcome {
            Ok(()) => Ok(()),
            Err(TransportError::ExplicitClose) => Err(self.inner_cancelled()),
            // A failure under a latched close is that close's to report.
            Err(_) if self.latch.is_closed() => Err(self.latch.error()),
            // An inner's own `Closed` is a dead connection, not the
            // wrapper's close (the drain reports it the same way).
            Err(TransportError::Closed) => Err(TransportError::Broken {
                msg: "transport closed right after reconnect".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            }),
            Err(e) => Err(e),
        }
    }

    /// Spawn the per-outage worker. Callers must NOT hold the gap lock
    /// (invariant 3) and must have set `bg_active` under it already.
    fn spawn_worker(&self) {
        let mut slot = self
            .bg_thread
            .lock()
            .expect("BUG: bg_thread lock poisoned — held only across spawn/join bookkeeping");
        if let Some(prev) = slot.take() {
            // A previous worker cleared bg_active before exiting (that is
            // the only way we got here), so it is exiting or exited —
            // this join is bounded to its final instructions.
            let _ = prev.join();
        }
        let ctx = background::WorkerCtx {
            inner: Arc::clone(&self.inner),
            factory: Arc::clone(&self.factory),
            gap: Arc::clone(&self.gap),
            latch: Arc::clone(&self.latch),
            shutdown: Arc::clone(&self.shutdown),
            shared: Arc::clone(&self.shared),
            policy: self.policy.clone(),
            active: Arc::clone(&self.active),
            silent: self.silent.clone(),
        };
        match thread::Builder::new()
            .name("tst-reconnect".into())
            .spawn(move || background::worker_run(ctx))
        {
            Ok(handle) => *slot = Some(handle),
            Err(_) => {
                drop(slot);
                // The OS refused a thread: take the same exit a panicking
                // worker takes, so the next send reports Broken instead of
                // queuing behind a worker that does not exist. The gap lock
                // linearizes the clear with the send gate (invariant 2);
                // the abnormal flag is stored first, as the guard does.
                let _gap = self
                    .gap
                    .lock()
                    .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                use std::sync::atomic::Ordering;
                self.shared.gave_up_abnormal.store(true, Ordering::Release);
                self.shared.gave_up.store(true, Ordering::Release);
                self.shared.bg_active.store(false, Ordering::Release);
            }
        }
    }

    /// Latch the cancel an inner reported (see `Latch::inner_cancel`) and
    /// return the error to report for it. Callers hold neither lock.
    fn inner_cancelled(&self) -> TransportError {
        self.latch.inner_cancel(&self.shutdown, &self.active);
        self.latch.error()
    }
}

impl<T: Transport + 'static> Transport for ManagedTransport<T> {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.send_managed(msg)
    }

    fn max_payload(&self) -> usize {
        // The cached ceiling — NEVER the `inner` lock (locking invariant
        // 4). `RawSender::send`, `MuxSender`'s bundle sizing and `Sender`'s
        // per-bundle split call this on every send, and the background
        // drain worker
        // holds `inner` across one inner send that is unbounded against a
        // peer which stops reading. Answering from `inner` therefore put
        // the producer straight back behind a stalled sink — the exact
        // stall `ReconnectMode::Background` exists to prevent — and made
        // it poison-sensitive for no gain.
        //
        // The value is the ceiling of the LAST INSTALLED inner: equal to
        // the live inner's whenever one is installed, and while none is
        // (mid-reconnect) it keeps reporting that last ceiling instead of
        // a placeholder. This matches `ManagedRecvTransport`, which
        // caches for the same reason. A ceiling that SHRINKS on reinstall
        // is already handled downstream: `send_managed`'s pre-check reads
        // this same value, and the drain drops (and counts) a queued
        // message the rebuilt transport can no longer carry. Before any
        // inner is installed the field holds `SRT_TS_BUNDLE_BYTES`, but
        // `ManagedTransport::new` overwrites it with the real ceiling
        // immediately, so that constant is only ever a pre-construction
        // placeholder.
        self.shared
            .max_payload
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn is_alive(&self) -> bool {
        // The wrapper's own latch first: after close()/cancel()
        // every send is terminal (`Closed` / `ExplicitClose` — see
        // `latched_error`), whatever the inner says — a plain
        // `SrtTransport` keeps answering `socket.is_some()` after its fd
        // was closed by the cancel handle, and a background worker may
        // still be winding down.
        if self.latch.is_closed() {
            return false;
        }
        if self
            .shared
            .bg_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return true; // background reconnect in progress — recovering, not dead
        }
        // Mutex-poisoning policy (safe-default on poison): false matches the
        // "no live inner transport" answer (already the unwrap_or default).
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|t| t.is_alive()))
            .unwrap_or(false)
    }

    fn close(&mut self) {
        // Latch, wake the backoff wait, AND fire the live inner's wake
        // handle BEFORE the join below: a worker parked inside
        // an inner `send_bytes` only returns — so the join only completes —
        // once that handle fires. Signalling alone would leave close() waiting on
        // the transport's own timeout (or forever, for a channel/socket
        // with none).
        self.latch.terminal(&self.shutdown, &self.active);
        // Join an active worker. Bounded: it exits at its next shutdown
        // check; an in-flight factory() call must return first — connect
        // timeouts are the factory's own knob.
        if let Ok(mut slot) = self.bg_thread.lock()
            && let Some(h) = slot.take()
        {
            let _ = h.join();
        }
        // DELIBERATE ASYMMETRY with `ManagedRecvTransport::close`
        // (`managed_receive.rs`), which DOES `self.active.clear()`: it closes
        // AND retires its inner, so the slot would outlive what it wakes. Here
        // the inner stays in the guard (only `close()`d), and the slot's
        // invariant is that it mirrors `inner` — the only clear sites are the
        // ones that set `*guard = None`. Don't "fix" this to match. (There is
        // no stale handle to worry about either way: `CancelSlot::cancel`,
        // fired just above by `Latch::terminal`, already `take()`s —
        // consumes — whatever target was installed, so the slot carries
        // nothing left to clear here; the asymmetry is only about `inner`.)
        //
        // Mutex-poisoning policy (silent no-op on poison): close on a poisoned
        // state is naturally a no-op — the inner transport is already in an
        // unknown state and close-attempt would compound the problem. The
        // closed flag is already latched above so subsequent operations exit
        // cleanly via the existing poll-the-flag paths.
        if let Ok(mut guard) = self.inner.lock()
            && let Some(t) = guard.as_mut()
        {
            t.close();
        }
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(ManagedCancel {
            latch: Arc::clone(&self.latch),
            shutdown: Arc::clone(&self.shutdown),
            active: Arc::clone(&self.active),
        }))
    }

    fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        // Mirror max_payload() / is_alive() shape: when inner is None
        // (mid-reconnect or after close), there's no socket to query.
        // Returns None rather than fake-zero — the C ABI maps that to
        // TST_E_NOT_AVAILABLE so callers can distinguish "no socket"
        // from "socket exists with zero counters".
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|t| t.socket_stats()))
    }
}

impl<T: Transport> Drop for ManagedTransport<T> {
    fn drop(&mut self) {
        // Signal-and-detach — Drop must never block. The worker owns Arcs
        // to everything it touches, observes the signal at its next
        // check (backoff waits are interruptible), and exits on its own.
        // Without this, a max_attempts: None worker would retry forever
        // after the transport is gone.
        //
        // The wake handle is fired too: a worker parked
        // inside an inner `send_bytes` has no "next check" until that
        // send returns, and only the inner's cancel handle can return it.
        // Non-blocking here rests on the `TransportCancel` contract (an
        // interrupt, never a wait) — the same reliance every cross-thread
        // `cancel()` already has.
        self.latch.terminal(&self.shutdown, &self.active);
    }
}

/// Test-only checkpoint between `send_managed`'s entry gate and its
/// Background enqueue decision: the one place a cancel can land after a send
/// was let in and before the gap lock is taken. Per thread and one-shot, so a
/// test arms it for its own next send and nothing else sees it.
#[cfg(test)]
mod before_gap_lock {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    pub(super) fn arm(hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run() {
        if let Some(hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

/// Test-only checkpoint at the places a `Background` send is about to report
/// a refusal or a give-up (`Backpressure`, the worker's `Broken`) after the
/// checks that precede it: where a cancel can land late. Per thread and
/// one-shot.
#[cfg(test)]
mod cancel_window {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    pub(super) fn arm(hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run() {
        if let Some(hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

struct ManagedCancel {
    latch: Arc<Latch>,
    shutdown: Arc<Shutdown>,
    active: Arc<CancelSlot>,
}

impl TransportCancel for ManagedCancel {
    fn cancel(&self) {
        // `Latch::cancel` latches the cancel FIRST, then runs the shared
        // terminal transition (close()/Drop run it without the cancel
        // latch), so a send woken by step 3 already reads the cancel.
        self.latch.cancel(&self.shutdown, &self.active);
    }
    fn is_cancelled(&self) -> bool {
        // The slot latches in `Latch::terminal`, i.e. on cancel() AND on the
        // wrapper's own close()/Drop (one terminal transition).
        // Documented on the trait: where close() is implemented by firing the
        // same handle, is_cancelled() reads true after that close() too.
        self.active.is_cancelled()
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tst_core::transport::{Transport, TransportCancel, TransportError};

    /// Stub Transport whose cancel_handle records cancel() calls and
    /// makes is_alive() return false after cancel.
    struct CancellableMock {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        cancel_calls: Arc<AtomicU32>,
    }
    struct CancellableMockCancel {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        calls: Arc<AtomicU32>,
    }
    impl TransportCancel for CancellableMockCancel {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }
    impl Transport for CancellableMock {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            if self.cancelled.load(Ordering::SeqCst) {
                Err(TransportError::Broken {
                    msg: "cancelled".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            } else {
                Ok(())
            }
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            !self.cancelled.load(Ordering::SeqCst)
        }
        fn close(&mut self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
            Some(Arc::new(CancellableMockCancel {
                cancelled: self.cancelled.clone(),
                calls: self.cancel_calls.clone(),
            }))
        }
    }

    /// Stub Transport whose max_payload() returns a sentinel value (4242) so
    /// tests can distinguish "poison path returned default" from "happy path
    /// returned inner's value".
    struct NoopT;
    impl Transport for NoopT {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            4242
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    #[test]
    fn managed_transport_inner_lock_poisoned_returns_safe_default() {
        // Construct a ManagedTransport with a NoopT inner whose
        // max_payload() returns 4242. Poison the inner mutex via a sibling
        // thread that panics while holding the lock. Then confirm that
        // max_payload(), is_alive(), and close() all take a non-panicking
        // path.
        //
        // max_payload() does not consult `inner` at all (locking invariant 4 — the
        // drain worker holds that lock across one unbounded inner send,
        // and RawSender/MuxSender call max_payload() per send), so it is
        // poison-IMMUNE rather than poison-defaulting, and keeps reporting
        // the cached ceiling of the last installed inner. Asserting
        // SRT_TS_BUNDLE_BYTES here would be asserting that a poisoned
        // lock silently shrinks the caller's send budget — the opposite of
        // the guarantee. is_alive() and close() DO still take the lock and
        // keep their safe-default-on-poison behavior.
        let factory = || -> Result<NoopT, TransportError> {
            Err(TransportError::Broken {
                msg: "".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let mut managed = ManagedTransport::new(NoopT, factory, ReconnectPolicy::default())
            .expect("the policy validates");

        // Sanity-check: happy path returns the inner transport's values.
        assert_eq!(managed.max_payload(), 4242, "pre-poison: inner sentinel");
        assert!(managed.is_alive(), "pre-poison: inner is alive");

        // Poison the inner mutex: spawn a thread that holds the lock and
        // panics. The join() returns Err (thread panicked), and the Mutex
        // is now poisoned.
        {
            let inner_clone = Arc::clone(&managed.inner);
            let h = std::thread::spawn(move || {
                let _guard = inner_clone.lock().unwrap();
                panic!("intentional poison");
            });
            h.join()
                .expect_err("poison thread must panic to poison the mutex");
        }

        // After poison, none of the three may panic. max_payload() reads
        // the cached ceiling, which the poison cannot touch, so it still
        // reports the inner's sentinel.
        assert_eq!(
            managed.max_payload(),
            4242,
            "max_payload() must not consult the (poisoned) inner lock — it reports the cached ceiling"
        );
        assert!(!managed.is_alive(), "poisoned inner lock must return false");
        managed.close(); // must not panic
    }

    #[test]
    fn managed_cancel_handle_cancels_current_inner() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(AtomicU32::new(0));
        let inner = CancellableMock {
            cancelled: cancelled.clone(),
            cancel_calls: calls.clone(),
        };
        let factory = move || -> Result<CancellableMock, TransportError> {
            Err(TransportError::Broken {
                msg: "test factory always fails".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let managed = ManagedTransport::new(inner, factory, ReconnectPolicy::default())
            .expect("the policy validates");

        let handle = managed.cancel_handle().expect("cancellable inner -> Some");
        handle.cancel();
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn managed_cancel_latches_closed_so_reconnect_loop_exits() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(AtomicU32::new(0));
        let inner = CancellableMock {
            cancelled: cancelled.clone(),
            cancel_calls: calls.clone(),
        };
        let factory_calls = Arc::new(AtomicU32::new(0));
        let factory_calls_cl = factory_calls.clone();
        let cancelled_cl = cancelled.clone();
        let calls_cl2 = calls.clone();
        let factory = move || -> Result<CancellableMock, TransportError> {
            factory_calls_cl.fetch_add(1, Ordering::SeqCst);
            Ok(CancellableMock {
                cancelled: cancelled_cl.clone(),
                cancel_calls: calls_cl2.clone(),
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(100),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_millis(0)),
            ..Default::default()
        };
        let mut managed =
            ManagedTransport::new(inner, factory, policy).expect("the policy validates");

        // Trigger cancel before any send.
        let h = managed.cancel_handle().unwrap();
        h.cancel();

        // After cancel, send_bytes reports the cancel without burning
        // through reconnect attempts (the closed flag short-circuits the
        // reconnect loop).
        let err = managed.send_bytes(b"x").unwrap_err();
        assert_eq!(
            err,
            TransportError::ExplicitClose,
            "a cancel is reported as the cancel, not as a close or a wire break"
        );
        // The factory should NOT have been called repeatedly trying to
        // reconnect after cancel.
        assert!(factory_calls.load(Ordering::SeqCst) <= 1);
    }

    #[test]
    fn reconnect_policy_default_mode_is_blocking() {
        assert_eq!(ReconnectPolicy::default().mode, ReconnectMode::Blocking);
        assert_eq!(ReconnectMode::default(), ReconnectMode::Blocking);
    }

    /// Inner that always reports Broken, forcing the reconnect path.
    struct BrokenT;
    impl Transport for BrokenT {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Err(TransportError::Broken {
                msg: "always broken".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            false
        }
        fn close(&mut self) {}
    }

    #[test]
    fn blocking_cancel_interrupts_backoff_wait() {
        // Pre-fix, the reconnect loop slept the full backoff period even
        // after cancel latched; with the interruptible wait, cancel from a
        // sibling thread bounces send_bytes out orders of magnitude sooner
        // than the 30s backoff.
        let factory = || -> Result<BrokenT, TransportError> {
            Err(TransportError::Broken {
                msg: "factory down".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(3),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_secs(30)),
            ..Default::default()
        };
        let mut managed =
            ManagedTransport::new(BrokenT, factory, policy).expect("the policy validates");
        let cancel = managed
            .cancel_handle()
            .expect("managed always has a handle");
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            cancel.cancel();
        });
        let t0 = std::time::Instant::now();
        let err = managed.send_bytes(b"x").unwrap_err();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "cancel must interrupt the 30s backoff wait, took {:?}",
            t0.elapsed()
        );
        assert_eq!(
            err,
            TransportError::ExplicitClose,
            "the interrupted backoff wait reports the cancel that interrupted it"
        );
        canceller.join().unwrap();
    }

    #[test]
    fn stats_handle_counts_blocking_reconnect_cycle() {
        // Inner is a CancellableMock (defined above in this module): it
        // sends fine until its `cancelled` flag flips, then reports
        // Broken — which forces the reconnect path. The factory fails
        // twice, then produces a fresh, alive mock.
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = CancellableMock {
            cancelled: cancelled.clone(),
            cancel_calls: Arc::new(AtomicU32::new(0)),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let factory = move || -> Result<CancellableMock, TransportError> {
            let n = calls_cl.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(TransportError::Broken {
                    msg: "factory down".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            } else {
                Ok(CancellableMock {
                    cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    cancel_calls: Arc::new(AtomicU32::new(0)),
                })
            }
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(10),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_millis(0)),
            ..Default::default()
        };
        let mut managed =
            ManagedTransport::new(inner, factory, policy).expect("the policy validates");
        let stats = managed.stats_handle();
        let s0 = stats.stats().expect("no poison");
        assert_eq!((s0.reconnect_attempts, s0.reconnect_successes), (0, 0));
        assert!(!s0.reconnecting);

        cancelled.store(true, Ordering::SeqCst); // inner now reports Broken
        managed
            .send_bytes(b"x")
            .expect("blocking reconnect succeeds on the 3rd factory call");
        let s1 = stats.stats().expect("no poison");
        assert_eq!(s1.reconnect_attempts, 3, "two failures + one success");
        assert_eq!(s1.reconnect_successes, 1);
        assert_eq!(s1.gap_len, 0, "gap drained by the successful reconnect");
    }

    /// The three lock-free observers `ManagedHandles`
    /// carries for a sender read the SAME counters `stats_handle()`
    /// snapshots — obtained before the transport moves into a shell.
    #[test]
    fn counter_handles_track_the_blocking_reconnect_cycle() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = CancellableMock {
            cancelled: cancelled.clone(),
            cancel_calls: Arc::new(AtomicU32::new(0)),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let factory = move || -> Result<CancellableMock, TransportError> {
            let n = calls_cl.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(TransportError::Broken {
                    msg: "factory down".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            } else {
                Ok(CancellableMock {
                    cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    cancel_calls: Arc::new(AtomicU32::new(0)),
                })
            }
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(10),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_millis(0)),
            ..Default::default()
        };
        let mut managed =
            ManagedTransport::new(inner, factory, policy).expect("the policy validates");
        let attempts = managed.attempts_handle();
        let reconnects = managed.reconnects_handle();
        let reconnecting = managed.reconnecting_handle();
        assert_eq!(
            (
                attempts.load(Ordering::Acquire),
                reconnects.load(Ordering::Acquire)
            ),
            (0, 0)
        );
        assert!(!reconnecting.load(Ordering::Acquire));

        cancelled.store(true, Ordering::SeqCst); // inner now reports Broken
        managed
            .send_bytes(b"x")
            .expect("blocking reconnect succeeds on the 3rd factory call");
        assert_eq!(
            attempts.load(Ordering::Acquire),
            3,
            "two failures + one success"
        );
        assert_eq!(reconnects.load(Ordering::Acquire), 1);
        assert!(
            !reconnecting.load(Ordering::Acquire),
            "Blocking mode never sets the background-worker flag"
        );
        let stats = managed.stats_handle().stats().expect("no poison");
        assert_eq!(
            (stats.reconnect_attempts, stats.reconnect_successes),
            (3, 1),
            "the handles and the stats snapshot read one set of counters"
        );
    }

    /// The handles must never queue behind the `inner` mutex: a binding
    /// polls them from a watchdog thread while the sender thread is parked
    /// inside the reconnect loop holding that lock (bounded failing
    /// watchdog, no wall-clock assert). The *values* are
    /// the load-bearing part: the in-flight attempt is already visible,
    /// which pins the bump BEFORE the factory call — the placement a
    /// binding's reconnect dashboard depends on.
    #[test]
    fn counter_handles_read_while_the_sender_is_parked_in_the_factory() {
        const PROMPT: std::time::Duration = std::time::Duration::from_secs(10);
        fn wait_for(deadline: std::time::Duration, f: impl Fn() -> bool) -> bool {
            let start = std::time::Instant::now();
            while start.elapsed() < deadline {
                if f() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            f()
        }

        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let inner = CancellableMock {
            cancelled: cancelled.clone(),
            cancel_calls: Arc::new(AtomicU32::new(0)),
        };
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let factory = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let calls = Arc::new(AtomicU32::new(0));
            move || -> Result<CancellableMock, TransportError> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    entered.store(true, Ordering::SeqCst);
                    assert!(
                        wait_for(PROMPT, || release.load(Ordering::SeqCst)),
                        "parked factory was never released — watchdog"
                    );
                    return Err(TransportError::Broken {
                        msg: "factory down".into(),
                        errno_code: None,
                        cause: BrokenCause::Unspecified,
                    });
                }
                Ok(CancellableMock {
                    cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    cancel_calls: Arc::new(AtomicU32::new(0)),
                })
            }
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(10),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_millis(0)),
            ..Default::default()
        };
        let mut managed =
            ManagedTransport::new(inner, factory, policy).expect("the policy validates");
        // Obtain BEFORE the move — the only way a binding can reach them.
        let attempts = managed.attempts_handle();
        let reconnects = managed.reconnects_handle();
        let reconnecting = managed.reconnecting_handle();
        let sender = std::thread::spawn(move || {
            managed.send_bytes(b"x").expect("2nd factory call wins");
        });

        assert!(
            wait_for(PROMPT, || entered.load(Ordering::SeqCst)),
            "the factory was never entered"
        );
        let reader = {
            let (a, r, g) = (
                Arc::clone(&attempts),
                Arc::clone(&reconnects),
                Arc::clone(&reconnecting),
            );
            std::thread::spawn(move || {
                (
                    a.load(Ordering::Acquire),
                    r.load(Ordering::Acquire),
                    g.load(Ordering::Acquire),
                )
            })
        };
        assert!(
            wait_for(PROMPT, || reader.is_finished()),
            "a counter handle waited on the parked sender"
        );
        assert_eq!(
            reader.join().expect("reader thread"),
            (1, 0, false),
            "the in-flight attempt is counted; no success yet; Blocking never flags a worker"
        );

        release.store(true, Ordering::SeqCst);
        sender.join().expect("sender thread");
        assert_eq!(
            (
                attempts.load(Ordering::Acquire),
                reconnects.load(Ordering::Acquire)
            ),
            (2, 1)
        );
    }

    /// A `Background` wrapper in an outage that will not end: the inner is
    /// broken, the factory refuses, and the worker sits in a 30 s backoff
    /// that only the terminal signal interrupts. One message is queued.
    fn background_in_outage(
        capacity: usize,
        overflow: OverflowPolicy,
    ) -> ManagedTransport<BrokenT> {
        let factory = || -> Result<BrokenT, TransportError> {
            Err(TransportError::Broken {
                msg: "factory down".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: None,
            backoff: BackoffStrategy::Constant(std::time::Duration::from_secs(30)),
            gap_buffer_capacity: capacity,
            overflow_policy: overflow,
            mode: ReconnectMode::Background,
        };
        let mut managed =
            ManagedTransport::new(BrokenT, factory, policy).expect("the policy validates");
        managed
            .send_bytes(b"queued")
            .expect("the break is accepted into the gap buffer");
        managed
    }

    /// A cancel that lands after the entry gate let the send in, and before
    /// the send reaches the gap lock, is still ahead of the enqueue: the
    /// send reports the cancel and queues nothing.
    #[test]
    fn background_send_let_in_before_a_cancel_is_not_accepted_after_it() {
        let mut managed = background_in_outage(4, OverflowPolicy::DropOldest);
        let stats = managed.stats_handle();
        let cancel = managed.cancel_handle().expect("managed has a handle");
        before_gap_lock::arm(move || cancel.cancel());

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::ExplicitClose));
        let s = stats.stats().expect("no poison");
        assert_eq!(s.gap_len, 1, "the refused message was queued anyway");
        assert_eq!(s.gap_messages_dropped, 0);
    }

    /// Same window, full `Reject` buffer: the cancel outranks the
    /// `Backpressure` the buffer would answer with.
    #[test]
    fn background_cancel_before_the_enqueue_outranks_a_full_reject_buffer() {
        let mut managed = background_in_outage(1, OverflowPolicy::Reject);
        let stats = managed.stats_handle();
        let cancel = managed.cancel_handle().expect("managed has a handle");
        before_gap_lock::arm(move || cancel.cancel());

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::ExplicitClose));
        assert_eq!(stats.stats().expect("no poison").gap_len, 1);
    }

    /// A full `Reject` buffer refuses the send; a cancel that lands after the
    /// gap-locked check and before the refusal returns is what the call
    /// reports. Nothing is queued or counted for the refused message.
    #[test]
    fn background_cancel_after_a_refusal_under_the_gap_lock_is_the_cancel() {
        let mut managed = background_in_outage(1, OverflowPolicy::Reject);
        let stats = managed.stats_handle();
        let cancel = managed.cancel_handle().expect("managed has a handle");
        cancel_window::arm(move || cancel.cancel());

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::ExplicitClose));
        let s = stats.stats().expect("no poison");
        assert_eq!((s.gap_len, s.gap_messages_dropped), (1, 0));
    }

    /// Same, for the refusal after a direct send failed (capacity 0: the
    /// message is refused outright and a worker takes over the reconnect).
    #[test]
    fn background_cancel_after_a_refusal_behind_a_failed_send_is_the_cancel() {
        let factory = || -> Result<BrokenT, TransportError> {
            Err(TransportError::Broken {
                msg: "factory down".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: None,
            backoff: BackoffStrategy::Constant(std::time::Duration::from_secs(30)),
            gap_buffer_capacity: 0,
            overflow_policy: OverflowPolicy::Reject,
            mode: ReconnectMode::Background,
        };
        let mut managed =
            ManagedTransport::new(BrokenT, factory, policy).expect("the policy validates");
        let stats = managed.stats_handle();
        let cancel = managed.cancel_handle().expect("managed has a handle");
        cancel_window::arm(move || cancel.cancel());

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::ExplicitClose));
        let s = stats.stats().expect("no poison");
        assert_eq!((s.gap_len, s.gap_messages_dropped), (0, 0));
    }

    /// A send that finds the worker's give-up reports a cancel that landed
    /// since the entry gate, not the give-up. Its message is not queued.
    #[test]
    fn background_cancel_before_the_give_up_report_is_the_cancel() {
        let factory = || -> Result<BrokenT, TransportError> {
            Err(TransportError::Broken {
                msg: "factory down".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(1),
            backoff: BackoffStrategy::Constant(std::time::Duration::ZERO),
            gap_buffer_capacity: 4,
            overflow_policy: OverflowPolicy::Reject,
            mode: ReconnectMode::Background,
        };
        let mut managed =
            ManagedTransport::new(BrokenT, factory, policy).expect("the policy validates");
        managed
            .send_bytes(b"queued")
            .expect("the break is accepted into the gap buffer");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !managed.shared.gave_up.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "the worker gave up");
            std::thread::yield_now();
        }
        let stats = managed.stats_handle();
        let cancel = managed.cancel_handle().expect("managed has a handle");
        cancel_window::arm(move || cancel.cancel());

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::ExplicitClose));
        let s = stats.stats().expect("no poison");
        assert_eq!((s.gap_len, s.gap_messages_dropped), (1, 0));
        assert_eq!(
            managed.send_bytes(b"next"),
            Err(TransportError::ExplicitClose),
            "the next call reports the cancel too"
        );
    }

    /// The wrapper's own `close()` in that window is reported as `Closed`.
    /// `close()` needs `&mut`, so the latch is set the way `Drop` sets it.
    #[test]
    fn background_close_before_the_enqueue_is_reported_as_closed() {
        let mut managed = background_in_outage(4, OverflowPolicy::DropOldest);
        let stats = managed.stats_handle();
        let (latch, shutdown, active) = (
            Arc::clone(&managed.latch),
            Arc::clone(&managed.shutdown),
            Arc::clone(&managed.active),
        );
        before_gap_lock::arm(move || latch.terminal(&shutdown, &active));

        let result = managed.send_bytes(b"late");

        assert_eq!(result, Err(TransportError::Closed));
        assert_eq!(stats.stats().expect("no poison").gap_len, 1);
    }

    /// `is_alive()` reads the `closed` latch the wrapper sets on cancel (and
    /// every later `send_bytes` returns `Closed`); asking only the background
    /// flag and the inner reports a cancelled wrapper alive. `NoopT` is always alive and has no
    /// cancel handle — the exact shape of a real `SrtTransport`, whose
    /// `is_alive` is `socket.is_some()` and stays true after the cancel
    /// handle closes the fd.
    #[test]
    fn is_alive_reads_the_closed_latch_first() {
        let factory = || -> Result<NoopT, TransportError> {
            Err(TransportError::Broken {
                msg: "".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        };
        let mut managed = ManagedTransport::new(NoopT, factory, ReconnectPolicy::default())
            .expect("the policy validates");
        assert!(managed.is_alive(), "pre-cancel: inner is alive");

        managed
            .cancel_handle()
            .expect("managed always hands out a handle")
            .cancel();

        assert!(
            matches!(managed.send_bytes(b"x"), Err(TransportError::ExplicitClose)),
            "the send path already honours the latch"
        );
        assert!(
            !managed.is_alive(),
            "cancel latched `closed`, but is_alive() still reported the inner's answer (NoopT is always alive)"
        );
    }
}

#[cfg(test)]
mod validate_tests {
    #[test]
    fn with_setters_match_the_field_literal() {
        let chained = ReconnectPolicy::default()
            .with_max_attempts(Some(3))
            .with_backoff(BackoffStrategy::Constant(Duration::from_millis(7)))
            .with_gap_buffer_capacity(5)
            .with_overflow_policy(OverflowPolicy::Reject)
            .with_mode(ReconnectMode::Background);
        let literal = ReconnectPolicy {
            max_attempts: Some(3),
            backoff: BackoffStrategy::Constant(Duration::from_millis(7)),
            gap_buffer_capacity: 5,
            overflow_policy: OverflowPolicy::Reject,
            mode: ReconnectMode::Background,
        };
        assert_eq!(chained.max_attempts, literal.max_attempts);
        assert_eq!(chained.backoff, literal.backoff);
        assert_eq!(chained.gap_buffer_capacity, literal.gap_buffer_capacity);
        assert_eq!(chained.overflow_policy, literal.overflow_policy);
        assert_eq!(chained.mode, literal.mode);
    }

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn policy(max_attempts: Option<u32>, backoff: BackoffStrategy) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts,
            backoff,
            ..Default::default()
        }
    }

    #[test]
    fn validate_refuses_zero_constant_backoff_with_unlimited_attempts() {
        let p = policy(None, BackoffStrategy::Constant(Duration::ZERO));
        assert_eq!(
            p.validate(),
            Err(ReconnectPolicyError::UnpacedUnlimitedRetry)
        );
    }

    #[test]
    fn validate_refuses_zero_exponential_base_with_unlimited_attempts() {
        let p = policy(
            None,
            BackoffStrategy::Exponential {
                base: Duration::ZERO,
                max: Duration::from_secs(10),
            },
        );
        assert_eq!(
            p.validate(),
            Err(ReconnectPolicyError::UnpacedUnlimitedRetry)
        );
    }

    #[test]
    fn validate_refuses_zero_exponential_max_with_unlimited_attempts() {
        let p = policy(
            None,
            BackoffStrategy::Exponential {
                base: Duration::from_millis(100),
                max: Duration::ZERO,
            },
        );
        assert_eq!(
            p.validate(),
            Err(ReconnectPolicyError::UnpacedUnlimitedRetry)
        );
    }

    #[test]
    fn validate_accepts_zero_backoff_with_bounded_attempts() {
        let p = policy(Some(3), BackoffStrategy::Constant(Duration::ZERO));
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_the_default() {
        assert_eq!(ReconnectPolicy::default().validate(), Ok(()));
    }

    struct DropFlagTransport(Arc<AtomicBool>);
    impl Drop for DropFlagTransport {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl tst_core::transport::Transport for DropFlagTransport {
        fn send_bytes(&mut self, _bytes: &[u8]) -> Result<(), TransportError> {
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

    #[test]
    fn managed_transport_new_refuses_an_unpaced_unlimited_policy() {
        let dropped = Arc::new(AtomicBool::new(false));
        let r = ManagedTransport::new(
            DropFlagTransport(Arc::clone(&dropped)),
            || Err(TransportError::Closed),
            policy(None, BackoffStrategy::Constant(Duration::ZERO)),
        );
        assert!(matches!(
            r,
            Err(ReconnectPolicyError::UnpacedUnlimitedRetry)
        ));
        assert!(dropped.load(Ordering::SeqCst), "inner was dropped");
    }
}

#[cfg(test)]
mod silent_peer_tests {
    use super::silent_peer::{SilentPeer, Verdict};
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    /// An inner that accepts every send and keeps every provided default.
    struct Plain;
    impl Transport for Plain {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
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

    fn managed(policy: ReconnectPolicy) -> ManagedTransport<Plain> {
        ManagedTransport::new(Plain, || Ok(Plain), policy).expect("the policy validates")
    }

    fn window_of(m: &ManagedTransport<Plain>) -> Option<Duration> {
        m.silent
            .as_ref()
            .map(|s| s.lock().expect("not poisoned").window())
    }

    #[test]
    fn with_silent_peer_timeout_arms_and_a_zero_window_does_not() {
        assert_eq!(window_of(&managed(ReconnectPolicy::default())), None);
        let armed = managed(ReconnectPolicy::default())
            .with_silent_peer_timeout(Duration::from_millis(1500));
        assert_eq!(window_of(&armed), Some(Duration::from_millis(1500)));
        let zero = managed(ReconnectPolicy::default()).with_silent_peer_timeout(Duration::ZERO);
        assert_eq!(window_of(&zero), None);
    }

    #[test]
    fn an_install_resets_the_silent_peer_rule() {
        let w = Duration::from_millis(100);
        let m = managed(ReconnectPolicy::default()).with_silent_peer_timeout(w);
        let silent = m.silent.as_deref().expect("armed");
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        // Prime: an answered inner, then a frozen run two windows old.
        {
            let mut s = silent.lock().unwrap();
            s.sent(at(0));
            s.reading(at(0), Some(1));
            s.sent(at(5));
            assert_eq!(s.reading(at(5), Some(2)), Verdict::Answered);
            let mut t = 10;
            while t <= 210 {
                s.sent(at(t));
                t += 10;
            }
            // Primed: the next equal reading on this inner trips.
            assert!(matches!(
                s.reading(at(215), Some(2)),
                Verdict::Silent { .. }
            ));
        }
        assert!(matches!(
            install_fresh_inner(
                &m.inner,
                &m.active,
                &m.latch,
                &m.shared,
                Some(silent),
                Plain
            ),
            Install::Installed
        ));
        let mut s = silent.lock().unwrap();
        assert!(s.sent(at(220)), "a fresh inner is read at its first send");
        assert_eq!(
            s.reading(at(220), Some(2)),
            Verdict::Healthy,
            "the run of the old inner carried over"
        );
        assert!(s.sent(at(230)), "a fresh inner has not answered yet");
        assert_eq!(s.reading(at(230), Some(3)), Verdict::Answered);
    }

    /// Leaves the rule one sample short of a trip on reading `v`: a first
    /// reading of `v` two windows ago, then a send every quarter window up
    /// to a millisecond ago. The next sample on reading `v` trips, unless
    /// half a window passes first.
    fn prime_to_trip(silent: &Mutex<SilentPeer>, v: u64) {
        let mut s = silent.lock().unwrap();
        let w = s.window();
        let now = Instant::now();
        let start = now
            .checked_sub(w * 2)
            .expect("the clock is two windows past its origin");
        s.sent(start);
        s.reading(start, Some(v));
        let last = now - Duration::from_millis(1);
        let mut t = start + w / 4;
        while t < last {
            s.sent(t);
            t += w / 4;
        }
        s.sent(last);
    }

    /// Long enough that the test's own scheduling never restarts a primed
    /// run (a gap of half of it would).
    const PRIMED_WINDOW: Duration = Duration::from_secs(10);

    type Fire = Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>>;

    /// An inner whose peer never answers. Its `abandon` is counted, and can
    /// announce itself and wait on a gate; a handle put in `fire` is fired
    /// by its next send, which still succeeds.
    ///
    /// `broken` fails every send; `prime` primes that rule to trip from
    /// inside the next send, so the sample right after it trips.
    #[derive(Default)]
    struct Gated {
        abandons: Arc<AtomicU32>,
        closes: Arc<AtomicU32>,
        entered: Option<mpsc::Sender<()>>,
        gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
        fire: Fire,
        broken: bool,
        prime: Option<Arc<Mutex<SilentPeer>>>,
    }
    impl Transport for Gated {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            if self.broken {
                return Err(TransportError::Broken {
                    msg: "scripted break".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
            if let Some(h) = self.fire.lock().unwrap().take() {
                h.cancel();
            }
            if let Some(rule) = self.prime.take() {
                prime_to_trip(&rule, 1);
            }
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        fn abandon(&mut self) {
            self.abandons.fetch_add(1, Ordering::SeqCst);
            if let Some(tx) = &self.entered {
                let _ = tx.send(());
            }
            if let Some(gate) = &self.gate {
                let (open, cv) = &**gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }
        }
        fn peer_activity(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn gated_sender(
        inner: Gated,
        mode: ReconnectMode,
    ) -> (ManagedTransport<Gated>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let m = ManagedTransport::new(
            inner,
            move || {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(Gated::default())
            },
            ReconnectPolicy::default()
                .with_mode(mode)
                .with_backoff(BackoffStrategy::Constant(Duration::ZERO)),
        )
        .expect("the policy validates")
        .with_silent_peer_timeout(PRIMED_WINDOW);
        (m, calls)
    }

    #[test]
    fn a_retired_connection_is_abandoned_with_the_inner_lock_free() {
        let (tx, rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let abandons = Arc::new(AtomicU32::new(0));
        let inner = Gated {
            abandons: Arc::clone(&abandons),
            entered: Some(tx),
            gate: Some(Arc::clone(&gate)),
            ..Gated::default()
        };
        let (m, calls) = gated_sender(inner, ReconnectMode::Blocking);
        let m = Arc::new(m);
        prime_to_trip(m.silent.as_deref().expect("armed"), 1);
        let sender = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || m.send_managed(b"trips"))
        };
        rx.recv_timeout(tst_test_helpers::timing::scaled(Duration::from_secs(5)))
            .expect("the tripped connection is abandoned");
        let free = m.inner.try_lock().is_ok();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let sent = sender.join().expect("the sender does not panic");
        assert!(free, "abandon ran with `inner` held");
        assert!(sent.is_ok(), "{sent:?}");
        assert_eq!(abandons.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_connection_retired_by_the_drain_is_abandoned_with_the_inner_lock_free() {
        let (tx, rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let abandons = Arc::new(AtomicU32::new(0));
        // The worker's first replacement: primed by its first drain send.
        let first: Arc<Mutex<Option<Gated>>> = Arc::default();
        let calls = Arc::new(AtomicU32::new(0));
        let m = {
            let first = Arc::clone(&first);
            let calls = Arc::clone(&calls);
            ManagedTransport::new(
                Gated {
                    broken: true,
                    ..Gated::default()
                },
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(first.lock().unwrap().take().unwrap_or_default())
                },
                ReconnectPolicy::default()
                    .with_mode(ReconnectMode::Background)
                    .with_backoff(BackoffStrategy::Constant(Duration::ZERO)),
            )
            .expect("the policy validates")
            .with_silent_peer_timeout(PRIMED_WINDOW)
        };
        *first.lock().unwrap() = Some(Gated {
            abandons: Arc::clone(&abandons),
            entered: Some(tx),
            gate: Some(Arc::clone(&gate)),
            prime: m.silent.clone(),
            ..Gated::default()
        });
        // The initial connection is broken: the message is queued and the
        // worker drains it into the primed replacement, which trips.
        assert!(m.send_managed(b"queued").is_ok());
        rx.recv_timeout(tst_test_helpers::timing::scaled(Duration::from_secs(5)))
            .expect("the drain's tripped connection is abandoned");
        let free = m.inner.try_lock().is_ok();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert!(free, "abandon ran with `inner` held");
        let stats = m.stats_handle();
        let deadline = Instant::now() + tst_test_helpers::timing::scaled(Duration::from_secs(5));
        while !stats
            .stats()
            .is_some_and(|s| s.reconnect_successes == 2 && !s.reconnecting && s.gap_len == 0)
        {
            assert!(Instant::now() < deadline, "the worker did not reconnect");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(abandons.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_trip_with_the_cancel_latched_reports_the_cancel_and_dials_nothing() {
        for mode in [ReconnectMode::Blocking, ReconnectMode::Background] {
            let fire = Fire::default();
            let abandons = Arc::new(AtomicU32::new(0));
            let inner = Gated {
                abandons: Arc::clone(&abandons),
                fire: Arc::clone(&fire),
                ..Gated::default()
            };
            let (m, calls) = gated_sender(inner, mode);
            *fire.lock().unwrap() = m.cancel_handle();
            prime_to_trip(m.silent.as_deref().expect("armed"), 1);
            let sent = m.send_managed(b"trips under a cancel");
            assert!(
                matches!(sent, Err(TransportError::ExplicitClose)),
                "{mode:?}: {sent:?}"
            );
            assert_eq!(abandons.load(Ordering::SeqCst), 1, "{mode:?}: retired");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{mode:?}: dialled");
            assert_eq!(m.stats_handle().stats().unwrap().gap_len, 0, "{mode:?}");
        }
    }
}
