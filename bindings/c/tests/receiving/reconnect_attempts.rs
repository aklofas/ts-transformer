//! `reconnect_attempts` MEANS attempts — the recv side's Arc 2 fix.
//!
//! Before 0.7.0 `tst_managed_demux_receiver_get_reconnect_stats` read the
//! SUCCESS counter into BOTH fields (`let successes = handle.reconnects…;
//! reconnect_attempts: successes, reconnect_successes: successes`), so a
//! reconnect loop that only ever FAILED reported `attempts == 0` — "never
//! attempted" — which is exactly the state an operator most needs to see.
//! The two fields now come from `ManagedHandles::{attempts, reconnects}`.
//!
//! Shape: bounded and latch-and-poll, never a wall-clock assert.
//!   1. a real SRT listener accepts one caller, then goes away for good;
//!   2. the managed receiver's factory therefore FAILS every attempt;
//!   3. the policy caps it at ONE attempt with the tightest backoff the C
//!      ABI exposes (constant 10 ms) — one failed attempt already proves
//!      the two counters are independent, and each extra one costs
//!      libsrt's connect-timeout floor (~1 s; `?conntimeo=` is clamped);
//!   4. poll the stats until `attempts > 0` against a FAILING watchdog.
//!
//! Own port band: 33_000.
#![cfg(feature = "srt")]

use std::ffi::{CStr, CString};
use std::time::{Duration, Instant};

use tst_srt::ListenerBuilder;
use tstrans::config::{
    tst_reconnect_policy_free, tst_reconnect_policy_new,
    tst_reconnect_policy_set_backoff_constant_ms, tst_reconnect_policy_set_max_attempts,
};
use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::event::TstEvent;
use tstrans::receiver::demux_receiver::managed::{
    tst_managed_demux_receiver_close, tst_managed_demux_receiver_get_reconnect_stats,
    tst_managed_demux_receiver_open, tst_managed_demux_receiver_recv_event,
};
use tstrans::stats::TstManagedTransportStats;

const WATCHDOG: Duration = Duration::from_secs(10);

fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[test]
fn recv_side_reconnect_attempts_counts_attempts_not_successes() {
    // A listener that accepts exactly one peer and is then dropped: every
    // later factory call (a fresh connect to the same port) fails.
    let mut listener = ListenerBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .bind("127.0.0.1:0")
        .expect("bind");
    let port = listener.local_addr().expect("local_addr").port();

    let policy = unsafe { tst_reconnect_policy_new() };
    unsafe {
        // ONE attempt is all the pin needs (attempts=1, successes=0 already
        // proves the two counters are independent), and each extra attempt
        // costs libsrt's connect-timeout floor (~1 s, `conntimeo` is clamped).
        tst_reconnect_policy_set_max_attempts(policy, 1);
        tst_reconnect_policy_set_backoff_constant_ms(policy, 10);
    }

    // `?conntimeo=` bounds the failing reconnect: without it the attempt
    // would wait out the sender preset's 15 s connect timeout.
    // `?x-recvtimeout=` bounds the receive itself so the loop below spins
    // instead of parking until libsrt's peer-idle timeout.
    let url = CString::new(format!(
        "srt://127.0.0.1:{port}?conntimeo=150&x-recvtimeout=100"
    ))
    .unwrap();
    let accepted = std::thread::spawn(move || listener.accept().ok().map(|(s, _)| s));
    let h = unsafe { tst_managed_demux_receiver_open(url.as_ptr(), policy) };
    unsafe { tst_reconnect_policy_free(policy) };
    assert!(!h.is_null(), "open: {}", last_error_msg());

    // Drop BOTH the accepted socket and the listener: the peer is gone and
    // nothing is bound on `port` any more, so every reconnect attempt fails.
    drop(accepted.join().expect("listener thread"));

    // Drive the receive until the reconnect budget is exhausted. Bounded by
    // the policy (one attempt, 10 ms backoff) plus libsrt's own peer-idle
    // break detection; the watchdog FAILS rather than looping forever.
    let deadline = Instant::now() + WATCHDOG;
    let mut last_rc = 0;
    while Instant::now() < deadline {
        let mut ev = TstEvent::default();
        last_rc = unsafe { tst_managed_demux_receiver_recv_event(h, &mut ev) };
        // `?x-recvtimeout=` makes an idle receive return the RETRYABLE
        // TST_E_BUFFER_FULL; only a terminal code ends the drive loop.
        if last_rc != 0 && last_rc != TstError::BufferFull as i32 {
            break;
        }
    }
    assert_ne!(
        last_rc, 0,
        "the receive never ended within {WATCHDOG:?} — the reconnect budget \
         should have been exhausted after its one failing attempt"
    );

    // Latch-and-poll the observer: attempts must have been counted.
    let mut stats = TstManagedTransportStats::default();
    let deadline = Instant::now() + WATCHDOG;
    loop {
        let rc = unsafe { tst_managed_demux_receiver_get_reconnect_stats(h, &mut stats) };
        assert_eq!(rc, 0, "get_reconnect_stats: {}", last_error_msg());
        if stats.reconnect_attempts > 0 || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        stats.reconnect_attempts > 0,
        "reconnect_attempts must count every factory call; got attempts={} successes={}",
        stats.reconnect_attempts,
        stats.reconnect_successes
    );
    assert_eq!(
        stats.reconnect_successes, 0,
        "no reconnect can have succeeded — nothing is bound on the port any more"
    );
    // The point of the fix: the two fields are no longer the same number.
    assert_ne!(
        stats.reconnect_attempts, stats.reconnect_successes,
        "attempts and successes must be independent counters (before 0.7.0 the \
         recv side reported successes in BOTH fields, so a failing reconnect \
         loop read as 'never attempted')"
    );

    unsafe { tst_managed_demux_receiver_close(h) };
}
