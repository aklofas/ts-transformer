//! C-ABI URL parsing tests for `tst_demux_receiver_*` (plain + managed).
//!
//! Placeholder note (still true for URL-grammar coverage): receiver-side
//! `_open` entry points accept the same SRT URL grammar (parsed by
//! `tst_srt::SrtUrl::parse`, which already round-trips mode=listener —
//! see `url_mode_listener_parse_accepted` in `ts_sender.rs`). Dedicated
//! demux-receiver URL roundtrip tests live in
//! `tests/receiving/demux_receiver_loopback.rs`.
//!
//! This file also carries the managed-demux-receiver lifecycle test that
//! needs a real SRT socket rendezvous (Task 9): stream-end reason,
//! reconnect-stats baseline, and the `unwrap_timestamps` config knob. It
//! lives here rather than in `receiving/demux_receiver_loopback.rs`
//! because it exercises the `tst_managed_demux_receiver_*` *caller*-mode
//! open path against a background-thread *listener* built directly on
//! `tst_srt::ListenerBuilder` — the same threading shape as
//! `managed_ts_sender_url_options_persist_across_reconnect` in
//! `ts_sender.rs`, just mirrored sender→receiver.

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::ListenerBuilder;
use tstrans::demux_config::{
    tst_demux_config_free, tst_demux_config_new, tst_demux_config_set_unwrap_timestamps,
};
use tstrans::error::TstError;
use tstrans::event::TstEvent;
use tstrans::receiver::demux_receiver::{
    tst_managed_demux_receiver_cancel, tst_managed_demux_receiver_close,
    tst_managed_demux_receiver_end_reason, tst_managed_demux_receiver_get_reconnect_stats,
    tst_managed_demux_receiver_open_with_config, tst_managed_demux_receiver_recv_event,
};
use tstrans::stats::TstManagedTransportStats;
use tstrans::stream_end_reason::TstStreamEndReason;

use super::last_error_msg;

