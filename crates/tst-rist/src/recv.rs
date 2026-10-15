//! [`RistRecvTransport`] — RIST receiver impl [`RecvTransport`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::transport::{BrokenCause, RecvTransport, TransportCancel, TransportError};

use crate::config::RistConfig;
use crate::error::RistError;
use crate::stats::RistStats;
use crate::transport::RistCancelHandle;
use crate::url::RistUrl;

/// Default per-`recv_bytes` poll interval in milliseconds. Short enough that
/// `close()` from another thread (via `alive` swap) is observed quickly.
const POLL_TIMEOUT_MS: i32 = 100;

/// What to do with a librist data block once read. Pure decision so it can be
/// unit-tested without a live RIST session.
#[derive(Debug, PartialEq, Eq)]
enum BlockDisposition {
    /// Payload that fits (≤ buf) — copy `n` bytes (`n` ≥ 1; a zero-length
    /// block is [`BlockDisposition::SkipEmpty`]).
    Accept(usize),
    /// `payload_len > buf.len()` — caller buffer too small; drop the datagram
    /// (non-fatal, retryable) rather than killing the transport.
    DropOversize,
    /// `payload_len > 0` with a NULL payload pointer — malformed block from
    /// librist; drop it (non-fatal, retryable).
    DropMalformed,
    /// A zero-length block. librist has no end of stream and the shells read
    /// `Ok(0)` as "closed", so the block is freed and the tick reports
    /// `Backpressure`.
    SkipEmpty,
}

