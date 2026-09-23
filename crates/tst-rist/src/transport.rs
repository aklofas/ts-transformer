//! [`RistTransport`] — RIST sender impl [`Transport`].

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::transport::{BrokenCause, Transport, TransportCancel, TransportError};

use crate::config::{EncryptionKey, RistConfig, RistProfile};
use crate::error::RistError;
use crate::init::{GLOBAL_LOGGING, ensure_init};
use crate::stats::{OnDrop, RistStats};
use crate::url::{RistUrl, native_endpoint};

/// Cheap cloneable handle that ends a [`crate::recv::RistRecvTransport`]
/// parked in `recv_bytes` on librist's 100 ms poll and makes the *next*
/// `send_bytes` / `recv_bytes` on the owning transport return
/// [`TransportError::ExplicitClose`], from any thread.
///
/// Obtained via [`RistTransport::cancel_handle`] /
/// [`crate::recv::RistRecvTransport::cancel_handle`] (inherent) or the
/// `Transport::cancel_handle` / `RecvTransport::cancel_handle` trait forms
/// (`Some` on both RIST transports). Cancelling does **not** destroy the
/// librist context — `close()` still does (`rist_destroy` + stats-ref
/// reclaim). Cooperative, same shape as `UdpCancelHandle`:
///
/// - a parked `recv_bytes` observes the flag when its current
///   `rist_receiver_data_read2` tick (≤ 100 ms, `POLL_TIMEOUT_MS`) returns
///   and reports `ExplicitClose` instead of the tick's `Backpressure`;
/// - calls started after `cancel()` return `ExplicitClose` at their entry
///   check (`send_bytes` never parks — `rist_sender_data_write` enqueues —
///   so the entry check is its only cancel point);
/// - a tick that delivered a block returns it; the *next* call fails;
/// - `is_alive()` reads `false` after `cancel()`; `close()` afterwards
///   still runs `rist_destroy` exactly once (double close stays a no-op).
///
/// `close()` is a different signal: post-close calls return
/// [`TransportError::Closed`] and the handle does not read cancelled.
#[derive(Clone, Debug)]
pub struct RistCancelHandle {
    /// The cancel latch proper, SEPARATE from the transport's `alive` flag
    /// — `alive` is cleared by `close()` and by a latched `Broken` too, so
    /// it cannot answer "did the caller cancel?".
    cancelled: Arc<AtomicBool>,
}

impl RistCancelHandle {
    pub(crate) fn from_flag(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Signal cancellation. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// `true` once [`Self::cancel`] has been called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl TransportCancel for RistCancelHandle {
    fn cancel(&self) {
        RistCancelHandle::cancel(self)
    }

    fn is_cancelled(&self) -> bool {
        RistCancelHandle::is_cancelled(self)
    }
}

/// Send-side RIST transport.
///
/// Wraps a librist `rist_ctx` configured as a sender. Per-message send via
/// `rist_sender_data_write`. Drop calls `rist_destroy`.
pub struct RistTransport {
    ctx: *mut rist_sys::rist_ctx,
    pkt_size: usize,
    peer_url: String,
    alive: Arc<AtomicBool>,
    /// Set by [`RistCancelHandle::cancel`]; checked at `send_bytes` entry
    /// BEFORE `alive` so a cancelled transport reports `ExplicitClose`.
    cancelled: Arc<AtomicBool>,
    stats: Arc<Mutex<RistStats>>,
    /// Leaked `Arc<Mutex<RistStats>>` ref handed to librist as the stats
    /// callback `arg`. Reclaimed exactly once in `close()` after `rist_destroy`.
    stats_arg: *mut c_void,
}

// librist's rist_ctx is safe to share across threads as long as we don't pass
// it concurrently to mutating ops from multiple threads — Transport's methods
// take &mut self so the borrow checker enforces single-threaded access here.
// stats_arg is a leaked Arc<Mutex<RistStats>> ref: its pointee is Send + Sync,
// and the pointer is reclaimed (Arc::from_raw) exactly once in close() under
// &mut self, so it is never aliased across threads.
unsafe impl Send for RistTransport {}

impl RistTransport {
    /// Build a sender from a URL using defaults.
    pub fn connect(url: &str) -> Result<Self, RistError> {
        let parsed = RistUrl::parse(url)?;
        if parsed.is_recv_bind {
            return Err(RistError::InvalidConfig(
                "URL has '@' prefix — use RistRecvTransport::listen".into(),
            ));
        }
        let mut cfg = RistConfig::default();
        cfg.merge_from_url(&parsed);
        Self::connect_with_config(&parsed, &cfg)
    }