/// Single managed-demux-receiver caller-mode connection, exercised
/// against all three Task 9 additions in sequence to keep this file's
/// contribution to the binary's process-wide SRT socket count to one
/// connection rather than three (see `accept_with_retry`'s doc for why
/// that concurrency matters here):
///
/// 1. `tst_managed_demux_receiver_open_with_config` with
///    `unwrap_timestamps` on — proves the C-ABI knob plumbs through
///    without error opening a real receiver (the unwrap arithmetic
///    itself is covered at the `tst-core` level).
/// 2. `end_reason` reads `NONE` while the session is still live.
/// 3. `get_reconnect_stats` on a freshly-opened, never-reconnected
///    receiver reads the zero/not-reconnecting baseline — same shape as
///    the existing send-side `variant_managed_*_get_reconnect_stats`
///    tests (`url_open/{mux_sender,raw_sender,ts_sender}.rs`), which
///    also assert only the freshly-opened zero state rather than an
///    induced reconnect.
/// 4. Cancel, then one `recv_event` call so the managed receiver's
///    inner loop actually observes the cancel signal and records it
///    (cancel alone only flags the transport — same two-step shape as
///    `tst_rtp_demux_receiver_end_reason`'s
///    `cancel_then_next_event_records_cancelled_end_reason` test).
/// 5. `end_reason` now reads `CANCELLED`.
///
/// **A real induced-reconnect flip assertion for `get_reconnect_stats`
/// was attempted and reverted.** A background-thread listener
/// drop/rebind (mirroring
/// `managed_ts_sender_url_options_persist_across_reconnect` in
/// `ts_sender.rs`) does reach `reconnecting == true` and
/// `reconnect_successes == 1`, confirmed by running it — but real SRT
/// peer-disconnect detection on an otherwise-idle caller-mode connection
/// (no TS bytes ever sent) took on the order of a **minute**, not the
/// sub-second-to-few-seconds window the sender-side test's timing
/// assumptions predicted, making a bounded, non-flaky version
/// impractical as a routine CI test. `reconnects_count()` incrementing
/// IS covered without any real socket by `tst-pipeline`'s
/// `crates/tst-pipeline/tests/managed_demux_receiver_reconnect.rs`
/// (`ScriptedInner`-injected `RecvTransport`, deterministic, no timing
/// dependency), and so is the `reconnecting()` true→false→latched-true
/// flip, by that suite's
/// `reconnecting_flag_true_during_outage_false_after_rebuild_latched_after_giveup`:
/// a channel-gated factory holds the outage open until a watcher thread
/// has seen the flag, so the transient state is a handshake, not a
/// timing window. The C layer therefore asserts only the baseline.
#[test]
fn managed_demux_receiver_end_reason_reconnect_stats_and_unwrap_config() {
    // Bind on the test thread so the port AND the listener's cancel handle
    // are in hand before the peer thread parks in `accept()`.
    let mut listener = ListenerBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .bind("127.0.0.1:0")
        .expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let listener_cancel = listener.cancel_handle();
    let (done_tx, done_rx) = mpsc::channel::<()>();

    let listener_thread = thread::spawn(move || {
        // `.ok()`, not `.expect()`: this accept can legitimately lose a
        // race. The test thread cancels its socket microseconds after
        // `connect` returns, and libsrt's GC pass
        // (`CUDTUnited::checkBrokenSockets`) prunes a connection that
        // breaks while still sitting in the listener's accept queue. If
        // this thread is descheduled across that window (a loaded 2-vCPU
        // CI runner; 2 sightings 2026-09-15/16), the queued connection
        // vanishes and a plain `accept()` would sleep forever — nothing
        // else ever connects. The test thread fires `listener_cancel`
        // before joining, which closes the listener and wakes a parked
        // accept with an error instead.
        let _accepted = listener.accept().ok();
        // Hold the connection open (both `_accepted` and `listener` alive
        // in scope) until the main thread is done with it.
        done_rx.recv_timeout(Duration::from_secs(5)).ok();
    });

    let url_c = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg = tst_demux_config_new();
        assert!(!cfg.is_null());
        assert_eq!(tst_demux_config_set_unwrap_timestamps(cfg, 1), 0);

        let rx = tst_managed_demux_receiver_open_with_config(url_c.as_ptr(), std::ptr::null(), cfg);
        assert!(
            !rx.is_null(),
            "open_with_config(unwrap_timestamps=on) failed: {}",
            last_error_msg()
        );
        tst_demux_config_free(cfg);

        // Still live: NONE.
        let mut reason = TstStreamEndReason::Cancelled; // seed a non-None value
        let rc = tst_managed_demux_receiver_end_reason(rx, &mut reason);
        assert_eq!(rc, 0, "end_reason failed: {}", last_error_msg());
        assert!(
            matches!(reason, TstStreamEndReason::None),
            "expected None while live"
        );

        // Freshly connected, never reconnected: zero/not-reconnecting
        // baseline. No gap buffer on the recv side (see
        // `tst_managed_demux_receiver_get_reconnect_stats`'s doc).
        let mut stats = TstManagedTransportStats::default();
        let rc = tst_managed_demux_receiver_get_reconnect_stats(rx, &mut stats);
        assert_eq!(rc, 0, "get_reconnect_stats failed: {}", last_error_msg());
        assert!(!stats.reconnecting, "should not be reconnecting yet");
        assert_eq!(stats.reconnect_successes, 0);
        assert_eq!(stats.reconnect_attempts, 0);
        assert_eq!(stats.gap_len, 0, "recv side has no gap buffer");
        assert_eq!(stats.gap_messages_dropped, 0);
        assert_eq!(stats.gap_bytes_dropped, 0);

        // Cancel, then one recv_event call to let the managed receiver's
        // inner loop actually observe the cancel signal and record it.
        let cancel_rc = tst_managed_demux_receiver_cancel(rx);
        assert_eq!(cancel_rc, 0);

        let mut ev = TstEvent::default();
        let next_rc = tst_managed_demux_receiver_recv_event(rx, &mut ev);
        assert_eq!(next_rc, TstError::Closed as i32);

        let mut reason2 = TstStreamEndReason::None;
        let rc2 = tst_managed_demux_receiver_end_reason(rx, &mut reason2);
        assert_eq!(rc2, 0, "end_reason failed: {}", last_error_msg());
        assert!(
            matches!(reason2, TstStreamEndReason::Cancelled),
            "expected Cancelled after cancel + recv_event, got a different reason"
        );

        tst_managed_demux_receiver_close(rx);
    }

    done_tx.send(()).ok();
    // Wake the peer if it is still parked in `accept()` (see the thread
    // body). Idempotent: if the thread already exited and dropped the
    // listener, this is a no-op.
    listener_cancel.cancel();
    listener_thread.join().expect("listener thread panicked");
}