/// Classify a librist block. `payload_null` is `payload_ptr.is_null()`.
fn classify_block(payload_len: usize, payload_null: bool, buf_len: usize) -> BlockDisposition {
    if payload_len == 0 {
        return BlockDisposition::SkipEmpty;
    }
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
/// Wraps a librist `rist_ctx` configured as a receiver. Each `recv_bytes`
/// call blocks at most ~100 ms (`rist_receiver_data_read2` poll) and returns
/// `TransportError::Backpressure` when nothing arrived; the retry loop lives
/// in the pipeline shells. A [`RistCancelHandle`] (from
/// [`RistRecvTransport::cancel_handle`]) fired from another thread ends the
/// current tick with `TransportError::ExplicitClose`. Drop calls
/// `rist_destroy`.
pub struct RistRecvTransport {
    ctx: crate::ctx::RistCtx,
    alive: Arc<AtomicBool>,
    /// Set by [`RistCancelHandle::cancel`]; read at entry and at the end of
    /// every poll tick before `alive`.
    cancelled: Arc<AtomicBool>,
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
        Ok(Self {
            ctx: crate::ctx::RistCtx::open(url, cfg, crate::ctx::Role::Receiver)?,
            alive: Arc::new(AtomicBool::new(true)),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Bind URL the receiver was built against (for diagnostics).
    pub fn bind_url(&self) -> &str {
        &self.ctx.endpoint
    }

    /// Cross-thread cancel handle for this receiver; see [`RistCancelHandle`].
    pub fn cancel_handle(&self) -> RistCancelHandle {
        RistCancelHandle::from_flag(self.cancelled.clone())
    }

    /// Current snapshot of cumulative stats.
    pub fn stats(&self) -> RistStats {
        self.ctx.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// Count one dropped datagram (oversize / malformed). Centralized so the
    /// counter source is consistent with stats living behind a lock.
    fn bump_dropped(&self) {
        if let Ok(mut s) = self.ctx.stats.lock() {
            s.packets_dropped = s.packets_dropped.wrapping_add(1);
        }
    }
}

impl RecvTransport for RistRecvTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // An empty destination is a no-op — `Ok(0)` before librist
        // or either flag is consulted (the kit row `empty_recv_is_noop`;
        // without this an empty read burns a 100 ms tick and reports
        // `Backpressure`, or drops a delivered block as `DropOversize`).
        //
        // Deliberately FIRST, ahead of the cancelled/closed checks: an empty
        // read asks nothing of the transport, so it answers the same on a
        // cancelled or closed one. Same ordering as tst-udp/tcp/rtp.
        if buf.is_empty() {
            return Ok(0);
        }
        // Cancel wins over close (same order as the sender).
        if self.cancelled.load(Ordering::Acquire) {
            return Err(TransportError::ExplicitClose);
        }
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }

        let mut block: *mut rist_sys::rist_data_block = std::ptr::null_mut();
        let rc = unsafe {
            rist_sys::rist_receiver_data_read2(self.ctx.as_ptr(), &mut block, POLL_TIMEOUT_MS)
        };

        if rc < 0 {
            self.alive.store(false, Ordering::Release);
            return Err(TransportError::Broken {
                msg: format!("rist_receiver_data_read2 returned {rc}"),
                errno_code: Some(rc),
                cause: BrokenCause::Unspecified,
            });
        }
        if rc == 0 || block.is_null() {
            // The tick elapsed with nothing to read. A cancel that landed
            // while we were parked is reported HERE — one tick of latency,
            // never a Backpressure that hides it.
            if self.cancelled.load(Ordering::Acquire) {
                return Err(TransportError::ExplicitClose);
            }
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
            BlockDisposition::SkipEmpty => {
                // SAFETY: block is non-null (rc > 0); freed exactly once here.
                unsafe {
                    rist_sys::rist_receiver_data_block_free2(&mut block);
                }
                // Still a received packet; keeps the count consistent with
                // tst-udp.
                if let Ok(mut s) = self.ctx.stats.lock() {
                    s.packets_received = s.packets_received.wrapping_add(1);
                }
                Err(TransportError::Backpressure {
                    msg: "rist recv: skipped a zero-length block".into(),
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
                if let Ok(mut s) = self.ctx.stats.lock() {
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
        self.ctx.destroy();
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

    /// Strong references to the stats cell: 2 while librist holds the
    /// callback's leaked one, 1 once `close()` has reclaimed it.
    pub(crate) fn stats_refs_for_test(&self) -> usize {
        Arc::strong_count(&self.ctx.stats)
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
        assert_eq!(classify_block(0, true, 200), SkipEmpty); // empty block: not EOS
        assert_eq!(classify_block(0, false, 200), SkipEmpty);
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

    /// The Simple-profile IPv6 receiver-bind guard returns before any librist
    /// context or peer creation, so this runs everywhere — no socket is opened
    /// and the vendored-librist null-deref is never reached.
    #[test]
    fn refuses_simple_profile_ipv6_bind() {
        use crate::error::RistErrorKind;

        let r = RistRecvTransport::listen("rist://@[::1]:33024?profile=simple");
        match r {
            Err(e) => {
                assert_eq!(
                    e.kind(),
                    RistErrorKind::InvalidConfig,
                    "got {e:?}, expected InvalidConfig"
                );
                // Not the '@'-missing InvalidConfig — this must be the guard.
                assert!(
                    e.to_string().contains("Simple profile"),
                    "expected the Simple-profile guard message, got: {e}"
                );
            }
            Ok(_) => panic!("Simple-profile IPv6 bind must be refused, not attempted"),
        }
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

    #[test]
    fn close_reclaims_the_stats_reference_exactly_once() {
        let mut t = match RistRecvTransport::listen("rist://@127.0.0.1:19008") {
            Ok(t) => t,
            Err(_) => return,
        };
        assert_eq!(t.stats_refs_for_test(), 2);
        t.close();
        assert_eq!(t.stats_refs_for_test(), 1);
        t.close();
        assert_eq!(t.stats_refs_for_test(), 1);
    }

    /// Without the `mbedtls` feature, a config asking for encryption is
    /// refused as `EncryptionDisabled` before the Simple-profile IPv6 guard
    /// looks at the address.
    #[cfg(not(feature = "mbedtls"))]
    #[test]
    fn encryption_check_precedes_the_ipv6_guard() {
        use crate::config::{EncryptionKey, RistProfile};
        let url = RistUrl::parse("rist://@[::1]:33025").unwrap();
        let cfg = RistConfig {
            profile: RistProfile::Simple,
            encryption: Some(EncryptionKey::aes128("s")),
            ..RistConfig::default()
        };
        assert!(matches!(
            RistRecvTransport::listen_with_config(&url, &cfg),
            Err(RistError::EncryptionDisabled)
        ));
    }
}
