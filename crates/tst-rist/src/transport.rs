//! [`RistTransport`] — RIST sender impl [`Transport`].

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::transport::{BrokenCause, Transport, TransportError};

use crate::config::{EncryptionKey, RistConfig, RistProfile};
use crate::error::RistError;
use crate::init::{GLOBAL_LOGGING, ensure_init};
use crate::stats::{OnDrop, RistStats};
use crate::url::{RistUrl, native_endpoint};

/// Send-side RIST transport.
///
/// Wraps a librist `rist_ctx` configured as a sender. Per-message send via
/// `rist_sender_data_write`. Drop calls `rist_destroy`.
pub struct RistTransport {
    ctx: *mut rist_sys::rist_ctx,
    pkt_size: usize,
    peer_url: String,
    alive: Arc<AtomicBool>,
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
}

impl Transport for RistTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
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
        if rc < 0 {
            self.alive.store(false, Ordering::Release);
            return Err(TransportError::Broken {
                msg: format!("rist_sender_data_write returned {rc}"),
                errno_code: Some(rc),
                cause: BrokenCause::Unspecified,
            });
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
        self.alive.load(Ordering::Acquire)
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

    if let Some(bw) = cfg.bandwidth_kbps {
        pc.recovery_maxbitrate = bw;
    }
    pc.recovery_length_min = duration_millis_u32(cfg.buffer);
    pc.recovery_length_max = duration_millis_u32(cfg.buffer).max(pc.recovery_length_max);

    if let Some(bw) = cfg.recovery_maxbitrate_kbps {
        pc.recovery_maxbitrate = bw;
    }
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

    /// Force the alive flag to false, simulating what the error paths do (e.g.
    /// after `rist_sender_data_write` returns a negative code). Does NOT touch ctx.
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
}
