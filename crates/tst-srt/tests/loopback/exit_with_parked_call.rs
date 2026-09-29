//! Process exit with a thread still parked inside libsrt.
//!
//! `tst-srt` registers `srt_cleanup()` with C `atexit`, and `srt_cleanup()`
//! joins libsrt's `SRT:GC` thread. With a thread parked in `srt_accept` the
//! GC thread cannot finish (it destroys the condition variable the accept
//! is waiting on), so the process never exits. The exit handler therefore
//! closes every socket that is still open and waits for the parked calls to
//! return BEFORE it calls `srt_cleanup()` — see `src/exit_guard.rs`.
//!
//! A regression here is a HANG, which no in-process assertion can catch, so
//! every test re-executes this test binary as a child process and gives it
//! a hard deadline: the test function is the PARENT when [`CHILD_ENV`] is
//! unset and the CHILD when it is set. The child parks a thread, proves the
//! park, prints [`PARKED`] and calls `std::process::exit(0)`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tst_core::mpegts::demux::DemuxerConfig;
use tst_pipeline::ReconnectPolicy;
use tst_srt::url::SrtUrl;
use tst_srt::{ListenerBuilder, SocketBuilder};

/// Set (to any value) in the child's environment.
const CHILD_ENV: &str = "TST_SRT_EXIT_WITH_PARKED_CALL_CHILD";

/// Printed by the child once the park is proven.
const PARKED: &str = "PARKED";

/// How long the parent lets the child run before it declares a hang. The
/// exit handler's own ceiling is 2 s; this sits well above that and under
/// nextest's 20 s kill for the network group.
const CHILD_DEADLINE: Duration = Duration::from_secs(12);

/// Entered/returned latch around a blocking call, the same shape the Python
/// exit test uses: `entered` is set immediately before the call and
/// `returned` after it, so "entered and not returned" proves the thread is
/// inside the call. Without that proof a test whose call failed at once
/// would pass with the guard removed.
#[derive(Clone, Default)]
struct ParkLatch {
    entered: Arc<AtomicBool>,
    returned: Arc<AtomicBool>,
}

impl ParkLatch {
    /// Run `call` on a detached thread, bracketed by the latch.
    fn park<F, R>(&self, call: F)
    where
        F: FnOnce() -> R + Send + 'static,
    {
        let latch = self.clone();
        std::thread::spawn(move || {
            latch.entered.store(true, Ordering::SeqCst);
            let _ = call();
            latch.returned.store(true, Ordering::SeqCst);
        });
    }

    /// Wait for the thread to reach the call, then require the call to
    /// still be outstanding after a short bounded look.
    fn assert_parked(&self) {
        crate::common::wait_for_ready(&self.entered);
        let look_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < look_until && !self.returned.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !self.returned.load(Ordering::SeqCst),
            "the blocking call returned; nothing is parked"
        );
        println!("{PARKED}");
        std::io::stdout().flush().expect("flush stdout");
    }
}

/// PARENT half: re-execute this binary on `test_name` with [`CHILD_ENV`]
/// set, and require a clean exit inside [`CHILD_DEADLINE`].
fn expect_child_exits_cleanly(test_name: &str, expect_parked: bool) {
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
             process exit hung with a call parked in libsrt\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    };
    if stdout.contains("SKIP: loopback unavailable") || stderr.contains("SKIP: loopback") {
        eprintln!("SKIP: loopback unavailable in the child");
        return;
    }
    if expect_parked {
        assert!(
            stdout.contains(PARKED),
            "{test_name}: the child never proved the park\n\
             status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    assert_eq!(
        status.code(),
        Some(0),
        "{test_name}: the child did not exit cleanly\n\
         status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// A listener parked in the plain blocking `accept()`.
#[test]
fn exit_is_clean_with_a_listener_parked_in_accept() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_a_listener_parked_in_accept",
            true,
        );
        return;
    }
    require_loopback!();
    let mut listener = ListenerBuilder::new()
        .bind("127.0.0.1:0")
        .expect("bind 127.0.0.1:0");
    let latch = ParkLatch::default();
    latch.park(move || listener.accept());
    latch.assert_parked();
    // Deliberately no close and no join: the leaked-parked-thread shape.
    std::process::exit(0);
}

/// A listener parked in `accept_timeout()` — the wait is `srt_epoll_wait`,
/// not `srt_accept`.
#[test]
fn exit_is_clean_with_a_listener_parked_in_accept_timeout() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_a_listener_parked_in_accept_timeout",
            true,
        );
        return;
    }
    require_loopback!();
    let mut listener = ListenerBuilder::new()
        .bind("127.0.0.1:0")
        .expect("bind 127.0.0.1:0");
    let latch = ParkLatch::default();
    latch.park(move || listener.accept_timeout(Duration::from_secs(300)));
    latch.assert_parked();
    std::process::exit(0);
}

/// The shape no caller-side cancel can reach: the FIRST accept inside the
/// listener-mode URL open every binding uses. The open has not returned, so
/// nothing exists to close.
#[test]
fn exit_is_clean_with_a_thread_parked_in_the_url_opens_first_accept() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_a_thread_parked_in_the_url_opens_first_accept",
            true,
        );
        return;
    }
    require_loopback!();
    let url = SrtUrl::parse("srt://127.0.0.1:0?mode=listener").expect("parse url");
    let latch = ParkLatch::default();
    latch.park(move || {
        tst_srt::shells::managed_demux_receiver_from_url(
            &url,
            ReconnectPolicy::default(),
            DemuxerConfig::default(),
        )
        .map(|_| ())
    });
    latch.assert_parked();
    std::process::exit(0);
}

/// A connected socket parked in `recv()` with nothing to read.
#[test]
fn exit_is_clean_with_a_socket_parked_in_recv() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_a_socket_parked_in_recv",
            true,
        );
        return;
    }
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    // The accepted side only has to stay open: hand it back and keep it.
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();
    let mut socket = SocketBuilder::new()
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    let _accepted = accept.join();

    let latch = ParkLatch::default();
    latch.park(move || {
        let mut buf = [0u8; 1500];
        socket.recv(&mut buf)
    });
    latch.assert_parked();
    std::process::exit(0);
}

/// A caller parked in `connect()`: the peer is a plain UDP socket that
/// never answers the handshake.
#[test]
fn exit_is_clean_with_a_caller_parked_in_connect() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_with_a_caller_parked_in_connect",
            true,
        );
        return;
    }
    require_loopback!();
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind the silent peer");
    let port = silent.local_addr().expect("local_addr").port();
    let latch = ParkLatch::default();
    latch.park(move || {
        SocketBuilder::new()
            .connect_timeout(Duration::from_secs(300))
            .connect(format!("127.0.0.1:{port}"))
    });
    latch.assert_parked();
    drop(silent);
    std::process::exit(0);
}

/// Control: everything closed before exit. Must exit 0 as it always did.
#[test]
fn exit_is_clean_after_a_clean_close() {
    if !is_child() {
        expect_child_exits_cleanly(
            "exit_with_parked_call::exit_is_clean_after_a_clean_close",
            false,
        );
        return;
    }
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();
    let socket = SocketBuilder::new()
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    let accepted = accept.join();
    socket.close().expect("close caller");
    accepted.close().expect("close accepted");
    std::process::exit(0);
}
