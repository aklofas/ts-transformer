//! End-to-end encrypted handshake test for srt-sys.
//!
//! Spawns a listener thread and a caller in the test thread, both on loopback.
//! Verifies that:
//!   1. With matching passphrases, the connect succeeds and a small payload
//!      round-trips intact.
//!   2. With mismatched passphrases, the connect fails (libsrt rejects the
//!      handshake before reporting success).
//!
//! Only built when the `mbedtls` feature is enabled.
//!
//! Skipped on Windows: the test reaches into `libc::sockaddr_in` /
//! `libc::sockaddr_storage`, neither of which are exposed by the
//! `libc` crate on `*-pc-windows-msvc`. Win32 uses its own `SOCKADDR`
//! family from <ws2def.h> reached via `windows-sys`. A Windows port
//! would require either pulling in `windows-sys` for the test
//! dependency or wrapping the sockaddr construction behind a
//! cross-platform shim. Tracked in `deferred-features.md` if a
//! consumer asks; for now Windows users still get end-to-end
//! coverage via the higher-level `tst-srt` / `tst-c` integration
//! tests (which use Rust's `std::net::SocketAddr` and never touch
//! libc sockaddr types directly).

#![cfg(all(feature = "mbedtls", unix))]

use srt_sys::*;
use std::ffi::CString;
use std::mem;
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

const SRT_INVALID_SOCK: SRTSOCKET = -1;

/// RAII guard that closes the wrapped SRT socket on drop.
///
/// Used by the test threads to ensure listener / caller handles are released
/// even when an `assert!` between create and the manual `srt_close` unwinds
/// the test. Without this, a panic mid-test leaks a libsrt socket into the
/// global table, which then survives `srt_cleanup` and can confuse later
/// runs in the same process.
struct SocketGuard(SRTSOCKET);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self.0 != SRT_INVALID_SOCK {
            unsafe { srt_close(self.0) };
        }
    }
}

fn ensure_startup() {
    let rc = unsafe { srt_startup() };
    assert!(rc >= 0, "srt_startup failed: rc={rc}");
}

fn cleanup() {
    let rc = unsafe { srt_cleanup() };
    assert!(rc >= 0, "srt_cleanup failed: rc={rc}");
}

fn last_error() -> String {
    unsafe {
        let p = srt_getlasterror_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn set_passphrase(sock: SRTSOCKET, passphrase: &str) {
    let cstr = CString::new(passphrase).unwrap();
    let rc = unsafe {
        srt_setsockflag(
            sock,
            SRT_SOCKOPT_SRTO_PASSPHRASE,
            cstr.as_ptr().cast(),
            cstr.as_bytes().len() as libc::c_int,
        )
    };
    assert_eq!(
        rc,
        0,
        "srt_setsockflag(PASSPHRASE) failed: {}",
        last_error()
    );
}

unsafe fn set_int_opt(sock: SRTSOCKET, opt: SRT_SOCKOPT, value: i32) {
    let rc = unsafe {
        srt_setsockflag(
            sock,
            opt,
            (&raw const value).cast(),
            mem::size_of::<i32>() as libc::c_int,
        )
    };
    assert_eq!(rc, 0, "srt_setsockflag({opt}) failed: {}", last_error());
}

fn sockaddr_from(addr: SocketAddr) -> (libc::sockaddr_storage, usize) {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            // Initialize via mem::zeroed + field assignment rather than a
            // struct literal: BSD-derived libcs (macOS, iOS, FreeBSD,
            // NetBSD, OpenBSD) include a `sin_len` field that glibc lacks,
            // so a literal listing every field stops compiling cross-
            // platform.  Zeroing covers `sin_len` (and `sin_zero`) on
            // every target.
            let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const sin).cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    mem::size_of::<libc::sockaddr_in>(),
                );
            }
            (storage, mem::size_of::<libc::sockaddr_in>())
        }
        SocketAddr::V6(_) => unimplemented!("test uses IPv4 loopback only"),
    }
}

fn port_from_sockaddr_storage(s: &libc::sockaddr_storage) -> u16 {
    assert_eq!(s.ss_family as i32, libc::AF_INET);
    let v4: &libc::sockaddr_in = unsafe { &*(&raw const *s).cast::<libc::sockaddr_in>() };
    u16::from_be(v4.sin_port)
}

/// Bind a listener on loopback at port 0 (kernel-chosen) and return
/// the bound port plus the listening socket.
unsafe fn bind_loopback_listener(passphrase: &str) -> (SRTSOCKET, u16) {
    let sock = unsafe { srt_create_socket() };
    assert_ne!(sock, SRT_INVALID_SOCK, "create_socket: {}", last_error());

    unsafe { set_passphrase(sock, passphrase) };
    // 5-second timeout to bound test wall clock.
    unsafe { set_int_opt(sock, SRT_SOCKOPT_SRTO_RCVTIMEO, 5_000) };
    unsafe { set_int_opt(sock, SRT_SOCKOPT_SRTO_SNDTIMEO, 5_000) };

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (sa, salen) = sockaddr_from(addr);
    let rc = unsafe { srt_bind(sock, (&raw const sa).cast(), salen as libc::c_int) };
    assert_eq!(rc, 0, "srt_bind: {}", last_error());

    let rc = unsafe { srt_listen(sock, 1) };
    assert_eq!(rc, 0, "srt_listen: {}", last_error());

    // Read back the kernel-assigned port.
    let mut sa_out: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut len_out = mem::size_of::<libc::sockaddr_storage>() as libc::c_int;
    let rc = unsafe { srt_getsockname(sock, (&raw mut sa_out).cast(), &raw mut len_out) };
    assert_eq!(rc, 0, "srt_getsockname: {}", last_error());

    let port = port_from_sockaddr_storage(&sa_out);
    (sock, port)
}