    /// Build a sender from a parsed URL + config.
    pub fn connect_with_config(url: &RistUrl, cfg: &RistConfig) -> Result<Self, RistError> {
        ensure_init();

        #[cfg(not(feature = "mbedtls"))]
        if cfg.encryption.is_some() {
            return Err(RistError::EncryptionDisabled);
        }

        let profile = rist_profile_to_c(cfg.profile);
        let logging_settings = global_logging_ptr();

        // ===== Create sender context =====
        let mut ctx: *mut rist_sys::rist_ctx = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_sender_create(&mut ctx, profile, 0, logging_settings) };
        if rc != 0 || ctx.is_null() {
            return Err(RistError::ContextCreateFailed);
        }
        // Guard that calls rist_destroy on every error-exit path. Disarmed at
        // the bottom of this constructor once ctx is safely in Self.
        let mut ctx_guard = OnDrop::new(|| unsafe {
            rist_sys::rist_destroy(ctx);
        });

        // ===== Parse peer URL into peer_config =====
        let peer_url_str = native_endpoint(url, false);
        let peer_url_c = CString::new(peer_url_str.clone())
            .map_err(|e| RistError::InvalidConfig(format!("bad peer URL: {e}")))?;

        let mut peer_config: *mut rist_sys::rist_peer_config = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_parse_address2(peer_url_c.as_ptr(), &mut peer_config) };
        if rc != 0 || peer_config.is_null() {
            return Err(RistError::Ffi {
                code: rc,
                function: "rist_parse_address2",
            });
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // Sender = caller: initiate_conn=1.
        unsafe {
            (*peer_config).initiate_conn = 1;
        }

        // Apply cfg overlays. apply_peer_overrides is pub(crate) so recv.rs
        // can reuse it.
        if let Err(e) = apply_peer_overrides(peer_config, cfg) {
            unsafe {
                // peer_config_free2 must happen before rist_destroy (guard drop).
                rist_sys::rist_peer_config_free2(&mut peer_config);
            }
            return Err(e);
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // ===== Add peer to sender =====
        let mut peer: *mut rist_sys::rist_peer = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_peer_create(ctx, &mut peer, peer_config) };
        // peer_config is now owned by librist (or freed internally); per
        // librist docs we still free the wrapper.
        unsafe {
            rist_sys::rist_peer_config_free2(&mut peer_config);
        }

        if rc != 0 {
            return Err(RistError::PeerCreateFailed);
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // Register the librist stats callback BEFORE rist_start (interval,
        // leak-one-Arc-ref + reclaim-at-close contract live in
        // stats::register_stats_callback). The order is load-bearing: librist
        // >= 0.2.20's protocol thread re-reads the stats interval lock-free on
        // every tick, so registering once that thread is running is a data
        // race (TSan-caught on the 2026-08-31 nightly). librist's own tools
        // register before start too.
        let (stats, stats_arg) = crate::stats::register_stats_callback(ctx);

        // ===== Start the session =====
        let rc = unsafe { rist_sys::rist_start(ctx) };
        if rc != 0 {
            // Mirror close(): rist_destroy first (no protocol thread ever ran,
            // so no callback can be in flight), then reclaim the leaked Arc
            // ref exactly once — there is no Self for close() to do it.
            drop(ctx_guard);
            unsafe { drop(Arc::from_raw(stats_arg as *const Mutex<RistStats>)) };
            return Err(RistError::Ffi {
                code: rc,
                function: "rist_start",
            });
        }

        // All setup succeeded — ctx ownership transfers to Self; disarm the guard.
        ctx_guard.disarm();
        Ok(Self {
            ctx,
            pkt_size: cfg.pkt_size,
            peer_url: peer_url_str,
            alive: Arc::new(AtomicBool::new(true)),
            cancelled: Arc::new(AtomicBool::new(false)),
            stats,
            stats_arg,
        })
    }

    /// Peer URL the transport was built against (for diagnostics).
    pub fn peer_url(&self) -> &str {
        &self.peer_url
    }

    /// Current snapshot of cumulative stats.
    pub fn stats(&self) -> RistStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Cross-thread cancel handle for this sender; see [`RistCancelHandle`].
    pub fn cancel_handle(&self) -> RistCancelHandle {
        RistCancelHandle::from_flag(self.cancelled.clone())
    }
}

impl Transport for RistTransport {
    /// Send one block via `rist_sender_data_write`.
    ///
    /// Error mapping (librist return-code namespace, see `WriteOutcome`):
    /// - empty `msg` → [`TransportError::TooLarge`] `{ len: 0, max }` (librist
    ///   refuses zero-length blocks; an input error, the transport stays alive
    ///   and nothing was sent);
    /// - `rc == -2` (sender queue full) → [`TransportError::Backpressure`]
    ///   `{ errno_code: Some(-2) }`, transport alive, `msg` NOT consumed —
    ///   retry it later;
    /// - any other `rc < 0` → [`TransportError::Broken`] `{ errno_code:
    ///   Some(rc) }` and the transport is latched dead.
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        // Cancel wins over close: a caller who fired the handle sees the
        // cancel outcome even if a close() raced in afterwards.
        if self.cancelled.load(Ordering::Acquire) {
            return Err(TransportError::ExplicitClose);
        }
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        // librist returns -1 for `payload_len <= 0` with the context still
        // usable. Reject it here as an input error so the -1 never reaches
        // the fatal arm below (CORR-04).
        if msg.is_empty() {
            return Err(TransportError::TooLarge {
                len: 0,
                max: self.pkt_size,
            });
        }
        if msg.len() > self.pkt_size {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.pkt_size,
            });
        }

        let block = rist_sys::rist_data_block {
            payload: msg.as_ptr() as *const _,
            payload_len: msg.len(),
            ts_ntp: 0,
            virt_src_port: 0,
            virt_dst_port: 0,
            peer: std::ptr::null_mut(),
            flow_id: 0,
            seq: 0,
            flags: 0,
            ref_: std::ptr::null_mut(),
        };

        let rc = unsafe { rist_sys::rist_sender_data_write(self.ctx, &block) };
        match classify_write_rc(rc) {
            WriteOutcome::Sent => {}
            WriteOutcome::QueueFull => {
                // One packet dropped by librist; the context is healthy. Do
                // NOT latch `alive` — a latched Broken would make
                // ManagedTransport rist_destroy + rebuild the context, losing
                // the recovery buffer and every peer for one dropped packet.
                return Err(TransportError::Backpressure {
                    msg: "rist_sender_data_write returned -2 (sender queue full, packet dropped)"
                        .into(),
                    errno_code: Some(rc),
                });
            }
            WriteOutcome::Fatal(rc) => {
                self.alive.store(false, Ordering::Release);
                return Err(TransportError::Broken {
                    msg: format!("rist_sender_data_write returned {rc}"),
                    errno_code: Some(rc),
                    cause: BrokenCause::Unspecified,
                });
            }
        }

        if let Ok(mut s) = self.stats.lock() {
            s.bytes_sent = s.bytes_sent.wrapping_add(msg.len() as u64);
            s.packets_sent = s.packets_sent.wrapping_add(1);
        }
        Ok(())
    }

    fn max_payload(&self) -> usize {
        self.pkt_size
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.cancelled.load(Ordering::Acquire)
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.cancel_handle()))
    }

    fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        Some(self.stats().to_socket_stats())
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
        if !self.ctx.is_null() {
            unsafe {
                rist_sys::rist_destroy(self.ctx);
            }
            self.ctx = std::ptr::null_mut();
        }
        // Reclaim the leaked Arc ref EXACTLY ONCE. rist_destroy above joined
        // librist's protocol thread, so no callback can be in flight. The
        // null-guard makes double-close / Drop-after-close a no-op.
        if !self.stats_arg.is_null() {
            unsafe {
                drop(Arc::from_raw(self.stats_arg as *const Mutex<RistStats>));
            }
            self.stats_arg = std::ptr::null_mut();
        }
    }
}

