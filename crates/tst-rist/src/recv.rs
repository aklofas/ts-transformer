//! [`RistRecvTransport`] — RIST receiver impl [`RecvTransport`].

use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::transport::{RecvTransport, TransportError};

use crate::config::{RistConfig, RistProfile};
use crate::error::RistError;
use crate::init::ensure_init;
use crate::stats::{OnDrop, RistStats};
use crate::transport::{apply_peer_overrides, global_logging_ptr, rist_profile_to_c};
use crate::url::{RistUrl, native_endpoint};

/// Default per-`recv_bytes` poll interval in milliseconds. Short enough that
/// `close()` from another thread (via `alive` swap) is observed quickly.
const POLL_TIMEOUT_MS: i32 = 100;

/// What to do with a librist data block once read. Pure decision so it can be
/// unit-tested without a live RIST session.
#[derive(Debug, PartialEq, Eq)]
enum BlockDisposition {
    /// Payload that fits (≤ buf) — copy `n` bytes (`n` may be 0 for an
    /// empty/zero-length block).
    Accept(usize),
    /// `payload_len > buf.len()` — caller buffer too small; drop the datagram
    /// (non-fatal, retryable) rather than killing the transport.
    DropOversize,
    /// `payload_len > 0` with a NULL payload pointer — malformed block from
    /// librist; drop it (non-fatal, retryable).
    DropMalformed,
}

/// Classify a librist block. `payload_null` is `payload_ptr.is_null()`.
fn classify_block(payload_len: usize, payload_null: bool, buf_len: usize) -> BlockDisposition {
    if payload_len > 0 && payload_null {
        return BlockDisposition::DropMalformed;
    }
    if payload_len > buf_len {
        return BlockDisposition::DropOversize;
    }
    // payload_len <= buf_len here (the DropOversize early-return above caught
    // the larger case), so no clamp is needed.
    BlockDisposition::Accept(payload_len)
}

/// Receive-side RIST transport.
///
/// Wraps a librist `rist_ctx` configured as a receiver. Per-call recv via
/// `rist_receiver_data_read2` with a short poll timeout + cancel-poll loop.
/// Drop calls `rist_destroy`.
pub struct RistRecvTransport {
    ctx: *mut rist_sys::rist_ctx,
    bind_url: String,
    alive: Arc<AtomicBool>,
    stats: Arc<Mutex<RistStats>>,
    /// Leaked `Arc<Mutex<RistStats>>` ref handed to librist as the stats
    /// callback `arg`. Reclaimed exactly once in `close()` after `rist_destroy`.
    stats_arg: *mut c_void,
}

// Same Send/!Sync reasoning as RistTransport: RecvTransport methods take
// &mut self, so single-threaded mutation is borrow-checker enforced.
unsafe impl Send for RistRecvTransport {}

impl RistRecvTransport {
    /// Build a receiver from a URL using defaults.
    ///
    /// The URL must be a bind form (`rist://@host:port`) per the ffmpeg
    /// convention.
    pub fn listen(url: &str) -> Result<Self, RistError> {
        let parsed = RistUrl::parse(url)?;
        if !parsed.is_recv_bind {
            return Err(RistError::InvalidConfig(
                "URL missing '@' prefix — use rist://@host:port for receivers".into(),
            ));
        }
        let mut cfg = RistConfig::default();
        cfg.merge_from_url(&parsed);
        Self::listen_with_config(&parsed, &cfg)
    }

    /// Build a receiver from a parsed URL + config.
    pub fn listen_with_config(url: &RistUrl, cfg: &RistConfig) -> Result<Self, RistError> {
        ensure_init();

        #[cfg(not(feature = "mbedtls"))]
        if cfg.encryption.is_some() {
            return Err(RistError::EncryptionDisabled);
        }

        // Vendored librist 0.2.20's Simple-profile receiver-side RTCP-peer
        // creation dereferences the new peer before checking it for null
        // (rist.c's rist_receiver_peer_create); that null case is reachable
        // for an IPv6 bind (the RTCP peer's re-derived bind URL fails to
        // come up as IPv6), which SIGSEGVs the whole process. Refuse here,
        // before any librist call, rather than let the process crash.
        // Caller/sender side is unaffected — rist_sender_peer_create
        // null-checks before dereferencing — so only the receiver bind is
        // guarded.
        if cfg.profile == RistProfile::Simple && url.addr.is_ipv6() {
            return Err(RistError::InvalidConfig(
                "IPv6 bind is not supported in the Simple profile: vendored librist 0.2.20 \
                 dereferences the RTCP peer before its null check (rist.c \
                 rist_receiver_peer_create); use RistProfile::Main"
                    .into(),
            ));
        }

        let profile = rist_profile_to_c(cfg.profile);
        let logging_settings = global_logging_ptr();

        // ===== Create receiver context =====
        let mut ctx: *mut rist_sys::rist_ctx = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_receiver_create(&mut ctx, profile, logging_settings) };
        if rc != 0 || ctx.is_null() {
            return Err(RistError::ContextCreateFailed);
        }
        // Guard that calls rist_destroy on every error-exit path. Disarmed at
        // the bottom of this constructor once ctx is safely in Self.
        let mut ctx_guard = OnDrop::new(|| unsafe {
            rist_sys::rist_destroy(ctx);
        });