#[test]
fn matching_passphrase_round_trips_payload() {
    ensure_startup();

    let passphrase = "0123456789abcdef0123456789abcdef"; // 32 chars (within 10–80 limit)

    let (listener, port) = unsafe { bind_loopback_listener(passphrase) };

    // Listener thread: accept once, recv, close accepted handle.
    // Both the moved-in `listener` and the `accepted` peer are wrapped in
    // SocketGuards so a panic between create and the natural close still
    // releases the libsrt socket table entry.
    let listener_handle = thread::spawn(move || {
        let _listener_guard = SocketGuard(listener);
        let mut peer: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::c_int;
        let accepted = unsafe { srt_accept(listener, (&raw mut peer).cast(), &raw mut peer_len) };
        if accepted == SRT_INVALID_SOCK {
            return Err(format!("accept failed: {}", last_error()));
        }
        let _accepted_guard = SocketGuard(accepted);

        // Live mode requires a recv buffer >= the payload size (default 1316 bytes).
        let mut buf = [0u8; 1500];
        let n = unsafe {
            srt_recv(
                accepted,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len() as libc::c_int,
            )
        };
        if n < 0 {
            return Err(format!("recv failed: {}", last_error()));
        }
        Ok(buf[..n as usize].to_vec())
    });

    // Give the listener thread a moment to enter accept().
    thread::sleep(Duration::from_millis(100));

    // Caller side: connect and send.
    let caller = unsafe { srt_create_socket() };
    assert_ne!(caller, SRT_INVALID_SOCK);
    let caller_guard = SocketGuard(caller);
    unsafe {
        set_passphrase(caller, passphrase);
        set_int_opt(caller, SRT_SOCKOPT_SRTO_RCVTIMEO, 5_000);
        set_int_opt(caller, SRT_SOCKOPT_SRTO_SNDTIMEO, 5_000);
    }

    let server: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (sa, salen) = sockaddr_from(server);
    let rc = unsafe { srt_connect(caller, (&raw const sa).cast(), salen as libc::c_int) };
    assert_eq!(
        rc,
        0,
        "connect failed (matching passphrase): {}",
        last_error()
    );

    let payload = b"hello, encrypted srt!";
    let n = unsafe {
        srt_send(
            caller,
            payload.as_ptr().cast::<libc::c_char>(),
            payload.len() as libc::c_int,
        )
    };
    assert!(n > 0, "send returned {n}: {}", last_error());

    let received = listener_handle.join().expect("listener thread panicked");
    let received = received.expect("listener returned error");
    assert_eq!(&received[..], payload, "round-tripped payload mismatch");

    drop(caller_guard);
    cleanup();
}

#[test]
fn mismatched_passphrase_rejects_connect() {
    ensure_startup();

    let listener_pass = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let caller_pass = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    let (listener, port) = unsafe { bind_loopback_listener(listener_pass) };
    // Listener is owned by the main thread: closing it from main is what
    // unblocks the spawned `srt_accept` below, so the SocketGuard lives
    // here rather than in the thread closure. `SRTSOCKET` is `i32` (Copy),
    // so the `move` closure receives its own copy of the integer; only
    // this guard calls `srt_close`.
    let listener_guard = SocketGuard(listener);

    // Listener thread: accept with timeout; close any accepted handle.
    let listener_handle = thread::spawn(move || {
        let mut peer: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::c_int;
        let accepted = unsafe { srt_accept(listener, (&raw mut peer).cast(), &raw mut peer_len) };
        if accepted != SRT_INVALID_SOCK {
            let _accepted_guard = SocketGuard(accepted);
        }
    });

    thread::sleep(Duration::from_millis(100));

    let caller = unsafe { srt_create_socket() };
    assert_ne!(caller, SRT_INVALID_SOCK);
    let caller_guard = SocketGuard(caller);
    unsafe {
        set_passphrase(caller, caller_pass);
        set_int_opt(caller, SRT_SOCKOPT_SRTO_RCVTIMEO, 5_000);
        set_int_opt(caller, SRT_SOCKOPT_SRTO_SNDTIMEO, 5_000);
    }

    let server: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (sa, salen) = sockaddr_from(server);
    let rc = unsafe { srt_connect(caller, (&raw const sa).cast(), salen as libc::c_int) };

    // Mismatched passphrase: connect must fail.
    assert_ne!(
        rc, 0,
        "connect unexpectedly succeeded with mismatched passphrase"
    );
    let err = last_error();
    println!("expected connect failure: {err}");

    drop(caller_guard);
    drop(listener_guard); // unblocks the listener thread
    let _ = listener_handle.join();
    cleanup();
}
