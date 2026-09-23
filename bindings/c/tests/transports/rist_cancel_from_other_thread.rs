//! `tst_rist_receiver_cancel` from another thread must end a C-side RIST
//! receive loop with `TST_E_CLOSED` (Arc 2: WP-D gave the RIST transport a
//! real cancel handle, R4 gives the C ABI the non-freeing entry point that
//! reaches it).
//!
//! **Why a LOOP, not a parked call.** Unlike UDP, a `tst_rist_receiver_recv_ts`
//! never parks: each call is ONE ~100 ms librist poll
//! (`POLL_TIMEOUT_MS`) that returns `TST_E_BUFFER_FULL` (-4) when nothing
//! arrived, so a C caller polls in a loop. That is exactly why the
//! cross-thread interrupt here must be `_cancel` and not `_close`: `_close`
//! FREES the handle, and a caller loop racing a freeing `_close` is a
//! use-after-free, not an error code. `_cancel` never frees — the pointer
//! stays the main thread's to `_close` once, after the loop has ended.
//!
//! The Rust-side contract this pins is `RistRecvTransport::recv_bytes`
//! (`crates/tst-rist/src/recv.rs`): a cancel is checked on entry AND again
//! when the tick elapses with nothing to read, so it is reported as
//! `ExplicitClose` (→ `TST_E_CLOSED`) within at most one tick rather than
//! being hidden behind another `Backpressure`.
//!
//! Port 33100 is reserved for this C test by WP-D (EVEN — the Simple
//! profile puts RTCP on `port + 1`, `rist.c:866`), disjoint from
//! `loopback.rs` 33010–33026, `cancel.rs` 33040–33048, `conformance.rs`
//! 33050–33098, R34.7's own 33104 and the pytest suite 34110–34150.
//!
//! Failure is bounded and NEVER hangs: the loop has a wall-clock deadline
//! after which it FAILS, and every `_recv_ts` call in it returns on its own
//! within ~100 ms, so there is nothing to rescue and nothing to wedge on.
#![cfg(feature = "rist")]

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tstrans::error::TstError;
use tstrans::rist::{
    TstRistReceiver, tst_rist_receiver_cancel, tst_rist_receiver_close, tst_rist_receiver_recv_ts,
    tst_rist_recv_open,
};

/// `_cancel` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there.
struct SendPtr(*mut TstRistReceiver);
unsafe impl Send for SendPtr {}

/// Reserved by WP-D for this test; must be EVEN for the Simple profile.
const PORT: u16 = 33100;

#[test]
fn rist_receiver_cancel_from_other_thread_ends_the_poll_loop_with_closed() {
    let url = CString::new(format!("rist://@127.0.0.1:{PORT}")).unwrap();
    let rx = unsafe { tst_rist_recv_open(url.as_ptr()) };
    if rx.is_null() {
        eprintln!("skip: librist could not bind {PORT}");
        return;
    }

    // Set once the main thread's loop has completed its first poll, so the
    // canceller fires against a loop that is provably running rather than
    // against a receiver that has not started reading yet.
    let entered = Arc::new(AtomicBool::new(false));

    let canceller_ptr = SendPtr(rx);
    let canceller_entered = Arc::clone(&entered);
    let canceller = thread::spawn(move || {
        let p = canceller_ptr; // whole-struct capture: SendPtr is Send, its field is not
        let deadline = Instant::now() + Duration::from_secs(10);
        while !canceller_entered.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        unsafe { tst_rist_receiver_cancel(p.0) }
    });

    let mut buf = vec![0u8; 1316];
    let mut n = 0usize;
    let deadline = Instant::now() + Duration::from_secs(15);
    let t0 = Instant::now();
    let mut ticks = 0u32;
    let mut observed: Option<i32> = None;

    while Instant::now() < deadline {
        let rc = unsafe { tst_rist_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
        entered.store(true, Ordering::Release);
        ticks += 1;
        if rc == TstError::BufferFull as i32 {
            continue; // an empty 100 ms poll — what this loop is made of
        }
        observed = Some(rc);
        break;
    }

    let cancel_rc = canceller.join().expect("canceller thread");
    assert_eq!(cancel_rc, 0, "tst_rist_receiver_cancel must return 0");

    let rc = observed.unwrap_or_else(|| {
        // Nothing but -4 for 15 s: the cancel never reached the poll loop.
        // The loop already ended on its own deadline, so nothing is parked
        // and the handle is safe to free before failing.
        unsafe { tst_rist_receiver_close(rx) };
        panic!(
            "tst_rist_receiver_cancel did not end the poll loop within 15 s ({ticks} polls, \
             all TST_E_BUFFER_FULL)"
        );
    });
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "a cross-thread cancel must surface as TST_E_CLOSED (-7), got {rc} after {ticks} polls \
         ({:?})",
        t0.elapsed()
    );

    // Every later call keeps reporting the cancel, and `_cancel` is
    // idempotent — neither of them frees anything.
    let rc = unsafe { tst_rist_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "a post-cancel _recv_ts must keep reporting TST_E_CLOSED, got {rc}"
    );
    assert_eq!(
        unsafe { tst_rist_receiver_cancel(rx) },
        0,
        "cancel is idempotent"
    );

    // ONLY now, with no reader running, is the freeing close safe.
    unsafe { tst_rist_receiver_close(rx) };
}
