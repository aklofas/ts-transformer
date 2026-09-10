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

pub(crate) mod background;
mod gap_buffer;
mod recv_end_reason;

pub use gap_buffer::{GapBuffer, OverflowPolicy};
pub use recv_end_reason::{RecvEndReason, RecvEndReasonHandle};

use background::{ManagedShared, Shutdown};

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// on backoff or a factory call: while the inner transport is down it
    /// enqueues to the gap buffer under `overflow_policy` — though it can
    /// still block briefly on internal lock contention while the worker
    /// is mid-drain (bounded to at most one in-flight inner send).
    /// `Ok(())` means *accepted*, not *delivered* — pair with
    /// [`ManagedTransport::stats_handle`] for drop/reconnect visibility.
    Background,
}

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Maximum reconnect attempts before giving up. None = retry forever.
    /// Default: `Some(10)`.
    pub max_attempts: Option<u32>,

    /// Backoff strategy between attempts. Default: exponential 100ms..=10s.
    pub backoff: BackoffStrategy,

    /// Gap-buffer capacity in messages. Default 256.
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

impl ReconnectPolicy {
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
        if let Some(max) = self.max_attempts {
            if attempt > max {
                return None;
            }
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
use tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES;
use tst_core::transport::{Transport, TransportCancel, TransportError};

/// Snapshot of `ManagedTransport`'s reconnect/gap telemetry.
///
/// **Stability: Stable** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ManagedTransportStats {
    /// Total `factory()` invocations across all reconnect cycles.
    pub reconnect_attempts: u64,
    /// Successful reconnects (factory returned a transport that was installed).
    pub reconnect_successes: u64,
    /// Messages currently queued in the gap buffer.
    pub gap_len: u64,
    /// Messages lost to `DropOldest` eviction (plus oversized-after-reconnect drops).
    pub gap_messages_dropped: u64,
    /// Bytes lost to the same.
    pub gap_bytes_dropped: u64,
    /// True while a background reconnect worker is active (`ReconnectMode::Background` only).
    pub reconnecting: bool,
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
            reconnect_successes: self.shared.reconnect_successes.load(Ordering::Relaxed),
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
/// `ManagedTransport` itself implements `Transport`, so all three sender
/// shells (`MuxSender`, `Sender`, `RawSender`) compose with it
/// transparently:
///
/// ```ignore
/// let factory = || SrtTransport::connect(...);
/// let inner = factory()?;
/// let managed = ManagedTransport::new(inner, factory, ReconnectPolicy::default());
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
///   policy's `max_attempts` budget is exhausted.
/// - **`ReconnectMode::Background`** — a per-outage worker thread owns
///   the factory/backoff/drain loop instead. While that worker is active,
///   or the gap buffer is non-empty, `send_bytes` never touches the inner
///   transport, never waits on backoff or a factory call, and enqueues
///   under `overflow_policy` — though it can still block briefly on
///   internal lock contention while the worker is mid-drain (bounded to
///   at most one in-flight inner send). **`Ok(())` in this mode means the
///   bytes were *accepted* into the gap buffer, not that they were
///   *delivered*** —
///   pair `Background` with [`Self::stats_handle`] to observe
///   `reconnecting` / `gap_len` / `gap_messages_dropped`. That counter
///   also counts a queued message that no longer fits the *rebuilt*
///   transport's `max_payload` — dropped during drain rather than
///   wedging it forever; `Blocking` mode would instead return
///   `TooLarge` synchronously to the caller for that same message.
///   `max_attempts` bounds one continuous outage (the budget resets after every
///   successful reconnect, exactly as it does per-call in `Blocking`
///   mode). If the worker exhausts that budget — or terminates
///   abnormally (an unwind inside the factory, or an unrecoverable
///   poisoned lock) — the give-up is reported exactly once, as a single
///   `TransportError::Broken` on the next `send_bytes` call; that call's
///   own bytes are **not** queued — the caller sees the error and owns
///   the resend decision. [`Self::is_alive`] returns `true` while a
///   background worker is actively recovering (never `false`, which
///   would read as permanently dead rather than "recovering").
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
/// 4. The worker holds `gap` across a single inner send during drain —
///    deliberate, pins the front message against `DropOldest` eviction.
/// 5. The cancel path takes NEITHER lock: it fires the `active` cancel
///    slot, which publishes the live inner's wake handle. A cancel is only
///    useful while a send is in flight — i.e. exactly while `inner` is
///    held — so reading the handle out of `inner` would queue the cancel
///    behind the call it was asked to interrupt.
///
/// # Closing
///
/// [`Self::close`] (via the `Transport::close` trait method) joins any
/// active background worker before returning. That join is bounded by
/// whatever the worker happens to be doing at the moment `close()` is
/// called: an in-flight `factory()` call, or a single in-flight drain
/// send — never an unbounded backoff wait (those are interruptible in
/// both modes; cancel/close wakes them promptly). `Drop` never blocks:
/// it signals shutdown and detaches, leaving the worker to observe the
/// signal at its next check and exit on its own.
///
/// # Lock poisoning policy (post-Wave-6.F)
///
/// - **Inner-transport lock** (poisoned mid-mutation):
///   - `send_bytes`: returns `TransportError::Broken { .. }`. Caller can
///     rebuild the wrapper.
///   - `max_payload`: returns `SRT_TS_BUNDLE_BYTES` (the same default used
///     when the inner transport is `None` — no panic).
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
    /// Latched true by `cancel_handle().cancel()` or `close()`. The
    /// reconnect loop checks this each iteration so a cancel mid-retry
    /// breaks out instead of waiting through the full backoff budget.
    closed: Arc<std::sync::atomic::AtomicBool>,
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
}

impl<T: Transport + 'static> ManagedTransport<T> {
    pub fn new<F>(inner: T, factory: F, policy: ReconnectPolicy) -> Self
    where
        F: Fn() -> Result<T, TransportError> + Send + Sync + 'static,
    {
        let gap = GapBuffer::new(policy.gap_buffer_capacity, policy.overflow_policy);
        let active = Arc::new(CancelSlot::new());
        if let Some(h) = inner.cancel_handle() {
            active.install(h);
        }
        Self {
            inner: Arc::new(Mutex::new(Some(inner))),
            factory: Arc::new(factory),
            policy,
            gap: Arc::new(Mutex::new(gap)),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(Shutdown::new()),
            shared: Arc::new(ManagedShared::default()),
            bg_thread: Mutex::new(None),
            active,
        }
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

    /// Try to send via the inner transport. On Broken/Closed, queue bytes
    /// and attempt reconnect.
    ///
    /// Pre-checks `bytes.len() > max_payload` against the inner transport
    /// before any state mutation, so oversized messages never enter the gap
    /// buffer (where they'd block drain forever).
    ///
    /// # Panics
    ///
    /// Panics with `"BUG: gap lock poisoned — gap buffer is invariant-critical"`
    /// if the internal gap-buffer mutex has been poisoned by a previous panic.
    /// The poison signals a corrupted gap-buffer invariant (length tracking,
    /// ring cursors); proceeding would silently drop queued bytes. Caught by
    /// `tst-c`'s `ffi_catch` as `TST_E_PANIC_CAUGHT` (-11). The
    /// recoverable-path lock poison (the inner-transport mutex) instead
    /// returns `Err(TransportError::Broken { .. })` — see Task 3 sites.
    fn send_managed(&self, bytes: &[u8]) -> Result<(), TransportError> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(TransportError::Closed);
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
                });
            }
        }
        // Pre-check size against inner before queuing — oversized messages
        // would otherwise sit in the gap buffer and fail every drain.
        //
        // Mutex-poisoning policy (recoverable path): poisoned inner lock during the
        // size pre-check routes to TransportError::Broken with a site-specific
        // diagnostic. The `send_managed` poison checks use the same shape.
        let max = self
            .inner
            .lock()
            .map_err(|_| TransportError::Broken {
                msg: "reconnect: inner lock poisoned during size pre-check".into(),
                errno_code: None,
            })?
            .as_ref()
            .map(|t| t.max_payload())
            .unwrap_or(SRT_TS_BUNDLE_BYTES);
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
            let need_worker = {
                let mut gap = self
                    .gap
                    .lock()
                    .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                let worker_active = self
                    .shared
                    .bg_active
                    .load(std::sync::atomic::Ordering::Acquire);
                if !worker_active && gap.is_empty() {
                    None // fall through to the direct path below
                } else {
                    if let Err(gap_buffer::GapBufferError::Full) = gap.enqueue(bytes.to_vec()) {
                        return Err(TransportError::Backpressure {
                            msg: "gap buffer full".into(),
                            errno_code: None,
                        });
                    }
                    if worker_active {
                        Some(false)
                    } else {
                        // Backlog with no worker (post-give-up): start a
                        // fresh cycle. Flag set under the gap lock.
                        self.shared
                            .bg_active
                            .store(true, std::sync::atomic::Ordering::Release);
                        Some(true)
                    }
                }
            }; // gap lock dropped — never spawn while holding it (invariant 3)
            match need_worker {
                Some(true) => {
                    self.spawn_worker();
                    return Ok(());
                }
                Some(false) => return Ok(()),
                None => {} // direct path
            }
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
        // Plan B mutex sweep (recoverable path): poisoned inner lock means
        // a previous panic happened while another caller held this lock.
        // Route to TransportError::Broken so the caller's reconnect logic
        // (or shell-level error propagation) tears down the wrapper.
        // Precedent: plan #45.
        //
        // Scope-wrap: transport_guard MUST drop before any path reaches
        // self.reconnect_and_drain() further down — that function also
        // acquires self.inner.lock(), and std::sync::Mutex is not
        // reentrant. The previous shape used an anonymous MutexGuard
        // (which dropped at if-let scrutinee end); converting to a named
        // binding introduced a deadlock on any successful-reconnect path.
        // Final-review caught this; existing tests didn't because all
        // tests use always-failing factories.
        {
            let mut transport_guard = self.inner.lock().map_err(|_| TransportError::Broken {
                msg: "reconnect: inner lock poisoned during in-line send peek".into(),
                errno_code: None,
            })?;
            if let Some(transport) = transport_guard.as_mut() {
                match transport.send_bytes(bytes) {
                    Ok(()) => return Ok(()),
                    Err(TransportError::Backpressure { errno_code, .. }) => {
                        // Backpressure is recoverable without reconnect — propagate.
                        // Caller may retry the same bytes. Forward the inner
                        // errno_code (D5 follow-up): the wrapper does not have its
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
                    Err(_) => {
                        // Phase 1: Unknown future variant — treat as broken and reconnect.
                        // Fall through to reconnect path.
                    }
                }
            }
        } // transport_guard dropped here — inner lock released before reconnect_and_drain

        // Inner is broken/closed. Queue this message and attempt reconnect.
        //
        // Validate-1 C2 (Codex PIPE-01): `OverflowPolicy::Reject` is a
        // correctness-over-freshness contract — when the gap buffer is full,
        // the caller has explicitly asked us to refuse new bytes rather than
        // evict queued ones. Surface `GapBufferError::Full` as
        // `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` so the caller's
        // shell maps it to `ShellErrorKind::Backpressure` (and `tst-c` to
        // `TST_E_BUFFER_FULL`) instead of silently dropping the bytes. We
        // also skip the reconnect attempt on this path: the buffer rejection
        // is about local capacity, not transport liveness, and a reconnect
        // wouldn't change the buffer state.
        //
        // `OverflowPolicy::DropOldest` continues to return `Ok(())` from
        // `enqueue` (it evicts and pushes) so this path is unchanged for
        // that policy.
        {
            // Plan B mutex sweep (documented panic): gap-accumulator is
            // invariant-critical. A poisoned lock means a previous panic
            // happened while modifying the buffer's invariants (length
            // tracking, ring cursors); proceeding would silently lose
            // bytes. Panic with BUG: prefix per the FFI panic-isolation
            // convention (plan #50); tst-c's ffi_catch wraps to
            // TST_E_PANIC_CAUGHT (-11). See enclosing send_managed's
            // /// # Panics rustdoc for the contract.
            let mut gap = self
                .gap
                .lock()
                .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
            if let Err(gap_buffer::GapBufferError::Full) = gap.enqueue(bytes.to_vec()) {
                return Err(TransportError::Backpressure {
                    msg: "gap buffer full".into(),
                    errno_code: None,
                });
            }
            if self.policy.mode == ReconnectMode::Background {
                // Under the gap lock (invariant 2); spawn happens after
                // the lock drops (invariant 3).
                self.shared
                    .bg_active
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        if self.policy.mode == ReconnectMode::Background {
            self.spawn_worker();
            return Ok(());
        }
        self.reconnect_and_drain()
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
        // Plan B mutex sweep (recoverable path): poisoned inner lock means
        // a previous panic happened while another caller held this lock.
        // Route to TransportError::Broken so the reconnect loop or higher
        // shell tears down the wrapper. Precedent: plan #45.
        let mut transport_guard = self.inner.lock().map_err(|_| TransportError::Broken {
            msg: "reconnect: inner lock poisoned during drain peek".into(),
            errno_code: None,
        })?;
        let Some(transport) = transport_guard.as_mut() else {
            return Ok(()); // can't drain without a transport
        };
        // Plan B mutex sweep (documented panic): gap-accumulator is
        // invariant-critical. See Step 4.1 / send_managed /// # Panics
        // rustdoc for rationale.
        let mut gap = self
            .gap
            .lock()
            .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
        while let Some(msg) = gap.front() {
            match transport.send_bytes(msg) {
                Ok(()) => {
                    gap.pop_front();
                }
                Err(TransportError::Backpressure { errno_code, .. }) => {
                    // D5 follow-up: forward inner errno_code.
                    return Err(TransportError::Backpressure {
                        msg: "drain backpressure".into(),
                        errno_code,
                    });
                }
                Err(TransportError::Broken { errno_code, .. }) => {
                    // D5 follow-up: forward inner errno_code; the wrapper
                    // doesn't have its own SRT origin.
                    // Un-publish the dead inner's wake handle along with the
                    // inner it belongs to: nothing can be woken until the
                    // reconnect installs the replacement.
                    *transport_guard = None;
                    self.active.clear();
                    return Err(TransportError::Broken {
                        msg: "transport broken during drain".into(),
                        errno_code,
                    });
                }
                Err(TransportError::Closed) => {
                    // Inner reported Closed (no errno surface) — surface as
                    // Broken so the caller's shell maps to TransportBroken.
                    *transport_guard = None;
                    self.active.clear();
                    return Err(TransportError::Broken {
                        msg: "transport broken during drain".into(),
                        errno_code: None,
                    });
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn reconnect_and_drain(&self) -> Result<(), TransportError> {
        let mut attempt: u32 = 0;
        loop {
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return Err(TransportError::Closed);
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
                    "reconnect gave up — propagating final error to caller",
                );
                return Err(TransportError::Broken {
                    msg: format!("reconnect gave up after {max} attempts"),
                    errno_code: None,
                });
            };
            info!(
                target: "tst_pipeline::reconnect",
                attempt,
                max_attempts = self.policy.max_attempts.unwrap_or(0),
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
                return Err(TransportError::Closed);
            }
            self.shared
                .reconnect_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match (self.factory)() {
                Ok(new_inner) => {
                    // Take the wake handle before the transport moves into
                    // the mutex; publish it after the lock drops, so the
                    // slot's own firing (a cancel that landed while the
                    // factory was building) never runs under `inner`.
                    let new_cancel = new_inner.cancel_handle();
                    // Plan B mutex sweep (recoverable path): poisoned inner
                    // lock means a previous panic left the wrapper in an
                    // unknown state. Route to TransportError::Broken; the
                    // caller's shell propagates the error and may surface
                    // a TST_E_TRANSPORT (-8). Precedent: plan #45.
                    let mut guard = self.inner.lock().map_err(|_| TransportError::Broken {
                        msg: "reconnect: inner lock poisoned during new-inner install".into(),
                        errno_code: None,
                    })?;
                    *guard = Some(new_inner);
                    drop(guard);
                    if let Some(h) = new_cancel {
                        self.active.install(h);
                    }
                    self.shared
                        .reconnect_successes
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Drain gap buffer.
                    return self.drain_gap_if_alive();
                }
                Err(_) => {
                    continue; // try again
                }
            }
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
            closed: Arc::clone(&self.closed),
            shutdown: Arc::clone(&self.shutdown),
            shared: Arc::clone(&self.shared),
            policy: self.policy.clone(),
            active: Arc::clone(&self.active),
        };
        *slot = Some(thread::spawn(move || background::worker_run(ctx)));
    }
}

impl<T: Transport + 'static> Transport for ManagedTransport<T> {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.send_managed(msg)
    }

    fn max_payload(&self) -> usize {
        // Mutex-poisoning policy (safe-default on poison): SRT_TS_BUNDLE_BYTES is
        // already the "no live inner transport" default; poison falls through to
        // the same default. Matches socket_stats's shape below.
        // Deliberate asymmetry with ManagedRecvTransport (which caches the
        // last live inner's ceiling): understating a *send* budget is safe —
        // callers just chunk smaller — while understating a recv ceiling was
        // the PR #97 truncation bug class. Keep the conservative constant here.
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|t| t.max_payload()))
            .unwrap_or(SRT_TS_BUNDLE_BYTES)
    }

    fn is_alive(&self) -> bool {
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
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.shutdown.signal();
        // Join an active worker. Bounded: it exits at its next shutdown
        // check; an in-flight factory() call must return first — connect
        // timeouts are the factory's own knob.
        if let Ok(mut slot) = self.bg_thread.lock() {
            if let Some(h) = slot.take() {
                let _ = h.join();
            }
        }
        // DELIBERATE ASYMMETRY with `ManagedRecvTransport::close`
        // (`managed_receive.rs`), which DOES `self.active.clear()`: it closes
        // AND retires its inner, so the slot would outlive what it wakes. Here
        // the inner stays in the guard (only `close()`d), and the slot's
        // invariant is that it mirrors `inner` — the only clear sites are the
        // ones that set `*guard = None`. Don't "fix" this to match.
        //
        // Mutex-poisoning policy (silent no-op on poison): close on a poisoned
        // state is naturally a no-op — the inner transport is already in an
        // unknown state and close-attempt would compound the problem. The
        // closed flag is already latched above so subsequent operations exit
        // cleanly via the existing poll-the-flag paths.
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(t) = guard.as_mut() {
                t.close();
            }
        }
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(ManagedCancel {
            closed: Arc::clone(&self.closed),
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
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.shutdown.signal();
    }
}

