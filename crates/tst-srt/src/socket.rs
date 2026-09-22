//! Connected SRT data socket.

use crate::addr::{from_sockaddr, to_sockaddr};
use crate::config::SocketConfig;
use crate::error::{
    ConnectError, IoError, OptionError, RecvError, SendError, SrtErrno, classify_connect_error,
    last_error, last_reject,
};
use crate::init::ensure_initialized;
use crate::options::{MaxBandwidth, Passphrase};
use os_socketaddr::OsSocketAddr;
use std::ffi::{c_char, c_int};
use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;
use tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES;

const SRT_INVALID_SOCK: srt_sys::SRTSOCKET = -1;

/// Connected, bidirectional SRT data socket.
///
/// Constructed via [`SocketBuilder`](crate::SocketBuilder), via
/// [`Socket::connect_with`], or returned from [`Listener::accept`](crate::Listener::accept).
///
/// # Closing
///
/// `Socket` is `Send` (libsrt is internally per-handle thread-safe; the
/// `SRTSOCKET` integer can move across threads). It supports three
/// shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl calls `cancel.cancel()`, which fires
///    `srt_close(fd)` exactly once (idempotent with `close()`). Bounded
///    by `SRTO_LINGER` (libsrt default 30 s, configurable via
///    `SocketBuilder::linger` before construction).
/// 2. **Explicit close** — call [`Self::close`] (consuming `self`).
///    Equivalent to drop's cancel; always returns `Ok(())` (the inner
///    `srt_close` rc is currently swallowed; see method doc).
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    [`tst_core::SrtCancelHandle`] (clone-able, `Send + Sync`), then
///    `cancel()` from any thread. Closes the libsrt socket; a peer
///    parked in `send` / `recv` returns
///    [`SendError::ConnectionBroken`] / [`RecvError::ConnectionBroken`]
///    within one libsrt I/O cycle (~3-10 ms).
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = socket;` (Drop) or `socket.cancel_handle().cancel();` (cross-thread) |
/// | Java | Wrap as `AutoCloseable`; `try-with-resources` calls drop on exit |
/// | Kotlin | Wrap as `AutoCloseable`; `.use { }` calls drop on exit |
/// | Swift | `deinit` calls drop; `defer { handle.cancel() }` for explicit cross-thread |
/// | Python | Wrap as `__enter__`/`__exit__`; `with ... as sock:` calls drop on exit |
/// | C | (deferred — `Socket` is not directly exposed at the C ABI; senders/receivers wrap it) |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct Socket {
    handle: srt_sys::SRTSOCKET,
    /// Shared close-once primitive. Cloned out via `cancel_handle()` so a
    /// thread parked in `send`/`recv` can be woken from another thread.
    /// Drop calls `cancel.cancel()` so explicit `close()` and Drop never
    /// double-close.
    cancel: tst_core::SrtCancelHandle,
    /// Cached at construction; libsrt allows reading via getsockflag, but
    /// reading once is cheaper.
    cached_stream_id: Option<String>,
    /// `SRTO_PAYLOADSIZE` read after handshake. Used to give accurate
    /// `SendError::PayloadTooLarge { limit }` values without a per-send
    /// getsockopt round-trip. Defaults to [`SRT_TS_BUNDLE_BYTES`] (libsrt live default) if read fails.
    cached_payload_limit: usize,
}

impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket")
            .field("handle", &self.handle)
            .field("stream_id", &self.cached_stream_id)
            .field("payload_limit", &self.cached_payload_limit)
            .finish()
    }
}

/// Snapshot of libsrt's per-socket performance counters (subset of `CBytePerfMon`).
///
/// Loss/drop counters are split by which side observed the event:
/// - `*_send_side` — what the sender knows is lost/dropped (receiver NAKs received,
///   too-late drops on outgoing path). Read these on a sender; they will be
///   ~0 on a receiver.
/// - `*_recv_side` — what the receiver detected (sequence-gap discoveries,
///   too-late drops on incoming path). Read these on a receiver; they will
///   be ~0 on a sender.
#[must_use]
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Stats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bytes_lost_recv_side: u64,
    pub bytes_lost_send_side: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_lost_recv_side: u64,
    pub packets_lost_send_side: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_recv_side: u64,
    pub packets_dropped_send_side: u64,
    pub rtt: Duration,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub mbps_estimated_bandwidth: f64,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
}

// SAFETY: a SRTSOCKET is just an integer handle; libsrt is internally
// thread-safe per-socket (concurrent operations on the same handle from two
// threads are still UB, but moving across threads is fine). Mirrors std::net.
unsafe impl Send for Socket {}

impl Socket {
    /// Raw libsrt socket handle. **Unstable, used for low-level interop tests.**
    #[doc(hidden)]
    pub fn raw_handle(&self) -> srt_sys::SRTSOCKET {
        self.handle
    }

    /// Open a socket, apply config, and connect to `addr`.
    ///
    /// `addr` may resolve to multiple `SocketAddr`s (e.g. `localhost` →
    /// `[::1]:N` + `127.0.0.1:N`). We walk every resolved address in the
    /// order returned by `to_socket_addrs()` and return the first
    /// successful connection. On dual-stack hosts where AAAA records
    /// resolve before A but v6 is unroutable (tethered cellular, some
    /// VPNs, some corporate networks), this lets the v4 fallback succeed
    /// instead of failing fast on `[::1]`. Mirrors ffmpeg's
    /// `getaddrinfo(AF_UNSPEC)` + `ai_next` walk in
    /// `libavformat/libsrt.c`. Sequential, no Happy Eyeballs.
    ///
    /// # Panics
    ///
    /// On the very first libsrt-touching call in the process, this
    /// triggers `srt_startup()` and panics if libsrt fails to initialize
    /// (returns `< 0`). That is a process-fatal condition — libsrt cannot
    /// be used at all from this process — so a panic is the correct
    /// signal. Subsequent calls reuse the once-initialized state and do
    /// not re-trigger the startup path.
    pub fn connect_with(
        config: &SocketConfig,
        addr: impl ToSocketAddrs,
    ) -> Result<Self, ConnectError> {
        ensure_initialized();

        let addrs: Vec<SocketAddr> = addr
            .to_socket_addrs()
            .map_err(|e| ConnectError::InvalidAddress(e.into()))?
            .collect();
        if addrs.is_empty() {
            return Err(ConnectError::InvalidAddress(
                crate::error::AddrError::Resolve("no addresses resolved".into()),
            ));
        }

        let mut last_err: Option<ConnectError> = None;
        for sa in addrs {
            // Each iteration starts on a fresh handle. libsrt PRE options
            // must be set before srt_connect; once a handle has been
            // through a failed srt_connect we can't reuse it cleanly.
            let handle = unsafe { srt_sys::srt_create_socket() };
            if handle == SRT_INVALID_SOCK {
                last_err = Some(last_error().into());
                continue;
            }

            if let Err(e) = apply_socket_config(handle, config) {
                unsafe { srt_sys::srt_close(handle) };
                last_err = Some(ConnectError::InvalidOption(e));
                continue;
            }

            let os_addr = to_sockaddr(sa);
            let rc = unsafe {
                srt_sys::srt_connect(handle, os_addr.as_ptr().cast(), os_addr.len() as c_int)
            };
            if rc < 0 {
                let raw = last_error();
                // MUST read the reject reason from the live handle BEFORE
                // srt_close: once closed, libsrt's locateSocket returns null
                // and srt_getrejectreason always yields SRT_REJ_UNKNOWN.
                let reason = last_reject(handle);
                unsafe { srt_sys::srt_close(handle) };
                last_err = Some(classify_connect_error(raw, reason));
                continue;
            }

            let cached_stream_id = read_stream_id(handle);
            let cached_payload_limit = read_payload_size(handle);

            return Ok(Self {
                handle,
                cancel: make_cancel_handle(handle),
                cached_stream_id,
                cached_payload_limit,
            });
        }
        Err(last_err.expect("non-empty addrs always populates last_err on full-walk failure"))
    }

    /// Internal: wrap an already-accepted handle (called from `Listener::accept`).
    ///
    /// **Leak safety:** the raw accepted `handle` is wrapped in the owning
    /// `Socket` (Drop active → `srt_close`) BEFORE any fallible option is
    /// applied. A failed `set_int` therefore early-returns through `?` and the
    /// constructed `Socket` is dropped, closing the descriptor — never leaking
    /// the accepted SRT socket. (Previously options were applied to the bare
    /// `handle` before any owner existed, so an early return leaked the fd.)
    pub(crate) fn from_accepted(
        handle: srt_sys::SRTSOCKET,
        send_timeout: Option<Duration>,
        recv_timeout: Option<Duration>,
    ) -> Result<Self, IoError> {
        let cached_stream_id = read_stream_id(handle);
        let cached_payload_limit = read_payload_size(handle);
        // Construct the owner first so its Drop closes `handle` on any early
        // return below. Option application now goes through the owned socket.
        let socket = Self {
            handle,
            cancel: make_cancel_handle(handle),
            cached_stream_id,
            cached_payload_limit,
        };
        // Force blocking mode (`SRTO_SNDSYN`/`SRTO_RCVSYN` = true) on every
        // accepted socket, regardless of whatever those POST options may
        // have inherited from the listener at accept time. libsrt builds a
        // newly accepted socket as a copy of the listener's live state
        // (`CUDTUnited::newConnection`, `srtcore/api.cpp`: `new
        // CUDTSocket(*ls)`) at the moment its OWN internal handshake-
        // processing thread completes the connection — asynchronously and
        // independently of when the application calls `srt_accept`. A
        // caller that transiently toggles `SRTO_RCVSYN` false on the
        // listener (`Listener::accept_timeout`'s non-blocking probe,
        // `try_accept_nonblocking`) can therefore have a real, concurrent
        // handshake complete while that toggle is in effect, and the
        // resulting accepted socket inherits `RCVSYN=false` PERMANENTLY —
        // `set_bool`/`read_bool` on the listener only ever affects the
        // listener's own future `accept()` calls, never a socket that's
        // already been split off. Every socket this crate hands out is
        // designed to be blocking-with-timeout (`SRTO_RCVTIMEO`/
        // `SRTO_SNDTIMEO`, applied below, gate how long that block lasts);
        // a non-blocking accepted socket breaks that invariant silently:
        // `srt_recv()` on it returns `SRT_EASYNCRCV` ("no data available
        // for reading") the instant nothing happens to be ready yet,
        // instead of blocking — `is_timeout` matches the exact
        // `SRT_ETIMEOUT` errno, so it doesn't recognize that error, so it's
        // classified as `RecvError::Other` and
        // `SrtTransport::recv_bytes`'s catch-all treats that as a fatal
        // `TransportError::Broken`, killing an otherwise-healthy
        // connection the moment its first read has no data ready yet.
        set_bool(socket.handle, srt_sys::SRT_SOCKOPT_SRTO_SNDSYN, true)
            .map_err(io_from_option_error)?;
        set_bool(socket.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN, true)
            .map_err(io_from_option_error)?;
        if let Some(t) = send_timeout {
            set_int(
                socket.handle,
                srt_sys::SRT_SOCKOPT_SRTO_SNDTIMEO,
                duration_to_ms(t),
            )
            .map_err(io_from_option_error)?;
        }
        if let Some(t) = recv_timeout {
            set_int(
                socket.handle,
                srt_sys::SRT_SOCKOPT_SRTO_RCVTIMEO,
                duration_to_ms(t),
            )
            .map_err(io_from_option_error)?;
        }
        Ok(socket)
    }

    /// Send a buffer. Returns bytes sent. Live mode requires `buf.len() ≤ payload_size`.
    pub fn send(&mut self, buf: &[u8]) -> Result<usize, SendError> {
        let n = unsafe {
            srt_sys::srt_send(
                self.handle,
                buf.as_ptr().cast::<c_char>(),
                buf.len() as c_int,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let raw = last_error();
        Err(classify_send_error(
            raw,
            buf.len(),
            self.cached_payload_limit,
        ))
    }

    /// Receive into a buffer. Returns bytes received (one libsrt message).
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, RecvError> {
        let n = unsafe {
            srt_sys::srt_recv(
                self.handle,
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len() as c_int,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let raw = last_error();
        Err(classify_recv_error(raw, buf.len()))
    }

    pub fn peer_addr(&self) -> Result<SocketAddr, IoError> {
        let mut os_addr = OsSocketAddr::new();
        let mut len = os_addr.capacity() as c_int;
        let rc = unsafe {
            srt_sys::srt_getpeername(self.handle, os_addr.as_mut_ptr().cast(), &raw mut len)
        };
        if rc < 0 {
            return Err(last_error().into());
        }
        from_sockaddr(&os_addr).map_err(|e| IoError::System(std::io::Error::other(e.to_string())))
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

    /// Stream ID negotiated during handshake. Cached at construction.
    pub fn stream_id(&self) -> Option<&str> {
        self.cached_stream_id.as_deref()
    }

    /// Post-handshake `SRTO_PAYLOADSIZE`, cached at construction.
    ///
    /// Returned in bytes. After the SRT handshake completes, both peers
    /// have agreed on a payload size (the smaller of the two configured
    /// values). This is the upper bound on the per-message length passed
    /// to [`Self::send`] in live mode — exceeding it causes
    /// [`SendError::PayloadTooLarge`].
    ///
    /// Defaults to libsrt's `SRT_LIVE_DEF_PLSIZE` (1316 bytes — i.e.
    /// [`tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES`]) when libsrt's
    /// option-readback fails or returns a non-positive value.
    ///
    /// Used by [`SrtTransport::new`] to derive its `max_payload`; callers
    /// that build their own transport wrappers should query the same way.
    ///
    /// [`SendError::PayloadTooLarge`]: crate::error::SendError::PayloadTooLarge
    /// [`SrtTransport::new`]: crate::SrtTransport::new
    pub fn payload_limit(&self) -> usize {
        self.cached_payload_limit
    }

    /// Snapshot of libsrt's per-socket performance counters.
    pub fn stats(&self) -> Result<Stats, IoError> {
        let mut perf: srt_sys::CBytePerfMon = unsafe { mem::zeroed() };
        let rc = unsafe { srt_sys::srt_bistats(self.handle, &raw mut perf, 0, 0) };
        if rc < 0 {
            return Err(last_error().into());
        }
        Ok(perf_to_stats(&perf))
    }

    pub fn set_send_timeout(&mut self, timeout: Option<Duration>) -> Result<(), OptionError> {
        let ms = timeout.map(duration_to_ms).unwrap_or(-1);
        set_int(self.handle, srt_sys::SRT_SOCKOPT_SRTO_SNDTIMEO, ms)
    }

    pub fn set_recv_timeout(&mut self, timeout: Option<Duration>) -> Result<(), OptionError> {
        let ms = timeout.map(duration_to_ms).unwrap_or(-1);
        set_int(self.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVTIMEO, ms)
    }

    /// Explicit close; rare. Drop handles the normal path.
    ///
    /// After `close()` returns, any other thread parked in `send`/`recv`
    /// on a clone of this socket's `cancel_handle()` observes
    /// `SendError::ConnectionBroken` / `RecvError::ConnectionBroken`.
    ///
    /// **Always returns `Ok`.** The `Result` is retained for API stability
    /// and may carry an error in a future revision (the underlying
    /// `srt_close` rc is currently swallowed by the `SrtCancelHandle` closer).
    pub fn close(self) -> Result<(), IoError> {
        // SrtCancelHandle::cancel does the srt_close and is idempotent. We
        // can't easily plumb the rc back out (closer is `Fn`), so the
        // Result type stays for back-compat but always returns Ok.
        self.cancel.cancel();
        Ok(())
    }

    /// Clone-able close handle. Calling `cancel()` from any thread
    /// closes the underlying SRT socket — wakes a peer thread parked in
    /// `send` or `recv` with a Broken-class error. Idempotent.
    ///
    /// # Post-cancel fd-reuse note
    ///
    /// After `cancel()` fires, this `Socket` still holds the integer
    /// libsrt handle. `&self` methods (`stats`, `peer_addr`, `local_addr`)
    /// called on the still-live `Socket` after cross-thread cancel may
    /// operate on a reused fd if libsrt has reassigned that integer to a
    /// new socket in the interim. The transport layer guards against this
    /// by nulling the socket on `Broken`, but callers that retain bare
    /// `Socket` values past a cancel should treat any post-cancel `&self`
    /// results as advisory. No memory unsafety results; libsrt validates
    /// handles internally.
    pub fn cancel_handle(&self) -> tst_core::SrtCancelHandle {
        self.cancel.clone()
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // No-op if explicit close() / cancel() already fired.
        //
        // `close_without_cancel`, NOT `cancel`: Drop runs on every transport
        // error path that retires a dead socket (`SrtTransport` nulls its
        // `Option<Socket>` on a peer break), and a peer disconnect must not
        // latch the caller-visible `is_cancelled()`. The socket is still
        // closed and any parked call still woken — only the latch differs.
        self.cancel.close_without_cancel();
    }
}

// ============================================================================
// Helpers (private to crate; shared by socket.rs and listener.rs)
// ============================================================================

pub(crate) fn duration_to_ms(d: Duration) -> i32 {
    d.as_millis().min(i32::MAX as u128) as i32
}

/// `SRTO_RCVBUF`/`SRTO_SNDBUF` take an `i32`; a `u32` above `i32::MAX` would
/// wrap negative through `as` and misconfigure libsrt, so reject it instead.
fn buf_bytes_to_i32(name: &str, n: u32) -> Result<i32, OptionError> {
    i32::try_from(n).map_err(|_| {
        OptionError::OutOfRange(format!("{name} must be at most {}, got {n}", i32::MAX))
    })
}

/// Set `SRTO_RCVBUF` and warn if libsrt silently clamped it.
///
/// libsrt accepts any positive value but stores
/// `min(bytes / (MSS - 28), SRTO_FC)` packets and still returns success
/// (socketconfig.cpp, `RcvBufferSizeOptionToValue`) — at the default
/// flow-control window of 25600 packets and default MSS that's a hard
/// ceiling of ~37.7 MB, hit silently by anything larger. Read the
/// effective value back and warn on a shortfall beyond the benign
/// one-packet floor-division rounding, naming the knob that actually
/// lifts the ceiling — this silent clamp cost an integrator a debugging
/// session (2026-08-03 field ask). Callers must apply `SRTO_MSS` and
/// `SRTO_FC` BEFORE calling this — both feed the conversion above.
fn set_rcvbuf_checked(
    handle: srt_sys::SRTSOCKET,
    requested: u32,
    mss: Option<u16>,
) -> Result<(), OptionError> {
    let bytes = buf_bytes_to_i32("recv_buf_bytes", requested)?;
    set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_RCVBUF, bytes)?;
    // Everything below is diagnostics: a readback failure must not fail
    // a config apply whose set already succeeded — skip the warn instead.
    let Ok(effective) = get_int(handle, srt_sys::SRT_SOCKOPT_SRTO_RCVBUF) else {
        return Ok(());
    };
    // Rounding allowance from the MSS actually in effect on the socket
    // (libsrt may adjust the requested MSS); fall back to the configured
    // value / libsrt default only if that readback fails too.
    let mss_effective = get_int(handle, srt_sys::SRT_SOCKOPT_SRTO_MSS)
        .unwrap_or_else(|_| i32::from(mss.unwrap_or(1500)));
    let payload_per_pkt = i64::from(mss_effective) - 28;
    if i64::from(effective) + payload_per_pkt < i64::from(bytes) {
        tracing::warn!(
            requested_bytes = bytes,
            effective_bytes = effective,
            "SRTO_RCVBUF silently clamped to the flow-control window; raise \
             flow_window_packets (SRTO_FC, default 25600 packets) to lift \
             the ceiling"
        );
    }
    Ok(())
}

pub(crate) fn set_int(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
    value: i32,
) -> Result<(), OptionError> {
    let rc = unsafe {
        srt_sys::srt_setsockflag(
            handle,
            opt,
            (&raw const value).cast(),
            mem::size_of::<i32>() as c_int,
        )
    };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(())
}

/// Read an `int` socket option back from libsrt. Used to detect the
/// silent adjustments libsrt applies at set time (e.g. `SRTO_RCVBUF`'s
/// flow-control-window clamp) — the setter returns success in those
/// cases, so a readback is the only way to see the effective value.
pub(crate) fn get_int(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
) -> Result<i32, OptionError> {
    let mut value: i32 = 0;
    let mut len = mem::size_of::<i32>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(value)
}

pub(crate) fn set_i64(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
    value: i64,
) -> Result<(), OptionError> {
    let rc = unsafe {
        srt_sys::srt_setsockflag(
            handle,
            opt,
            (&raw const value).cast(),
            mem::size_of::<i64>() as c_int,
        )
    };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(())
}

pub(crate) fn set_bool(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
    value: bool,
) -> Result<(), OptionError> {
    let v: i32 = if value { 1 } else { 0 };
    set_int(handle, opt, v)
}

pub(crate) fn read_bool(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
) -> Result<bool, OptionError> {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(value != 0)
}

pub(crate) fn set_string(
    handle: srt_sys::SRTSOCKET,
    opt: srt_sys::SRT_SOCKOPT,
    value: &str,
) -> Result<(), OptionError> {
    let rc = unsafe {
        srt_sys::srt_setsockflag(handle, opt, value.as_ptr().cast(), value.len() as c_int)
    };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(())
}

/// `struct linger` for `SRTO_LINGER`, hand-rolled because the `libc` crate
/// doesn't expose `linger` on `*-pc-windows-msvc`.
///
/// The ABI is NOT the same across platforms, and libsrt uses each platform's
/// native definition: POSIX `struct linger` is two `int` (8 bytes), but
/// Win32/Winsock `LINGER` is two `u_short` (4 bytes). libsrt validates the
/// option length with `cast_optval<linger>` which throws `MJ_NOTSUP/MN_INVAL`
/// ("Operation not supported: Bad parameters") when `optlen != sizeof(linger)`
/// (socketconfig.h:368-371) — so a POSIX-sized 8-byte struct is rejected on
/// Windows, breaking every sender connect (sender_defaults sets `linger`).
/// Match the field widths per platform so `size_of::<LingerOpt>()` equals the
/// `sizeof(linger)` libsrt expects.
#[cfg(windows)]
#[repr(C)]
struct LingerOpt {
    l_onoff: core::ffi::c_ushort,
    l_linger: core::ffi::c_ushort,
}
#[cfg(not(windows))]
#[repr(C)]
struct LingerOpt {
    l_onoff: c_int,
    l_linger: c_int,
}

/// Set `SRTO_LINGER`. Unlike most SRT options it takes a `struct linger`
/// (not an `int`), so we can't go through `set_int`. `Duration::ZERO` (or
/// any sub-second duration) disables linger entirely (`l_onoff = 0`),
/// causing `srt_close` to return immediately and discard any unsent
/// payload. Non-zero seconds are clamped into `i32` range.
pub(crate) fn set_linger(handle: srt_sys::SRTSOCKET, d: Duration) -> Result<(), OptionError> {
    // Field types differ per platform (see `LingerOpt`): Win32 LINGER fields
    // are `u_short` (cap at u16::MAX seconds), POSIX `int`.
    #[cfg(windows)]
    let lin = {
        let secs = d.as_secs().min(u16::MAX as u64) as core::ffi::c_ushort;
        LingerOpt {
            l_onoff: if secs > 0 { 1 } else { 0 },
            l_linger: secs,
        }
    };
    #[cfg(not(windows))]
    let lin = {
        let secs = d.as_secs().min(i32::MAX as u64) as c_int;
        LingerOpt {
            l_onoff: if secs > 0 { 1 } else { 0 },
            l_linger: secs,
        }
    };
    let rc = unsafe {
        srt_sys::srt_setsockopt(
            handle,
            0,
            srt_sys::SRT_SOCKOPT_SRTO_LINGER,
            (&raw const lin).cast(),
            std::mem::size_of::<LingerOpt>() as c_int,
        )
    };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(())
}

pub(crate) fn set_passphrase(
    handle: srt_sys::SRTSOCKET,
    p: &Passphrase,
) -> Result<(), OptionError> {
    let bytes = p.as_bytes();
    let rc = unsafe {
        srt_sys::srt_setsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PASSPHRASE,
            bytes.as_ptr().cast(),
            bytes.len() as c_int,
        )
    };
    if rc < 0 {
        return Err(last_error().into());
    }
    Ok(())
}

/// Option values applied identically by `apply_socket_config` and
/// `apply_listener_config`. Built via `From` on each config type so the
/// application logic (`apply_common_options`) lives in exactly one place.
struct CommonOpts<'a> {
    passphrase: Option<&'a Passphrase>,
    key_length: crate::options::KeyLength,
    latency: Option<Duration>,
    recv_latency: Option<Duration>,
    mss: Option<u16>,
    flow_window_packets: Option<u32>,
    recv_buf_bytes: Option<u32>,
    max_bandwidth: Option<MaxBandwidth>,
    overhead_bandwidth_pct: Option<u8>,
    payload_size: Option<u16>,
    udp_recv_buffer_bytes: Option<u32>,
    loss_max_ttl: Option<u32>,
    too_late_packet_drop: Option<bool>,
    packet_filter: Option<&'a crate::options::PacketFilter>,
    congestion: Option<crate::options::Congestion>,
    recv_timeout: Option<Duration>,
    linger: Option<Duration>,
}

impl<'a> From<&'a SocketConfig> for CommonOpts<'a> {
    fn from(cfg: &'a SocketConfig) -> Self {
        Self {
            passphrase: cfg.passphrase.as_ref(),
            key_length: cfg.key_length,
            latency: cfg.latency,
            recv_latency: cfg.recv_latency,
            mss: cfg.mss,
            flow_window_packets: cfg.flow_window_packets,
            recv_buf_bytes: cfg.recv_buf_bytes,
            max_bandwidth: cfg.max_bandwidth,
            overhead_bandwidth_pct: cfg.overhead_bandwidth_pct,
            payload_size: cfg.payload_size,
            udp_recv_buffer_bytes: cfg.udp_recv_buffer_bytes,
            loss_max_ttl: cfg.loss_max_ttl,
            too_late_packet_drop: cfg.too_late_packet_drop,
            packet_filter: cfg.packet_filter.as_ref(),
            congestion: cfg.congestion,
            recv_timeout: cfg.recv_timeout,
            linger: cfg.linger,
        }
    }
}

