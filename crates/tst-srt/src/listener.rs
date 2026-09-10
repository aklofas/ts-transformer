//! Passive (listening) SRT socket.

use crate::addr::{from_sockaddr, to_sockaddr};
use crate::config::ListenerConfig;
use crate::error::{AcceptError, BindError, IoError, OptionError, last_error};
use crate::init::ensure_initialized;
use crate::socket::{
    Socket, apply_listener_config, duration_to_ms, make_cancel_handle, read_bool, set_bool, set_int,
};
use crate::transport::SrtTransport;
use os_socketaddr::OsSocketAddr;
use std::ffi::c_int;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tst_core::cancel::CancelSlot;
use tst_core::transport::TransportError;

const SRT_INVALID_SOCK: srt_sys::SRTSOCKET = -1;

// SRT_EPOLL_IN = 1 per srt.h. Signals a listener fd has an incoming
// connection ready for srt_accept. The generated constant is typed as
// SRT_EPOLL_OPT (c_uint) but srt_epoll_add_usock expects *const c_int.
const SRT_EPOLL_IN: c_int = 0x1;

/// Passive socket. Created by [`ListenerBuilder`](crate::ListenerBuilder)
/// or [`Listener::bind_with`].
///
/// # Closing
///
/// `Listener` is `Send` (libsrt is internally per-handle thread-safe).
/// It supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl calls `cancel.cancel()`, which fires
///    `srt_close(fd)` exactly once (idempotent with `close()`). Quick
///    on a listening socket — no `SRTO_LINGER` payload to drain.
/// 2. **Explicit close** — call [`Self::close`] (consuming `self`).
///    Equivalent to drop's cancel; always returns `Ok(())` (the inner
///    `srt_close` rc is currently swallowed; see method doc).
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    [`tst_core::SrtCancelHandle`] (clone-able, `Send + Sync`), then
///    `cancel()` from any thread. Closes the listening socket; a peer
///    parked in [`Self::accept`] returns
///    [`AcceptError::ListenerClosed`] within one libsrt I/O cycle
///    (~3-10 ms). Use [`Self::accept_timeout`] instead of `accept()`
///    when you need a bounded wait without out-of-band cancel.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = listener;` (Drop) or `listener.cancel_handle().cancel();` (cross-thread) |
/// | Java | Wrap as `AutoCloseable`; `try-with-resources` calls drop on exit |
/// | Kotlin | Wrap as `AutoCloseable`; `.use { }` calls drop on exit |
/// | Swift | `deinit` calls drop; `defer { handle.cancel() }` for explicit cross-thread |
/// | Python | Wrap as `__enter__`/`__exit__`; `with ... as listener:` calls drop on exit |
/// | C | (deferred — `Listener` is not directly exposed at the C ABI today) |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct Listener {
    handle: srt_sys::SRTSOCKET,
    /// Shared close-once primitive. Cloned out via `cancel_handle()` so a
    /// thread parked in `accept` can be woken from another thread. Drop
    /// calls `cancel.cancel()` so explicit `close()` and Drop never
    /// double-close.
    cancel: tst_core::SrtCancelHandle,
    /// Stored for inheritance into accepted Sockets.
    accepted_send_timeout: Option<Duration>,
    accepted_recv_timeout: Option<Duration>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("handle", &self.handle)
            .field("accepted_send_timeout", &self.accepted_send_timeout)
            .field("accepted_recv_timeout", &self.accepted_recv_timeout)
            .finish()
    }
}

unsafe impl Send for Listener {}