impl Drop for RistTransport {
    fn drop(&mut self) {
        self.close();
    }
}

// ============================================================
// Helpers
// ============================================================

pub(crate) fn rist_profile_to_c(profile: RistProfile) -> rist_sys::rist_profile {
    match profile {
        RistProfile::Simple => rist_sys::rist_profile_RIST_PROFILE_SIMPLE,
        RistProfile::Main => rist_sys::rist_profile_RIST_PROFILE_MAIN,
    }
}

/// Return the global logging-settings pointer registered in init.rs, or
/// NULL if logging registration failed earlier.
pub(crate) fn global_logging_ptr() -> *mut rist_sys::rist_logging_settings {
    GLOBAL_LOGGING
        .get()
        .map(|p| p.0)
        .unwrap_or(std::ptr::null_mut())
}

/// Outcome of one `rist_sender_data_write` call, classified from its return
/// code. librist's namespace (`rist.c` `rist_sender_data_write`, `udp.c`
/// `rist_sender_enqueue`):
///
/// - `>= 0` — the block was enqueued; the value is the payload length.
/// - `-2` — the sender queue is full and THIS block was dropped. The context
///   is healthy and the next write may succeed ("decrease bitrate, buffer
///   time length or increase packet size" in librist's own log line).
/// - any other negative — fatal: null/non-sender context, zero-length or
///   oversize payload, `USE_SEQ` with split mode, no peers. After
///   `connect_with_config` the only reachable one is the zero-length payload,
///   which `send_bytes` rejects before calling librist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Sent,
    QueueFull,
    Fatal(i32),
}