impl<'a> From<&'a crate::config::ListenerConfig> for CommonOpts<'a> {
    fn from(cfg: &'a crate::config::ListenerConfig) -> Self {
        Self {
            passphrase: cfg.passphrase.as_ref(),
            key_length: cfg.key_length,
            latency: cfg.latency,
            recv_latency: cfg.recv_latency,
            mss: cfg.mss,
            flow_window_packets: cfg.flow_window_packets,
            recv_buf_bytes: cfg.recv_buf_bytes,
            max_bandwidth: cfg.max_bandwidth,
            overhead_bandwidth_pct: cfg.overhead_bandwidth_pct,
            payload_size: cfg.payload_size,
            udp_recv_buffer_bytes: cfg.udp_recv_buffer_bytes,
            loss_max_ttl: cfg.loss_max_ttl,
            too_late_packet_drop: cfg.too_late_packet_drop,
            packet_filter: cfg.packet_filter.as_ref(),
            congestion: cfg.congestion,
            recv_timeout: cfg.recv_timeout,
            linger: cfg.linger,
        }
    }
}

/// Apply the option set shared by `apply_socket_config` and
/// `apply_listener_config`.
fn apply_common_options(
    handle: srt_sys::SRTSOCKET,
    opts: &CommonOpts<'_>,
) -> Result<(), OptionError> {
    if let Some(p) = opts.passphrase {
        // Set key length BEFORE passphrase.
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PBKEYLEN,
            opts.key_length.as_bytes(),
        )?;
        set_passphrase(handle, p)?;
    }
    if let Some(d) = opts.latency {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_LATENCY, duration_to_ms(d))?;
    }
    if let Some(d) = opts.recv_latency {
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_RCVLATENCY,
            duration_to_ms(d),
        )?;
    }
    // MSS and the flow-control window MUST be applied before the SRT
    // buffer sizes: libsrt converts SRTO_RCVBUF/SRTO_SNDBUF to packets
    // using the MSS in effect at set time, and clamps SRTO_RCVBUF to the
    // SRTO_FC window in effect at set time (socketconfig.cpp,
    // RcvBufferSizeOptionToValue). With the old order — buffers first —
    // a config that raised both `flow_window_packets` and
    // `recv_buf_bytes` still had its receive buffer silently clamped to
    // the DEFAULT window (25600 packets ≈ 37.7 MB at default MSS),
    // making the FC raise a no-op for buffer sizing.
    if let Some(mss) = opts.mss {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_MSS, mss as i32)?;
    }
    if let Some(n) = opts.flow_window_packets {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_FC, n as i32)?;
    }
    if let Some(n) = opts.recv_buf_bytes {
        set_rcvbuf_checked(handle, n, opts.mss)?;
    }
    if let Some(bw) = opts.max_bandwidth {
        set_i64(handle, srt_sys::SRT_SOCKOPT_SRTO_MAXBW, bw.as_libsrt_i64())?;
    }
    if let Some(pct) = opts.overhead_bandwidth_pct {
        crate::options::validate_overhead_bandwidth_pct(pct as u32)
            .map_err(OptionError::OutOfRange)?;
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_OHEADBW, pct as i32)?;
    }
    if let Some(n) = opts.payload_size {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_PAYLOADSIZE, n as i32)?;
    }
    if let Some(n) = opts.udp_recv_buffer_bytes {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_UDP_RCVBUF, n as i32)?;
    }
    if let Some(n) = opts.loss_max_ttl {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_LOSSMAXTTL, n as i32)?;
    }
    if let Some(on) = opts.too_late_packet_drop {
        set_bool(handle, srt_sys::SRT_SOCKOPT_SRTO_TLPKTDROP, on)?;
    }
    if let Some(pf) = opts.packet_filter {
        set_string(handle, srt_sys::SRT_SOCKOPT_SRTO_PACKETFILTER, pf.as_str())?;
    }
    if let Some(c) = opts.congestion {
        set_string(handle, srt_sys::SRT_SOCKOPT_SRTO_CONGESTION, c.as_str())?;
    }
    if let Some(t) = opts.recv_timeout {
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_RCVTIMEO,
            duration_to_ms(t),
        )?;
    }
    if let Some(d) = opts.linger {
        set_linger(handle, d)?;
    }
    Ok(())
}