        // ===== Parse bind URL into peer_config =====
        let bind_url_str = native_endpoint(url, true);
        let bind_url_c = CString::new(bind_url_str.clone())
            .map_err(|e| RistError::InvalidConfig(format!("bad bind URL: {e}")))?;

        let mut peer_config: *mut rist_sys::rist_peer_config = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_parse_address2(bind_url_c.as_ptr(), &mut peer_config) };
        if rc != 0 || peer_config.is_null() {
            return Err(RistError::Ffi {
                code: rc,
                function: "rist_parse_address2",
            });
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // Receiver = listener: initiate_conn=0.
        unsafe {
            (*peer_config).initiate_conn = 0;
        }

        // Reuse the sender-side override application (cname / secret / buffer
        // / etc. apply symmetrically).
        if let Err(e) = apply_peer_overrides(peer_config, cfg) {
            unsafe {
                // peer_config_free2 must happen before rist_destroy (guard drop).
                rist_sys::rist_peer_config_free2(&mut peer_config);
            }
            return Err(e);
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // ===== Add peer (bind endpoint) =====
        let mut peer: *mut rist_sys::rist_peer = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_peer_create(ctx, &mut peer, peer_config) };
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
            bind_url: bind_url_str,
            alive: Arc::new(AtomicBool::new(true)),
            stats,
            stats_arg,
        })
    }

    /// Bind URL the receiver was built against (for diagnostics).
    pub fn bind_url(&self) -> &str {
        &self.bind_url
    }

    /// Current snapshot of cumulative stats.
    pub fn stats(&self) -> RistStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Count one dropped datagram (oversize / malformed). Centralized so the
    /// counter source is consistent now that stats live behind a lock (Task 3).
    fn bump_dropped(&self) {
        if let Ok(mut s) = self.stats.lock() {
            s.packets_dropped = s.packets_dropped.wrapping_add(1);
        }
    }
}

