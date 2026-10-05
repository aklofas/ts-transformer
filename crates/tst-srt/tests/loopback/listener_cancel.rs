//! `Listener::cancel_handle()` is the sanctioned cross-thread wake for a
//! parked `accept()`. This pins the property the managed-receiver
//! cancellable re-accept is built on: cancelling
//! from another thread makes a blocked `accept()` return promptly.
//! Requires libsrt loopback.

use std::time::{Duration, Instant};
use tst_srt::ListenerBuilder;

#[test]
fn cancel_handle_wakes_parked_accept_promptly() {
    require_loopback!();
    let mut listener = ListenerBuilder::new().bind("127.0.0.1:0").expect("bind");
    let cancel = listener.cancel_handle();

    // Fire the cancel from another thread while accept() is parked.
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        cancel.cancel();
    });

    let start = Instant::now();
    let result = listener.accept();
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    let err = match result {
        Ok(_) => panic!("accept returned a socket although no peer ever connected"),
        Err(e) => e,
    };
    // Recorded so the variant a cancelled accept surfaces as is visible
    // in the test log (the managed re-accept helper does not depend on it).
    eprintln!("accept after cancel -> {err:?} after {elapsed:?}");
    assert!(
        elapsed >= Duration::from_millis(150),
        "accept returned before the cancel fired: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel did not wake the parked accept promptly: {elapsed:?}"
    );
}