pub(crate) fn classify_write_rc(rc: i32) -> WriteOutcome {
    match rc {
        -2 => WriteOutcome::QueueFull,
        rc if rc < 0 => WriteOutcome::Fatal(rc),
        _ => WriteOutcome::Sent,
    }
}

/// Apply [`RistConfig`] overlays onto the parsed `rist_peer_config`. Shared
/// by [`RistTransport::connect_with_config`] and the receiver
/// (`RistRecvTransport`).
pub(crate) fn apply_peer_overrides(
    peer_config: *mut rist_sys::rist_peer_config,
    cfg: &RistConfig,
) -> Result<(), RistError> {
    if peer_config.is_null() {
        return Err(RistError::InvalidConfig("null peer_config".into()));
    }
    let pc = unsafe { &mut *peer_config };

    // `bandwidth_kbps` is an alias of `recovery_maxbitrate_kbps` — both are
    // librist's `recovery_maxbitrate`. RistUrl::parse already refuses a
    // conflicting URL; this catches the programmatic/builder route (CORR-23).
    let recovery_maxbitrate = match (cfg.bandwidth_kbps, cfg.recovery_maxbitrate_kbps) {
        (Some(bw), Some(rm)) if bw != rm => {
            return Err(RistError::InvalidConfig(format!(
                "bandwidth_kbps={bw} conflicts with recovery_maxbitrate_kbps={rm}; \
                 they set the same librist field (recovery_maxbitrate) — give one, or equal values"
            )));
        }
        (bw, rm) => bw.or(rm),
    };
    if let Some(kbps) = recovery_maxbitrate {
        pc.recovery_maxbitrate = kbps;
    }
    pc.recovery_length_min = duration_millis_u32(cfg.buffer);
    pc.recovery_length_max = duration_millis_u32(cfg.buffer).max(pc.recovery_length_max);
    if let Some(t) = cfg.session_timeout {
        pc.session_timeout = duration_millis_u32(t);
    }
    pc.compression = if cfg.compression { 1 } else { 0 };

    if let Some(cname) = &cfg.cname {
        write_c_string_field(&mut pc.cname, cname, "cname")?;
    }

    if let Some(enc) = &cfg.encryption {
        apply_encryption(pc, enc)?;
    }

    Ok(())
}