impl Listener {
    /// Open a passive socket, apply config, bind, and start listening.
    ///
    /// Walks every address resolved from `addr` in iterator order; first
    /// successful bind+listen wins. Same rationale as
    /// [`Socket::connect_with`]: on dual-stack hosts the iterator may
    /// return AAAA before A and the v6 entry may be unbindable (e.g. v6
    /// disabled on the interface), so we fall through to v4. Mirrors
    /// ffmpeg's `ai_next` walk.
    ///
    /// # Panics
    ///
    /// On the very first libsrt-touching call in the process, this
    /// triggers `srt_startup()` and panics if libsrt fails to initialize
    /// (returns `< 0`). That is a process-fatal condition — libsrt cannot
    /// be used at all from this process — so a panic is the correct
    /// signal. Subsequent calls reuse the once-initialized state and do
    /// not re-trigger the startup path.
    pub fn bind_with(config: &ListenerConfig, addr: impl ToSocketAddrs) -> Result<Self, BindError> {
        ensure_initialized();

        let addrs: Vec<SocketAddr> = addr
            .to_socket_addrs()
            .map_err(|e| BindError::InvalidAddress(e.into()))?
            .collect();
        if addrs.is_empty() {
            return Err(BindError::InvalidAddress(crate::error::AddrError::Resolve(
                "no addresses resolved".into(),
            )));
        }

        let mut last_err: Option<BindError> = None;
        for sa in addrs {
            let handle = unsafe { srt_sys::srt_create_socket() };
            if handle == SRT_INVALID_SOCK {
                last_err = Some(last_error().into());
                continue;
            }

            if let Err(e) = apply_listener_config(handle, config) {
                unsafe { srt_sys::srt_close(handle) };
                last_err = Some(BindError::InvalidOption(e));
                continue;
            }

            let os_addr = to_sockaddr(sa);
            let rc = unsafe {
                srt_sys::srt_bind(handle, os_addr.as_ptr().cast(), os_addr.len() as c_int)
            };
            if rc < 0 {
                let raw = last_error();
                unsafe { srt_sys::srt_close(handle) };
                last_err = Some(raw.into());
                continue;
            }

            let rc = unsafe { srt_sys::srt_listen(handle, config.backlog as c_int) };
            if rc < 0 {
                let raw = last_error();
                unsafe { srt_sys::srt_close(handle) };
                last_err = Some(raw.into());
                continue;
            }

            return Ok(Self {
                handle,
                cancel: make_cancel_handle(handle),
                accepted_send_timeout: config.send_timeout,
                accepted_recv_timeout: config.recv_timeout,
            });
        }
        Err(last_err.expect("non-empty addrs always populates last_err on full-walk failure"))
    }

    /// Block until an incoming connection completes the SRT handshake.
    pub fn accept(&mut self) -> Result<(Socket, SocketAddr), AcceptError> {
        let mut os_addr = OsSocketAddr::new();
        let mut len = os_addr.capacity() as c_int;

        let accepted =
            unsafe { srt_sys::srt_accept(self.handle, os_addr.as_mut_ptr().cast(), &raw mut len) };
        if accepted == SRT_INVALID_SOCK {
            return Err(last_error().into());
        }

        let socket = Socket::from_accepted(
            accepted,
            self.accepted_send_timeout,
            self.accepted_recv_timeout,
        )
        .map_err(|e| match e {
            IoError::Other { kind, message } => AcceptError::Other { kind, message },
            IoError::SocketClosed => AcceptError::ListenerClosed,
            IoError::System(io) => AcceptError::System(io),
        })?;

        let peer = from_sockaddr(&os_addr)
            .map_err(|e| AcceptError::System(std::io::Error::other(e.to_string())))?;
        Ok((socket, peer))
    }