impl RecvTransport for RistRecvTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }

        let mut block: *mut rist_sys::rist_data_block = std::ptr::null_mut();
        let rc =
            unsafe { rist_sys::rist_receiver_data_read2(self.ctx, &mut block, POLL_TIMEOUT_MS) };

        if rc < 0 {
            self.alive.store(false, Ordering::Release);
            return Err(TransportError::Broken {
                msg: format!("rist_receiver_data_read2 returned {rc}"),
                errno_code: Some(rc),
            });
        }
        if rc == 0 || block.is_null() {
            // Timeout — transport is alive, nothing to read this tick. Per
            // RecvTransport contract, surface as Backpressure (retryable).
            return Err(TransportError::Backpressure {
                msg: "rist recv timeout".into(),
                errno_code: None,
            });
        }

        // SAFETY: block is non-null per rc > 0; payload + payload_len come
        // from librist's internal ringbuffer and are valid until we call
        // rist_receiver_data_block_free2.
        let (payload_ptr, payload_len) =
            unsafe { ((*block).payload as *const u8, (*block).payload_len) };

        // Classify the block before touching any memory or freeing, so each
        // arm below owns exactly one free of `block`.
        match classify_block(payload_len, payload_ptr.is_null(), buf.len()) {
            BlockDisposition::DropMalformed => {
                // Non-fatal: free the block and return Backpressure so the
                // pipeline receive loop retries. Do NOT set alive=false — one
                // malformed block from librist does not break the session.
                unsafe {
                    rist_sys::rist_receiver_data_block_free2(&mut block);
                }
                self.bump_dropped();
                Err(TransportError::Backpressure {
                    msg: format!(
                        "rist recv: dropped malformed block (null payload, len={payload_len})"
                    ),
                    errno_code: None,
                })
            }
            BlockDisposition::DropOversize => {
                // Non-fatal: caller's buffer is smaller than the datagram. Free
                // the block and return Backpressure so the next call reads the
                // next packet. The transport stays alive — individual oversize
                // datagrams do not indicate a protocol error.
                unsafe {
                    rist_sys::rist_receiver_data_block_free2(&mut block);
                }
                self.bump_dropped();
                Err(TransportError::Backpressure {
                    msg: format!(
                        "rist recv: dropped oversize datagram (have {}, need {payload_len})",
                        buf.len()
                    ),
                    errno_code: None,
                })
            }
            BlockDisposition::Accept(copy_n) => {
                if copy_n > 0 && !payload_ptr.is_null() {
                    // SAFETY: payload_ptr valid for payload_len bytes until free;
                    // copy_n <= payload_len and <= buf.len() by classify_block.
                    unsafe {
                        std::ptr::copy_nonoverlapping(payload_ptr, buf.as_mut_ptr(), copy_n);
                    }
                }
                // Always release the block back to librist.
                unsafe {
                    rist_sys::rist_receiver_data_block_free2(&mut block);
                }
                if let Ok(mut s) = self.stats.lock() {
                    s.bytes_received = s.bytes_received.wrapping_add(copy_n as u64);
                    s.packets_received = s.packets_received.wrapping_add(1);
                }
                Ok(copy_n)
            }
        }
    }

    fn max_payload(&self) -> usize {
        // Recv-side deliverable ceiling (see RecvTransport::max_payload
        // in tst-core): RIST rides RTP over UDP, so the 16-bit UDP
        // length field (65535) bounds any datagram, and vendored
        // librist additionally caps every delivered block at
        // RIST_MAX_PACKET_SIZE = 10000 (rist-private.h — its enc/dec/
        // recv buffers are all this size) — 65535 is a generous,
        // allocation-cheap upper bound matching the UDP transport's
        // convention. pkt_size is the send-side budget and deliberately
        // does not factor in. classify_block keeps the oversize guard
        // as defence-in-depth for direct callers with smaller buffers.
        65535
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

impl Drop for RistRecvTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Test-only helpers mirroring those on `RistTransport`.
#[cfg(test)]
impl RistRecvTransport {
    pub(crate) fn ctx_is_null(&self) -> bool {
        self.ctx.is_null()
    }

    pub(crate) fn force_dead_for_test(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_block_decisions() {
        use BlockDisposition::*;
        assert_eq!(classify_block(100, false, 200), Accept(100)); // fits
        assert_eq!(classify_block(200, false, 200), Accept(200)); // exact fit
        assert_eq!(classify_block(201, false, 200), DropOversize); // 1 over
        assert_eq!(classify_block(50, true, 200), DropMalformed); // null+len>0
        assert_eq!(classify_block(0, true, 200), Accept(0)); // null+len==0 is OK (empty)
        assert_eq!(classify_block(0, false, 200), Accept(0));
    }

    /// Regression: after an error path sets alive=false WITHOUT destroying ctx,
    /// a subsequent close() (or Drop) MUST still destroy and null the ctx.
    #[test]
    fn close_destroys_ctx_even_when_already_dead() {
        let mut t = match RistRecvTransport::listen("rist://@0.0.0.0:19003") {
            Ok(t) => t,
            Err(_) => return,
        };
        assert!(!t.ctx_is_null());

        t.force_dead_for_test();
        assert!(!t.ctx_is_null(), "ctx still non-null after force_dead");
        assert!(!t.is_alive(), "alive is false after force_dead");

        t.close();
        assert!(
            t.ctx_is_null(),
            "ctx must be null after close() — rist_ctx was leaked"
        );
    }

    #[test]
    fn double_close_is_safe() {
        let mut t = match RistRecvTransport::listen("rist://@0.0.0.0:19004") {
            Ok(t) => t,
            Err(_) => return,
        };
        t.close();
        assert!(t.ctx_is_null());
        t.close(); // must not panic / double-free
    }

    #[test]
    fn rejects_non_bind_url() {
        // Plain rist:// without @ prefix is a sender URL.
        let r = RistRecvTransport::listen("rist://1.2.3.4:0");
        match r {
            Err(RistError::InvalidConfig(msg)) => {
                assert!(msg.contains('@'), "expected '@' diagnostic, got: {msg}");
            }
            Err(_) => { /* other error acceptable (port=0 etc.) */ }
            Ok(_) => panic!("expected error for non-bind URL"),
        }
    }
}