/// Convert a [`std::time::Duration`] to whole milliseconds as a `u32`,
/// **saturating at [`u32::MAX`]** rather than silently wrapping.
///
/// librist's `recovery_length_*` / `session_timeout` fields are `u32`
/// milliseconds, so a `Duration` longer than ~49.7 days (`u32::MAX` ms)
/// cannot be represented. A plain `as u32` cast truncates `as_millis()`
/// (a `u128`) and would wrap such a value to a small, wrong number. We clamp
/// to `u32::MAX` instead — these are buffer/timeout knobs where the largest
/// representable value is the safest fallback for an over-large request.
fn duration_millis_u32(d: std::time::Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

fn apply_encryption(
    pc: &mut rist_sys::rist_peer_config,
    key: &EncryptionKey,
) -> Result<(), RistError> {
    if !matches!(key.size_bits, 128 | 192 | 256) {
        return Err(RistError::InvalidConfig(format!(
            "encryption key_size must be 128/192/256, got {}",
            key.size_bits
        )));
    }
    pc.key_size = key.size_bits as i32;
    pc.key_rotation = key.rotation;
    write_c_string_field(&mut pc.secret, key.secret.expose(), "secret")?;
    Ok(())
}

/// Copy a Rust `&str` into a fixed-size `[c_char; N]` field, null-terminating.
fn write_c_string_field(
    dst: &mut [c_char],
    src: &str,
    field_name: &'static str,
) -> Result<(), RistError> {
    let bytes = src.as_bytes();
    if bytes.contains(&0) {
        return Err(RistError::InvalidConfig(format!(
            "{field_name} contains interior null byte"
        )));
    }
    if bytes.len() >= dst.len() {
        return Err(RistError::InvalidConfig(format!(
            "{field_name} exceeds {} bytes",
            dst.len() - 1
        )));
    }
    for d in dst.iter_mut() {
        *d = 0;
    }
    for (i, b) in bytes.iter().enumerate() {
        dst[i] = *b as c_char;
    }
    Ok(())
}

/// Test-only helpers for verifying close() idempotency / leak behaviour without
/// requiring a live network peer.
#[cfg(test)]
impl RistTransport {
    /// Returns true if the internal ctx pointer has been nulled (i.e. destroyed).
    pub(crate) fn ctx_is_null(&self) -> bool {
        self.ctx.is_null()
    }

    /// Force the alive flag to false, simulating what the fatal error path does
    /// (`rist_sender_data_write` returning a negative code other than `-2`).
    /// Does NOT touch ctx.
    pub(crate) fn force_dead_for_test(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: after an error path sets alive=false WITHOUT destroying ctx,
    /// a subsequent close() (or Drop) MUST still destroy and null the ctx.
    ///
    /// Before the fix, close() used `alive.swap(false) &&` which short-circuited
    /// when alive was already false, leaving ctx non-null (leaked rist_ctx).
    #[test]
    fn close_destroys_ctx_even_when_already_dead() {
        // Attempt to construct a real sender. Port 0 on loopback is enough for
        // the context + peer setup path; actual data-path is never exercised.
        // If construction fails (e.g. rist_sender_create fails in CI), skip.
        let mut t = match RistTransport::connect("rist://127.0.0.1:19001") {
            Ok(t) => t,
            Err(_) => return, // librist not available or port unusable — skip
        };
        assert!(
            !t.ctx_is_null(),
            "ctx should be non-null after construction"
        );

        // Simulate what happens when an error path fires: alive goes false, ctx
        // stays non-null.
        t.force_dead_for_test();
        assert!(!t.ctx_is_null(), "ctx still non-null after force_dead");
        assert!(!t.is_alive(), "alive is false after force_dead");

        // Now close() must destroy and null ctx even though alive is already false.
        t.close();
        assert!(
            t.ctx_is_null(),
            "ctx must be null after close() — rist_ctx was leaked"
        );
    }

    /// Double close must be a no-op (no double-free): calling close() twice is
    /// safe because the second call sees ctx==null and skips rist_destroy.
    #[test]
    fn double_close_is_safe() {
        let mut t = match RistTransport::connect("rist://127.0.0.1:19002") {
            Ok(t) => t,
            Err(_) => return,
        };
        t.close();
        assert!(t.ctx_is_null());
        t.close(); // must not panic / double-free
    }

    /// Pins the [`RistCancelHandle`] doc claim that a `close()` AFTER a
    /// `cancel()` still runs `rist_destroy` exactly once and a second
    /// close stays a no-op. A cancel only latches a flag — it must not
    /// consume the context, and it must not make `close()` skip it.
    #[test]
    fn close_after_cancel_destroys_ctx_exactly_once() {
        let mut t = match RistTransport::connect("rist://127.0.0.1:19006") {
            Ok(t) => t,
            Err(_) => return,
        };
        let h = t.cancel_handle();
        h.cancel();
        assert!(h.is_cancelled());
        assert!(!t.is_alive(), "a cancelled transport reports dead");
        assert!(
            !t.ctx_is_null(),
            "a cancel must NOT destroy the librist context"
        );
        t.close();
        assert!(
            t.ctx_is_null(),
            "close() after a cancel must still run rist_destroy"
        );
        t.close(); // must not panic / double-free
        assert!(t.ctx_is_null());
    }

    #[test]
    fn rejects_recv_bind_url() {
        // RistUrl with @ prefix means "receiver" — connect() should refuse.
        let r = RistTransport::connect("rist://@0.0.0.0:0");
        // Connect might fail earlier (port=0 / bind issues), so just check
        // the error contains "@ prefix" if it's InvalidConfig.
        match r {
            Err(RistError::InvalidConfig(msg)) => {
                assert!(msg.contains('@'), "expected '@' diagnostic, got: {msg}");
            }
            Err(_) => { /* other failure path also acceptable */ }
            Ok(_) => panic!("expected error for recv-bind URL"),
        }
    }

    #[test]
    fn rist_profile_to_c_maps_correctly() {
        assert_eq!(
            rist_profile_to_c(RistProfile::Simple),
            rist_sys::rist_profile_RIST_PROFILE_SIMPLE
        );
        assert_eq!(
            rist_profile_to_c(RistProfile::Main),
            rist_sys::rist_profile_RIST_PROFILE_MAIN
        );
    }

    #[test]
    fn duration_millis_u32_saturates_instead_of_wrapping() {
        use std::time::Duration;
        // Normal value: exact.
        assert_eq!(duration_millis_u32(Duration::from_millis(1500)), 1500);
        // Exactly u32::MAX ms: representable, no clamp.
        assert_eq!(
            duration_millis_u32(Duration::from_millis(u32::MAX as u64)),
            u32::MAX
        );
        // One past u32::MAX ms: a plain `as u32` cast would wrap to 0; we clamp.
        assert_eq!(
            duration_millis_u32(Duration::from_millis(u32::MAX as u64 + 1)),
            u32::MAX
        );
        // ~100 days (well past the ~49.7-day u32::MAX ceiling): clamps, not wraps.
        assert_eq!(
            duration_millis_u32(Duration::from_secs(100 * 24 * 60 * 60)),
            u32::MAX
        );
    }

    #[test]
    fn write_c_string_field_truncation_rejected() {
        let mut buf = [0 as c_char; 4];
        // 4 bytes of payload + need for null term = needs 5; rejected.
        let r = write_c_string_field(&mut buf, "abcd", "test");
        assert!(matches!(r, Err(RistError::InvalidConfig(_))));
    }

    #[test]
    fn write_c_string_field_null_byte_rejected() {
        let mut buf = [0 as c_char; 16];
        let r = write_c_string_field(&mut buf, "ab\0c", "test");
        assert!(matches!(r, Err(RistError::InvalidConfig(_))));
    }

    #[test]
    // `c_char` is `i8` on x86_64 (signed char) but `u8` on aarch64 (unsigned
    // char). The casts below are necessary on x86_64 to compare against `u8`
    // byte literals, but redundant on aarch64 — clippy::unnecessary_cast trips
    // on the aarch64 build of the same code. Tell clippy this is intentional.
    #[allow(clippy::unnecessary_cast)]
    fn write_c_string_field_writes_and_null_terminates() {
        let mut buf = [0xFF_u8 as c_char; 16];
        write_c_string_field(&mut buf, "hello", "test").unwrap();
        assert_eq!(buf[0] as u8, b'h');
        assert_eq!(buf[4] as u8, b'o');
        assert_eq!(buf[5], 0);
        // Verify the rest is zeroed (we explicitly zeroed before copy).
        assert_eq!(buf[15], 0);
    }

    /// CORR-04 (a): librist refuses a zero-length block with `-1`. That is an
    /// INPUT error — the context is untouched — so it must surface as the
    /// `TooLarge`-class input error and leave the transport alive, not as a
    /// latched `Broken` that makes `ManagedTransport` tear the context down.
    #[test]
    fn empty_payload_is_an_input_error_not_a_broken_latch() {
        let mut t = match RistTransport::connect("rist://127.0.0.1:19004") {
            Ok(t) => t,
            Err(_) => return, // librist not available or port unusable — skip
        };
        let err = t
            .send_bytes(&[])
            .expect_err("empty payload must be refused");
        assert!(
            matches!(err, TransportError::TooLarge { len: 0, max } if max == t.max_payload()),
            "empty payload must be TooLarge {{ len: 0, max: pkt_size }}, got {err:?}"
        );
        assert!(
            t.is_alive(),
            "an input error must not latch the transport dead"
        );
        // The refusal consumed nothing: a real block is still accepted (UDP
        // send needs no peer to answer; librist returns the payload length).
        t.send_bytes(&[0x47u8; 188])
            .expect("send after the empty-payload refusal must succeed");
    }

    /// librist's `rist_sender_data_write` return-code namespace (rist.c:617-716
    /// and udp.c `rist_sender_enqueue`): `-2` = sender queue full, ONE packet
    /// dropped, context healthy; any other negative = fatal; `>= 0` = the
    /// payload length that was enqueued. `-2` cannot be forced from a test
    /// (524,288-entry queue drained by the protocol thread regardless of a
    /// peer), so the mapping is pinned here and guarded live in
    /// tests/loopback.rs `send_burst_without_receiver_never_latches_broken`.
    #[test]
    fn classify_write_rc_maps_librist_namespace() {
        assert_eq!(classify_write_rc(-2), WriteOutcome::QueueFull);
        assert_eq!(classify_write_rc(-1), WriteOutcome::Fatal(-1));
        assert_eq!(classify_write_rc(-3), WriteOutcome::Fatal(-3));
        assert_eq!(classify_write_rc(0), WriteOutcome::Sent);
        assert_eq!(classify_write_rc(1316), WriteOutcome::Sent);
    }

    /// CORR-23 at the config layer: the builder / RistConfig expose both
    /// knobs, so a programmatic conflict must be refused before librist sees
    /// a last-writer-wins value.
    #[test]
    fn apply_peer_overrides_rejects_conflicting_bandwidth_and_recovery_maxbitrate() {
        let mut pc: rist_sys::rist_peer_config = unsafe { std::mem::zeroed() };
        let cfg = RistConfig {
            bandwidth_kbps: Some(1000),
            recovery_maxbitrate_kbps: Some(5000),
            ..RistConfig::default()
        };
        match apply_peer_overrides(&mut pc, &cfg) {
            Err(RistError::InvalidConfig(msg)) => {
                assert!(msg.contains("bandwidth_kbps=1000"), "got: {msg}");
                assert!(msg.contains("recovery_maxbitrate_kbps=5000"), "got: {msg}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    /// The alias on its own and the canonical knob on its own both land in
    /// `recovery_maxbitrate`; equal values are fine.
    #[test]
    fn apply_peer_overrides_bandwidth_is_an_alias_of_recovery_maxbitrate() {
        let mut pc: rist_sys::rist_peer_config = unsafe { std::mem::zeroed() };
        let cfg = RistConfig {
            bandwidth_kbps: Some(1000),
            ..RistConfig::default()
        };
        apply_peer_overrides(&mut pc, &cfg).unwrap();
        assert_eq!(pc.recovery_maxbitrate, 1000);

        let mut pc: rist_sys::rist_peer_config = unsafe { std::mem::zeroed() };
        let cfg = RistConfig {
            recovery_maxbitrate_kbps: Some(5000),
            ..RistConfig::default()
        };
        apply_peer_overrides(&mut pc, &cfg).unwrap();
        assert_eq!(pc.recovery_maxbitrate, 5000);

        let mut pc: rist_sys::rist_peer_config = unsafe { std::mem::zeroed() };
        let cfg = RistConfig {
            bandwidth_kbps: Some(4000),
            recovery_maxbitrate_kbps: Some(4000),
            ..RistConfig::default()
        };
        apply_peer_overrides(&mut pc, &cfg).unwrap();
        assert_eq!(pc.recovery_maxbitrate, 4000);
    }
}