    /// Accept the next incoming connection, returning
    /// [`AcceptError::TimedOut`] if no peer connects within `timeout`.
    ///
    /// **Why this exists:** libsrt's `srt_accept` does not honor the
    /// `SRTO_RCVTIMEO` socket option — [`set_recv_timeout`](Self::set_recv_timeout)
    /// on the listener inherits to accepted sockets but does *not* gate
    /// the accept call itself. This method works around that via
    /// `srt_epoll_wait`.
    ///
    /// Implementation: registers the listener fd with a one-shot epoll
    /// set on `SRT_EPOLL_IN`, drains any connection that was queued
    /// before the subscription was registered (see below), then either
    /// calls `srt_accept` (epoll signaled readiness) or returns
    /// `TimedOut`. The epoll set is always released before this method
    /// returns.
    pub fn accept_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<(Socket, SocketAddr), AcceptError> {
        // Create an epoll set for this call only.
        let eid = unsafe { srt_sys::srt_epoll_create() };
        if eid < 0 {
            return Err(AcceptError::Other {
                kind: crate::error::SrtErrno::Unknown(eid),
                message: format!("srt_epoll_create returned {eid}"),
            });
        }

        // Register the listener fd for IN (= accept-ready) events.
        // Bind to a local binding so &raw const has a place expression.
        let events: c_int = SRT_EPOLL_IN;
        let rc = unsafe { srt_sys::srt_epoll_add_usock(eid, self.handle, &raw const events) };
        if rc < 0 {
            let raw = last_error();
            unsafe { srt_sys::srt_epoll_release(eid) };
            return Err(raw.into());
        }

        // libsrt's srt_epoll_add_usock initializes a new subscription's
        // state field to 0 and does NOT scan the listener's current
        // accept queue (vendor/srt/srtcore/epoll.cpp Wait::Wait + the
        // `wait.watch & wait.state` check in srt_epoll_update_usock).
        // If a peer's handshake completed before our subscription was
        // wired in — common under workspace test load when this thread
        // is descheduled between creating the listener and reaching
        // srt_epoll_wait — that already-queued connection is invisible
        // to srt_epoll_wait and we'd hang until either *another* peer
        // connects or the timeout fires.
        //
        // Drain that pre-existing readiness with a single non-blocking
        // accept probe before going to sleep on epoll. The listener is
        // `&mut self`, so flipping SRTO_RCVSYN around the probe is
        // exclusive — no other accept caller can observe the transient
        // non-blocking state.
        match self.try_accept_nonblocking() {
            Ok(Some(conn)) => {
                unsafe { srt_sys::srt_epoll_release(eid) };
                return Ok(conn);
            }
            Ok(None) => {} // queue empty; fall through to epoll_wait
            Err(e) => {
                unsafe { srt_sys::srt_epoll_release(eid) };
                return Err(e);
            }
        }

        // msTimeOut is i64; clamp Duration to the representable range.
        let timeout_ms: i64 = timeout.as_millis().min(i64::MAX as u128) as i64;
        let mut readfds: [srt_sys::SRTSOCKET; 1] = [SRT_INVALID_SOCK];
        let mut rnum: c_int = 1;

        let n = unsafe {
            srt_sys::srt_epoll_wait(
                eid,
                readfds.as_mut_ptr(),
                &raw mut rnum,
                std::ptr::null_mut(), // writefds
                std::ptr::null_mut(), // wnum
                timeout_ms,
                std::ptr::null_mut(), // lrfds (system sockets)
                std::ptr::null_mut(), // lrnum
                std::ptr::null_mut(), // lwfds
                std::ptr::null_mut(), // lwnum
            )
        };

        unsafe { srt_sys::srt_epoll_release(eid) };

        if n == 0 {
            // Unreachable per libsrt's contract: srt_epoll_wait returns
            // either > 0 (event ready) or -1 with SRT_ETIMEOUT on timeout.
            // The n == 0 case cannot arise from a blocking epoll_wait call.
            // Treated as timeout defensively: if libsrt ever changes its
            // contract, falling through to srt_accept would block forever.
            return Err(AcceptError::TimedOut);
        }
        if n < 0 {
            return Err(last_error().into());
        }

        // n > 0: listener fd is accept-ready; delegate to the blocking path.
        // srt_accept will return immediately because epoll confirmed readiness.
        self.accept()
    }

