//! Process exit with a thread still parked inside a C ABI call.
//!
//! The listener-mode opens (`tst_demux_receiver_open_listener`,
//! `tst_managed_demux_receiver_open_listener`) block in their first
//! `accept()` BEFORE they return a handle, so a C program has nothing to
//! `_cancel` or `_close` if it decides to exit while one is waiting for a
//! peer. Before the library's exit guard that process never terminated:
//! `exit()` ran `srt_cleanup()`, which waited forever on libsrt's GC
//! thread. The library now unparks every call still inside libsrt before
//! `srt_cleanup()` runs.
//!
//! A regression here is a HANG, so each test re-executes this test binary
//! as a child process with a hard deadline: the test function is the
//! PARENT when `CHILD_ENV` is unset and the CHILD when it is set. The child
//! parks a thread in the C ABI call, proves the park, prints `PARKED` and
//! calls `exit(0)`.

#![cfg(feature = "srt")]

use std::ffi::CString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tstrans::event::TstEvent;
use tstrans::receiver::demux_receiver::events::tst_demux_receiver_recv_event;
use tstrans::receiver::demux_receiver::managed::tst_managed_demux_receiver_open_listener;
use tstrans::receiver::demux_receiver::tst_demux_receiver_open_listener;
use tstrans::sender::raw_sender::tst_raw_sender_open;

/// Set (to any value) in the child's environment.
const CHILD_ENV: &str = "TST_C_EXIT_WITH_PARKED_CALL_CHILD";

/// Printed by the child once the park is proven.
const PARKED: &str = "PARKED";

/// How long the parent lets the child run before it declares a hang. The
/// exit guard's own ceiling is 2 s; this sits well above that and under
/// nextest's 20 s kill for the network group.
const CHILD_DEADLINE: Duration = Duration::from_secs(12);

/// How long the child waits for one of its own setup steps.
const SETUP_DEADLINE: Duration = Duration::from_secs(5);

fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// PARENT half: re-execute this binary on `test_name` with [`CHILD_ENV`]
/// set, and require the park marker and a clean exit inside
/// [`CHILD_DEADLINE`].
fn expect_child_exits_cleanly(test_name: &str) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child test process");

    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // The child prints a few lines at most, far below a pipe buffer, so
    // reading after it exited cannot have blocked it.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }

    let Some(status) = status else {
        panic!(
            "{test_name}: the child did not exit within {CHILD_DEADLINE:?} — \
             process exit hung with a C ABI call parked in libsrt\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    };
    assert!(
        stdout.contains(PARKED),
        "{test_name}: the child never proved the park\n\
         status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "{test_name}: the child did not exit cleanly\n\
         status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

fn wait_for(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + SETUP_DEADLINE;
    while !flag.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "{what} within {SETUP_DEADLINE:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How long the park has to hold before the child believes it.
const PARK_LOOK: Duration = Duration::from_millis(200);

/// CHILD half, last step. `entered` was set immediately before the C ABI
/// call and `returned` after it, but on their own they prove little: a
/// worker descheduled right after it set `entered` looks the same as one
/// inside libsrt. The observation is the SRT layer's own count of
/// operations in flight — non-zero means a thread has entered one and has
/// not returned from it. The park is proven when, for the whole of
/// [`PARK_LOOK`], that count is non-zero and the call has not returned.
/// Without that proof a call that failed at once would pass with the guard
/// removed.
fn prove_parked_then_exit(entered: &AtomicBool, returned: &AtomicBool) -> ! {
    wait_for(entered, "the worker must reach the C ABI call");
    let deadline = Instant::now() + SETUP_DEADLINE;
    let mut held_since = Instant::now();
    while held_since.elapsed() < PARK_LOOK {
        assert!(
            !returned.load(Ordering::SeqCst),
            "the C ABI call returned; nothing is parked"
        );
        assert!(
            Instant::now() < deadline,
            "no thread stayed inside an SRT operation within {SETUP_DEADLINE:?}"
        );
        if tst_srt::operations_in_flight() == 0 {
            held_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("{PARKED}");
    std::io::stdout().flush().expect("flush stdout");
    // What a C program's `exit(0)` / return from `main` does. No `_cancel`,
    // no `_close`, no join.
    std::process::exit(0);
}

/// Reserve an ephemeral UDP port and release it again, for the one shape
/// whose peer has to know the listener's port before the open returns.
fn reserve_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

#[test]
fn exit_is_clean_with_demux_receiver_open_listener_parked_in_its_first_accept() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_demux_receiver_open_listener_parked_in_its_first_accept",
        );
        return;
    }
    let entered = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let (e, r) = (entered.clone(), returned.clone());
    std::thread::spawn(move || {
        // Empty host: that is what selects listener mode. With a host and
        // no `?mode=listener` the `_open_listener` entry points dial.
        let url = CString::new("srt://:0").unwrap();
        e.store(true, Ordering::SeqCst);
        let _rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        r.store(true, Ordering::SeqCst);
    });
    prove_parked_then_exit(&entered, &returned);
}

#[test]
fn exit_is_clean_with_managed_demux_receiver_open_listener_parked_in_its_first_accept() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_managed_demux_receiver_open_listener_parked_in_its_first_accept",
        );
        return;
    }
    let entered = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let (e, r) = (entered.clone(), returned.clone());
    std::thread::spawn(move || {
        // Empty host: that is what selects listener mode. With a host and
        // no `?mode=listener` the `_open_listener` entry points dial.
        let url = CString::new("srt://:0").unwrap();
        e.store(true, Ordering::SeqCst);
        let _rx =
            unsafe { tst_managed_demux_receiver_open_listener(url.as_ptr(), std::ptr::null()) };
        r.store(true, Ordering::SeqCst);
    });
    prove_parked_then_exit(&entered, &returned);
}

/// The open has returned and the reader is parked in `_recv_event` on a
/// connected socket whose peer sends nothing.
#[test]
fn exit_is_clean_with_demux_receiver_parked_in_recv_event() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_demux_receiver_parked_in_recv_event",
        );
        return;
    }
    let port = reserve_port();
    let entered = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let (e, r) = (entered.clone(), returned.clone());
    std::thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        assert!(!rx.is_null(), "open_listener failed");
        let mut ev = TstEvent::default();
        e.store(true, Ordering::SeqCst);
        let _rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
        r.store(true, Ordering::SeqCst);
    });

    // The silent peer: connect (retrying until the listener has bound) and
    // keep the connection open without sending. Deliberately never closed.
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();
    let deadline = Instant::now() + SETUP_DEADLINE;
    loop {
        let tx = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
        if !tx.is_null() {
            break;
        }
        assert!(Instant::now() < deadline, "the peer never connected");
        std::thread::sleep(Duration::from_millis(50));
    }
    prove_parked_then_exit(&entered, &returned);
}