struct ManagedCancel {
    closed: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<Shutdown>,
    active: Arc<CancelSlot>,
}

impl TransportCancel for ManagedCancel {
    fn cancel(&self) {
        // Latch closed first so the reconnect loop exits next iteration.
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake a backoff wait (either mode); a no-op when nothing waits.
        self.shutdown.signal();
        // Then wake whatever the live inner is parked in. The handle comes
        // from the `active` slot, never from the `inner` mutex: every send
        // path holds `inner` across the inner `send_bytes`, so a cancel
        // that had to read the inner out of that mutex would queue behind
        // the very call it was asked to interrupt (locking invariant 5).
        // The slot also latches, so an inner installed after this point (a
        // reconnect was mid-flight) is cancelled on arrival instead of
        // being parked on.
        self.active.cancel();
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
    }
    impl Transport for CancellableMock {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            if self.cancelled.load(Ordering::SeqCst) {
                Err(TransportError::Broken {
                    msg: "cancelled".into(),
                    errno_code: None,
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
        // max_payload(), is_alive(), and close() all take the safe-default
        // path rather than propagating the panic.
        let factory = || -> Result<NoopT, TransportError> {
            Err(TransportError::Broken {
                msg: "".into(),
                errno_code: None,
            })
        };
        let mut managed = ManagedTransport::new(NoopT, factory, ReconnectPolicy::default());

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

        // After poison, all three methods must NOT panic and must return the
        // documented safe defaults rather than the inner transport's values.
        assert_eq!(
            managed.max_payload(),
            SRT_TS_BUNDLE_BYTES,
            "poisoned inner lock must return SRT_TS_BUNDLE_BYTES, not inner sentinel 4242"
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
            })
        };
        let managed = ManagedTransport::new(inner, factory, ReconnectPolicy::default());

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
        let mut managed = ManagedTransport::new(inner, factory, policy);

        // Trigger cancel before any send.
        let h = managed.cancel_handle().unwrap();
        h.cancel();

        // After cancel, send_bytes should return Broken without burning
        // through reconnect attempts (the closed flag short-circuits the
        // reconnect loop).
        let err = managed.send_bytes(b"x").unwrap_err();
        assert!(matches!(
            err,
            TransportError::Broken { .. } | TransportError::Closed
        ));
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
            })
        };
        let policy = ReconnectPolicy {
            max_attempts: Some(3),
            backoff: BackoffStrategy::Constant(std::time::Duration::from_secs(30)),
            ..Default::default()
        };
        let mut managed = ManagedTransport::new(BrokenT, factory, policy);
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
        assert!(matches!(
            err,
            TransportError::Broken { .. } | TransportError::Closed
        ));
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
        let mut managed = ManagedTransport::new(inner, factory, policy);
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
}