    /// Non-blocking accept probe: returns `Ok(Some(...))` if a
    /// connection is already queued, `Ok(None)` if the queue is empty
    /// (libsrt `SRT_EASYNCRCV`), or `Err(...)` for any other libsrt
    /// failure. Toggles `SRTO_RCVSYN` to false for the duration of the
    /// `srt_accept` call and restores it before returning.
    fn try_accept_nonblocking(&mut self) -> Result<Option<(Socket, SocketAddr)>, AcceptError> {
        let prev_rcvsyn = read_bool(self.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN).unwrap_or(true);
        if set_bool(self.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN, false).is_err() {
            // Can't flip to non-blocking — skip the probe and let
            // srt_epoll_wait drive timing. The probe is an optimistic
            // race-recovery; if it can't run we fall back to the prior
            // behavior (which is correct for the non-racy case).
            return Ok(None);
        }

        let mut os_addr = OsSocketAddr::new();
        let mut len = os_addr.capacity() as c_int;
        let accepted =
            unsafe { srt_sys::srt_accept(self.handle, os_addr.as_mut_ptr().cast(), &raw mut len) };

        // Capture libsrt's last-error BEFORE restoring SRTO_RCVSYN, since
        // the restore is itself a libsrt call that overwrites the
        // thread-local last-error slot.
        let probe_outcome: Result<Option<srt_sys::SRTSOCKET>, AcceptError> =
            if accepted == SRT_INVALID_SOCK {
                let raw_code = unsafe { srt_sys::srt_getlasterror(std::ptr::null_mut()) };
                if raw_code == srt_sys::SRT_ERRNO_SRT_EASYNCRCV as c_int {
                    Ok(None)
                } else {
                    Err(last_error().into())
                }
            } else {
                Ok(Some(accepted))
            };

        // Always restore the original blocking mode, even on probe error.
        let _ = set_bool(self.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN, prev_rcvsyn);

        match probe_outcome {
            Ok(None) => Ok(None),
            Ok(Some(accepted)) => {
                let socket = Socket::from_accepted(
                    accepted,
                    self.accepted_send_timeout,
                    self.accepted_recv_timeout,
                )
                .map_err(|e| match e {
                    IoError::Other { kind, message } => AcceptError::Other { kind, message },
                    IoError::SocketClosed => AcceptError::ListenerClosed,
                    IoError::System(io) => AcceptError::System(io),
                })?;
                let peer = from_sockaddr(&os_addr)
                    .map_err(|e| AcceptError::System(std::io::Error::other(e.to_string())))?;
                Ok(Some((socket, peer)))
            }
            Err(e) => Err(e),
        }
    }

    pub fn local_addr(&self) -> Result<SocketAddr, IoError> {
        let mut os_addr = OsSocketAddr::new();
        let mut len = os_addr.capacity() as c_int;
        let rc = unsafe {
            srt_sys::srt_getsockname(self.handle, os_addr.as_mut_ptr().cast(), &raw mut len)
        };
        if rc < 0 {
            return Err(last_error().into());
        }
        from_sockaddr(&os_addr).map_err(|e| IoError::System(std::io::Error::other(e.to_string())))
    }

    /// Set the receive timeout that will be inherited by accepted [`Socket`]s.
    ///
    /// **Important:** this does *not* gate the [`accept`](Self::accept) call
    /// itself — libsrt's `srt_accept` does not honor `SRTO_RCVTIMEO`. To
    /// time-bound the accept call, use [`accept_timeout`](Self::accept_timeout)
    /// instead.
    pub fn set_recv_timeout(&mut self, timeout: Option<Duration>) -> Result<(), OptionError> {
        let ms = timeout.map(duration_to_ms).unwrap_or(-1);
        set_int(self.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVTIMEO, ms)?;
        self.accepted_recv_timeout = timeout;
        Ok(())
    }