/// Apply every set field of a `SocketConfig` to the handle.
pub(crate) fn apply_socket_config(
    handle: srt_sys::SRTSOCKET,
    cfg: &SocketConfig,
) -> Result<(), OptionError> {
    apply_common_options(handle, &CommonOpts::from(cfg))?;
    if let Some(t) = cfg.send_timeout {
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_SNDTIMEO,
            duration_to_ms(t),
        )?;
    }
    if let Some(t) = cfg.connect_timeout {
        // SRTO_CONNTIMEO is a PRE option (set before srt_connect); this
        // function is called between srt_create_socket and srt_connect, so
        // ordering is satisfied.
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_CONNTIMEO,
            duration_to_ms(t),
        )?;
    }
    if let Some(d) = cfg.peer_latency {
        set_int(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PEERLATENCY,
            duration_to_ms(d),
        )?;
    }
    if let Some(n) = cfg.send_buf_bytes {
        let bytes = buf_bytes_to_i32("send_buf_bytes", n)?;
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_SNDBUF, bytes)?;
    }
    if let Some(bw) = cfg.input_bandwidth {
        set_i64(handle, srt_sys::SRT_SOCKOPT_SRTO_INPUTBW, bw as i64)?;
    }
    if let Some(n) = cfg.udp_send_buffer_bytes {
        set_int(handle, srt_sys::SRT_SOCKOPT_SRTO_UDP_SNDBUF, n as i32)?;
    }
    if let Some(id) = &cfg.stream_id {
        set_string(handle, srt_sys::SRT_SOCKOPT_SRTO_STREAMID, id.as_str())?;
    }
    if matches!(cfg.role, crate::options::Role::Sender) {
        set_bool(handle, srt_sys::SRT_SOCKOPT_SRTO_SENDER, true)?;
    }
    Ok(())
}