    /// Explicit close, consuming the listener. Idempotent (Drop also calls
    /// cancel).
    ///
    /// **Single-owner contract.** Because `close` takes `self` by value, it is
    /// inherently the owning thread's operation — it cannot be called while this
    /// thread holds the `&mut self` borrow that `accept()` requires. To wake a
    /// thread parked in `accept()` from *another* thread, use the independent
    /// handle from [`cancel_handle`](Self::cancel_handle): `cancel()` closes the
    /// underlying SRT socket (the parked `accept()` returns
    /// `AcceptError::ListenerClosed`) without consuming or freeing the listener,
    /// so the owning thread still drops/`close()`s it afterwards.
    ///
    /// (Bindings that expose the listener over a raw handle must uphold the same
    /// contract: free the listener only on the owning thread once `accept()` has
    /// returned, and use `cancel_handle()` — never a free-then-wake — for the
    /// cross-thread wake.)
    ///
    /// **Always returns `Ok`.** The `Result` is retained for API stability
    /// and may carry an error in a future revision (the underlying
    /// `srt_close` rc is currently swallowed by the `SrtCancelHandle` closer).
    pub fn close(self) -> Result<(), IoError> {
        self.cancel.cancel();
        Ok(())
    }

    /// Clone-able cancel handle — the sanctioned cross-thread wake for a parked
    /// `accept()`. Calling `cancel()` from any thread closes the underlying SRT
    /// listener socket without freeing the listener itself. Idempotent.
    pub fn cancel_handle(&self) -> tst_core::SrtCancelHandle {
        self.cancel.clone()
    }

    /// Bind `addr`, accept **one** connection, and return it as an
    /// [`SrtTransport`] — with the accept reachable by `slot.cancel()` from
    /// another thread.
    ///
    /// This is the single home of the bind → install → accept → clear →
    /// classify sequence every managed listener-mode factory needs. A
    /// managed receiver in listener mode re-runs its factory after a peer
    /// disconnects, and that factory parks in [`accept`](Self::accept)
    /// until the next peer shows up; without a published cancel target the
    /// caller's `cancel` cannot reach it. So the listener's
    /// [`cancel_handle`](Self::cancel_handle) goes into `slot` around the
    /// accept: firing the slot closes the listening socket, the accept
    /// returns, and this reports [`TransportError::ExplicitClose`] so the
    /// caller surfaces a caller-initiated close rather than a fault.
    ///
    /// Order of checks: bail before binding if the slot is already
    /// cancelled (no socket for a cancelled caller); after the accept, any
    /// error while the slot is cancelled is the cancel
    /// ([`AcceptError::ListenerClosed`] today — not depended on), not a
    /// transport fault. The listener is dropped on return — single-accept
    /// semantics, matching the connection-oriented shape of the binding
    /// entry points that call this.
    ///
    /// # Errors
    ///
    /// - [`TransportError::ExplicitClose`] — the slot was cancelled before
    ///   or during the accept.
    /// - [`TransportError::Broken`] — bind or accept fault. `errno_code` is
    ///   `None`: these are not libsrt `MJ_*` errnos in the typed sense, so
    ///   the message carries the detail.
    pub fn accept_one_cancellable(
        cfg: &ListenerConfig,
        addr: &str,
        slot: &CancelSlot,
    ) -> Result<SrtTransport, TransportError> {
        if slot.is_cancelled() {
            return Err(TransportError::ExplicitClose);
        }
        let mut listener = Self::bind_with(cfg, addr).map_err(|e| TransportError::Broken {
            msg: format!("bind: {e}"),
            errno_code: None,
        })?;
        slot.install(Arc::new(listener.cancel_handle()));
        let accepted = listener.accept();
        slot.clear();
        match accepted {
            Ok((socket, _peer)) => Ok(SrtTransport::new(socket)),
            Err(_) if slot.is_cancelled() => Err(TransportError::ExplicitClose),
            Err(e) => Err(TransportError::Broken {
                msg: format!("accept: {e}"),
                errno_code: None,
            }),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // No-op if explicit close() / cancel() already fired.
        self.cancel.cancel();
    }
}