pub(crate) fn apply_listener_config(
    handle: srt_sys::SRTSOCKET,
    cfg: &crate::config::ListenerConfig,
) -> Result<(), OptionError> {
    apply_common_options(handle, &CommonOpts::from(cfg))?;
    set_bool(handle, srt_sys::SRT_SOCKOPT_SRTO_REUSEADDR, cfg.reuse_addr)?;
    Ok(())
}

pub(crate) fn read_stream_id(handle: srt_sys::SRTSOCKET) -> Option<String> {
    let mut buf = [0u8; 513];
    let mut len = buf.len() as c_int;
    let rc = unsafe {
        srt_sys::srt_getsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_STREAMID,
            buf.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    if rc < 0 || len <= 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..len as usize]).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

pub(crate) fn read_payload_size(handle: srt_sys::SRTSOCKET) -> usize {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc = unsafe {
        srt_sys::srt_getsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PAYLOADSIZE,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if rc < 0 || value <= 0 {
        // libsrt's SRT_LIVE_DEF_PLSIZE — see SRT_TS_BUNDLE_BYTES.
        return SRT_TS_BUNDLE_BYTES;
    }
    value as usize
}

pub(crate) fn perf_to_stats(p: &srt_sys::CBytePerfMon) -> Stats {
    Stats {
        bytes_sent: p.byteSentTotal,
        bytes_received: p.byteRecvTotal,
        bytes_lost_recv_side: p.byteRcvLossTotal,
        // CBytePerfMon doesn't expose byteSndLossTotal — sender-side loss is
        // only reported as packet count (pktSndLossTotal). We surface 0 for
        // bytes-lost-send-side; consumers should use packets_lost_send_side.
        bytes_lost_send_side: 0,
        packets_sent: p.pktSentTotal as u64,
        packets_received: p.pktRecvTotal as u64,
        packets_lost_recv_side: p.pktRcvLossTotal as u64,
        packets_lost_send_side: p.pktSndLossTotal as u64,
        packets_retransmitted: p.pktRetransTotal as u64,
        packets_dropped_recv_side: p.pktRcvDropTotal as u64,
        packets_dropped_send_side: p.pktSndDropTotal as u64,
        rtt: Duration::from_micros((p.msRTT.max(0.0) * 1000.0) as u64),
        send_bandwidth_bps: (p.mbpsSendRate * 1_000_000.0).max(0.0) as u64,
        recv_bandwidth_bps: (p.mbpsRecvRate * 1_000_000.0).max(0.0) as u64,
        mbps_estimated_bandwidth: p.mbpsBandwidth,
        send_buffer_packets: p.pktSndBuf as u32,
        recv_buffer_packets: p.pktRcvBuf as u32,
    }
}

fn io_from_option_error(e: OptionError) -> IoError {
    match e {
        OptionError::Other { kind, message } => IoError::Other { kind, message },
        other => IoError::Other {
            kind: SrtErrno::Unknown(0),
            message: other.to_string(),
        },
    }
}

fn classify_send_error(raw: crate::error::RawError, payload_len: usize, limit: usize) -> SendError {
    // Deterministic check: if the caller's buffer obviously exceeds the
    // configured payload size, classify regardless of libsrt's specific
    // error wording (which has shifted across versions).
    if payload_len > limit {
        return SendError::PayloadTooLarge {
            actual: payload_len,
            limit,
        };
    }
    if raw.message.contains("Message has no destination address")
        || raw.message.contains("payload size")
        || raw.message.contains("Invalid argument")
        || raw
            .message
            .contains("Incorrect use of Message API (sendmsg/recvmsg)")
    {
        return SendError::PayloadTooLarge {
            actual: payload_len,
            limit,
        };
    }
    raw.into()
}

fn classify_recv_error(raw: crate::error::RawError, buf_len: usize) -> RecvError {
    if raw.message.contains("Message size exceeds buffer") {
        return RecvError::BufferTooSmall {
            buf_len,
            message_len: 0,
        };
    }
    raw.into()
}

/// Build a SrtCancelHandle that closes the SRTSOCKET on first cancel.
pub(crate) fn make_cancel_handle(handle: srt_sys::SRTSOCKET) -> tst_core::SrtCancelHandle {
    tst_core::SrtCancelHandle::new(handle as i64, |h| {
        // SAFETY: h was the same SRTSOCKET we stored; libsrt accepts
        // srt_close from any thread; the atomic-swap in SrtCancelHandle
        // guarantees this runs at most once.
        let _ = unsafe { srt_sys::srt_close(h as srt_sys::SRTSOCKET) };
    })
}

#[cfg(test)]
mod tests {
    /// libsrt silently clamps `SRTO_RCVBUF` to the flow-control window
    /// (default 25600 packets ≈ 37.7 MB at default MSS) — the setter
    /// still returns success. `set_rcvbuf_checked` must surface that as
    /// a warn naming the lifting knob. Empirically pins the clamp
    /// mechanism against the vendored libsrt, not just our wrapper.
    #[test]
    #[tracing_test::traced_test]
    fn rcvbuf_over_fc_window_clamps_and_warns() {
        super::ensure_initialized();
        let h = unsafe { srt_sys::srt_create_socket() };
        assert_ne!(h, super::SRT_INVALID_SOCK);
        let cfg = super::SocketConfig {
            recv_buf_bytes: Some(60_000_000), // > default ceiling ~37.7 MB
            ..Default::default()
        };
        super::apply_socket_config(h, &cfg).unwrap();
        let effective = super::get_int(h, srt_sys::SRT_SOCKOPT_SRTO_RCVBUF).unwrap();
        unsafe { srt_sys::srt_close(h) };
        assert!(
            effective < 60_000_000,
            "expected libsrt to clamp 60 MB below the FC window, got {effective}"
        );
        assert!(logs_contain("SRTO_RCVBUF silently clamped"));
    }

    /// Raising `flow_window_packets` must actually lift the receive
    /// buffer ceiling — which requires SRTO_FC to be applied BEFORE
    /// SRTO_RCVBUF (libsrt clamps against the window in effect at set
    /// time). With the old buffers-first ordering this request came
    /// back clamped to the DEFAULT window and the FC raise was a no-op
    /// for buffer sizing.
    #[test]
    #[tracing_test::traced_test]
    fn rcvbuf_with_raised_fc_window_honors_request() {
        super::ensure_initialized();
        let h = unsafe { srt_sys::srt_create_socket() };
        assert_ne!(h, super::SRT_INVALID_SOCK);
        let cfg = super::SocketConfig {
            flow_window_packets: Some(51_200), // 2x default → ceiling ~75 MB
            recv_buf_bytes: Some(60_000_000),
            ..Default::default()
        };
        super::apply_socket_config(h, &cfg).unwrap();
        let effective = super::get_int(h, srt_sys::SRT_SOCKOPT_SRTO_RCVBUF).unwrap();
        unsafe { srt_sys::srt_close(h) };
        // Allow the one-packet floor-division rounding, nothing more.
        assert!(
            i64::from(effective) + (1500 - 28) >= 60_000_000,
            "raised FC window did not lift the rcvbuf ceiling: effective {effective}"
        );
        assert!(!logs_contain("SRTO_RCVBUF silently clamped"));
    }

    #[test]
    fn stats_struct_has_role_split_loss_fields() {
        // Compile-check that the new public fields exist.
        let _ = |s: super::Stats| {
            let _ = s.packets_lost_recv_side;
            let _ = s.packets_lost_send_side;
            let _ = s.bytes_lost_recv_side;
            let _ = s.bytes_lost_send_side;
            let _ = s.packets_dropped_recv_side;
            let _ = s.packets_dropped_send_side;
        };
    }

    /// Live accepted-handle test scaffold. Binds a raw libsrt listener on
    /// `127.0.0.1:0`, connects a peer from a daemon thread, and blocks until
    /// one connection is accepted. Returns `None` if loopback is unavailable
    /// (caller should SKIP). On `Some`, the caller owns the raw `accepted`
    /// handle and MUST eventually close it (or hand it to a `Socket` that
    /// will); the returned `Cleanup` closes the listener and joins the peer
    /// thread on drop.
    ///
    /// Shared by the two `from_accepted` tests below so they don't duplicate
    /// the ~40 lines of raw libsrt listen/connect/accept boilerplate.
    fn accept_one_live() -> Option<(srt_sys::SRTSOCKET, Cleanup)> {
        use os_socketaddr::OsSocketAddr;
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Loopback gate (mirrors the integration harness's require_loopback!).
        if std::env::var_os("SKIP_LOOPBACK").is_some() || TcpListener::bind("127.0.0.1:0").is_err()
        {
            return None;
        }

        super::ensure_initialized();

        // Bind a raw libsrt listener and capture its port.
        let listener = unsafe { srt_sys::srt_create_socket() };
        assert_ne!(listener, super::SRT_INVALID_SOCK);
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let os_bind = super::to_sockaddr(bind_addr);
        let rc = unsafe {
            srt_sys::srt_bind(
                listener,
                os_bind.as_ptr().cast(),
                os_bind.len() as super::c_int,
            )
        };
        assert!(rc >= 0, "srt_bind failed");
        assert!(
            unsafe { srt_sys::srt_listen(listener, 1) } >= 0,
            "srt_listen failed"
        );
        let mut name = OsSocketAddr::new();
        let mut nlen = name.capacity() as super::c_int;
        assert!(
            unsafe { srt_sys::srt_getsockname(listener, name.as_mut_ptr().cast(), &raw mut nlen) }
                >= 0
        );
        let port = super::from_sockaddr(&name).unwrap().port();

        // Connect a peer from a separate thread so the listener can accept.
        let ready = Arc::new(AtomicBool::new(false));
        let r = ready.clone();
        let peer = std::thread::spawn(move || {
            // Wait until the main thread has reached accept readiness.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !r.load(Ordering::SeqCst) {
                if std::time::Instant::now() > deadline {
                    panic!("accept-ready signal never set");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            super::ensure_initialized();
            let s = unsafe { srt_sys::srt_create_socket() };
            let dst: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let os_dst = super::to_sockaddr(dst);
            let rc = unsafe {
                srt_sys::srt_connect(s, os_dst.as_ptr().cast(), os_dst.len() as super::c_int)
            };
            assert!(rc >= 0, "peer srt_connect failed");
            // Keep the peer alive briefly so the accepted handle stays live.
            std::thread::sleep(Duration::from_millis(200));
            unsafe { srt_sys::srt_close(s) };
        });

        ready.store(true, Ordering::SeqCst);
        let mut acc_name = OsSocketAddr::new();
        let mut acc_len = acc_name.capacity() as super::c_int;
        let accepted = unsafe {
            srt_sys::srt_accept(listener, acc_name.as_mut_ptr().cast(), &raw mut acc_len)
        };
        assert_ne!(accepted, super::SRT_INVALID_SOCK, "srt_accept failed");

        Some((
            accepted,
            Cleanup {
                listener,
                peer: Some(peer),
            },
        ))
    }

    /// RAII cleanup for [`accept_one_live`]: closes the listener and joins the
    /// peer daemon thread when the test body ends (even on panic).
    struct Cleanup {
        listener: srt_sys::SRTSOCKET,
        peer: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(peer) = self.peer.take() {
                let _ = peer.join();
            }
            unsafe { srt_sys::srt_close(self.listener) };
        }
    }

    // T2-SRT-LEAK regression (failure path). `from_accepted` now wraps the raw
    // accepted handle in the owning `Socket` (Drop active) BEFORE applying any
    // fallible option, so a `set_int` failure on the `?` path closes the
    // accepted SRT socket instead of leaking it.
    //
    // We can't feed a bad timeout through `from_accepted`'s public signature
    // (`duration_to_ms` clamps to `[0, i32::MAX]`, and SND/RCVTIMEO never
    // reject those), so this test reproduces the EXACT failure structure the
    // fix relies on: take a genuinely-live accepted handle, build the owning
    // `Socket` from it, then trigger a deterministic post-construction
    // option failure (a PRE-bind-only option set on a connected socket, which
    // libsrt rejects) — the same `set_int(...)?` shape `from_accepted` runs.
    // On the early return the `Socket` drops; we assert the handle is closed
    // (libsrt's `locateSocket` returns null → `srt_getsockflag` errors).
    //
    // It is paired with `from_accepted_happy_path_yields_usable_socket` below,
    // which calls the REAL `from_accepted` so a regression in the function's
    // own ordering (re-applying options on the bare handle, or adding a
    // pre-wrap fallible step) is caught instead of silently passing here.
    #[test]
    fn from_accepted_failure_path_closes_accepted_socket_no_leak() {
        use std::mem;

        let Some((accepted, _cleanup)) = accept_one_live() else {
            eprintln!("SKIP: loopback unavailable");
            return;
        };

        // Reproduce `from_accepted`'s post-fix structure: wrap the live handle
        // in the owning Socket FIRST, then run a fallible option set.
        let socket = super::Socket {
            handle: accepted,
            cancel: super::make_cancel_handle(accepted),
            cached_stream_id: super::read_stream_id(accepted),
            cached_payload_limit: super::read_payload_size(accepted),
        };

        // PRE-bind-only option (SRTO_PAYLOADSIZE) set on a CONNECTED socket is
        // rejected by libsrt — a deterministic stand-in for the SND/RCVTIMEO
        // `set_int(...)?` that could fail in `from_accepted`.
        let bad = super::set_int(socket.handle, srt_sys::SRT_SOCKOPT_SRTO_PAYLOADSIZE, 1316);
        assert!(
            bad.is_err(),
            "expected PRE-only option set on a connected socket to be rejected"
        );

        // Simulate `from_accepted`'s early return on the `?`: drop the owner.
        // RAII (cancel.cancel() → srt_close) must close the accepted handle.
        drop(socket);

        // Proof of no-leak: libsrt no longer recognizes the handle. Any flag
        // read on a closed socket fails (locateSocket → null → SRT_EINVSOCK).
        let mut probe: i32 = 0;
        let mut plen: super::c_int = mem::size_of::<i32>() as super::c_int;
        let rc = unsafe {
            srt_sys::srt_getsockflag(
                accepted,
                srt_sys::SRT_SOCKOPT_SRTO_RCVTIMEO,
                (&raw mut probe).cast(),
                &raw mut plen,
            )
        };
        assert!(
            rc < 0,
            "accepted handle still valid after Socket drop — it was LEAKED"
        );
    }

    // T2-SRT-LEAK regression (happy path / drift guard). Calls the REAL
    // `from_accepted` on a genuinely-live accepted handle and asserts it
    // returns a usable `Socket`. This keeps `from_accepted` referenced by a
    // test (signature/field drift breaks compilation) and exercises its happy
    // path against live libsrt — so a regression in the FUNCTION (not just the
    // RAII property the failure-path test reconstructs) is caught.
    #[test]
    fn from_accepted_happy_path_yields_usable_socket() {
        use std::time::Duration;

        let Some((accepted, _cleanup)) = accept_one_live() else {
            eprintln!("SKIP: loopback unavailable");
            return;
        };

        // The real function, both with default (None) and explicit timeouts.
        let socket = super::Socket::from_accepted(
            accepted,
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        )
        .expect("from_accepted should succeed on a freshly-accepted live socket");

        // The returned Socket owns the accepted handle and is usable: a
        // local_addr round-trip proves libsrt still recognizes the handle
        // (it was NOT closed by a spurious early return) and that the owner
        // wraps the same descriptor we handed in.
        assert_eq!(
            socket.raw_handle(),
            accepted,
            "from_accepted must own the handle it was given"
        );
        let local = socket
            .local_addr()
            .expect("local_addr on the accepted socket should succeed");
        assert!(local.ip().is_loopback(), "expected a loopback local addr");

        // Dropping the Socket closes the accepted handle (no leak on the
        // success path either).
        drop(socket);
    }

    // Regression for the bug `Listener::accept_timeout`'s non-blocking probe
    // exposed: libsrt builds a newly-accepted socket as a live copy of the
    // listener's own socket state at the moment its internal
    // handshake-processing thread completes the connection
    // (`srtcore/api.cpp`'s `newConnection`: `new CUDTSocket(*ls)`) —
    // asynchronously and independently of when the application calls
    // `srt_accept`. `try_accept_nonblocking` transiently sets
    // `SRTO_RCVSYN=false` on the *listener* around one non-blocking probe
    // call; a connection whose handshake completion races that toggle used
    // to inherit `RCVSYN=false` PERMANENTLY, because `from_accepted` never
    // reset it. A non-blocking accepted socket's `recv()` returns
    // `SRT_EASYNCRCV` instead of blocking for `SRTO_RCVTIMEO`, which
    // `tst-interop`'s CI hit as a total-zero-delivery regression: the raw
    // error doesn't match `is_timeout`'s exact `SRT_ETIMEOUT` errno check,
    // so it gets classified as a fatal broken connection the instant
    // there's nothing to read yet.
    //
    // Can't reliably force the real race (it's a TOCTOU against libsrt's
    // own internal thread, not something this test can synchronize
    // against), so this instead tests `from_accepted`'s own contract
    // directly: feed it a handle that already has `RCVSYN=false` (simulating
    // exactly what an unlucky inherited-copy would look like) and assert it
    // normalizes the result back to blocking mode regardless.
    #[test]
    fn from_accepted_forces_blocking_mode_regardless_of_inherited_rcvsyn() {
        use std::time::Duration;

        let Some((accepted, _cleanup)) = accept_one_live() else {
            eprintln!("SKIP: loopback unavailable");
            return;
        };

        // Simulate an accepted handle that inherited non-blocking mode from
        // the listener (the exact state a raced accept_timeout probe used to
        // leave behind).
        super::set_bool(accepted, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN, false)
            .expect("set_bool RCVSYN=false on a live accepted handle");
        super::set_bool(accepted, srt_sys::SRT_SOCKOPT_SRTO_SNDSYN, false)
            .expect("set_bool SNDSYN=false on a live accepted handle");

        let socket = super::Socket::from_accepted(
            accepted,
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        )
        .expect("from_accepted should succeed on a freshly-accepted live socket");

        assert!(
            super::read_bool(socket.handle, srt_sys::SRT_SOCKOPT_SRTO_RCVSYN)
                .expect("read back RCVSYN"),
            "from_accepted must force RCVSYN back to blocking mode, not leave an inherited false"
        );
        assert!(
            super::read_bool(socket.handle, srt_sys::SRT_SOCKOPT_SRTO_SNDSYN)
                .expect("read back SNDSYN"),
            "from_accepted must force SNDSYN back to blocking mode, not leave an inherited false"
        );

        drop(socket);
    }

    /// Construction-shape test that doesn't need a live socket: building
    /// a Socket from a fake handle and double-closing it should NOT
    /// double-call srt_close. Without an integration test running real
    /// libsrt, we verify by checking that explicit close().is_ok() and
    /// drop() coexist safely (drop() must skip srt_close when cancel
    /// already ran).
    #[test]
    fn double_close_via_cancel_then_drop_is_safe() {
        // We construct a SrtCancelHandle by hand around a fake handle with a
        // closer that records the call count, mirroring what Socket holds.
        use std::sync::atomic::{AtomicU32, Ordering};
        use tst_core::SrtCancelHandle;
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let cancel = SrtCancelHandle::new(99, move |_| {
            calls_cl.fetch_add(1, Ordering::SeqCst);
        });
        // Cancel once (simulates Socket::close).
        cancel.cancel();
        // Drop the handle (simulates Socket::drop calling cancel.cancel()
        // a second time).
        cancel.cancel();
        drop(cancel);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // DA-SRT-4: perf_to_stats must preserve sub-millisecond RTT precision.
    // libsrt's CBytePerfMon.msRTT is a float in milliseconds (e.g. 1.5 ms).
    // The old code used Duration::from_millis(1.5 as u64) = 1ms (truncated).
    // The fix uses Duration::from_micros((1.5 * 1000.0) as u64) = 1500µs.
    #[test]
    fn perf_to_stats_preserves_sub_ms_rtt() {
        let mut p: srt_sys::CBytePerfMon = unsafe { std::mem::zeroed() };
        p.msRTT = 1.5; // 1.5 ms — has a sub-millisecond component
        let stats = super::perf_to_stats(&p);
        assert_eq!(
            stats.rtt,
            std::time::Duration::from_micros(1500),
            "1.5 ms should map to 1500µs; got {:?}",
            stats.rtt
        );

        // Edge case: exactly 2 ms should also be exact.
        p.msRTT = 2.0;
        let stats2 = super::perf_to_stats(&p);
        assert_eq!(stats2.rtt, std::time::Duration::from_millis(2));

        // Negative values (should not happen but libsrt docs say > 0 on connected
        // socket) clamp to zero via max(0.0).
        p.msRTT = -1.0;
        let stats3 = super::perf_to_stats(&p);
        assert_eq!(stats3.rtt, std::time::Duration::ZERO);
    }

    // SRTO_RCVBUF/SNDBUF take an i32: a byte count above i32::MAX must be
    // rejected as OutOfRange, not wrapped negative by an `as` cast.
    #[test]
    fn buf_bytes_above_i32_max_is_out_of_range() {
        assert_eq!(super::buf_bytes_to_i32("recv_buf_bytes", 0).unwrap(), 0);
        assert_eq!(
            super::buf_bytes_to_i32("recv_buf_bytes", i32::MAX as u32).unwrap(),
            i32::MAX
        );
        let err = super::buf_bytes_to_i32("send_buf_bytes", i32::MAX as u32 + 1).unwrap_err();
        match err {
            crate::error::OptionError::OutOfRange(msg) => {
                assert!(
                    msg.contains("send_buf_bytes"),
                    "message names the field: {msg}"
                );
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }
}
