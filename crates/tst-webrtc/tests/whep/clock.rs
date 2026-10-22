//! The server's time sources: the system clock and the manual clock the
//! reaper tests drive.

use std::sync::Arc;
use std::time::Duration;

use tst_webrtc::{Clock, ManualClock, SystemClock};

#[test]
fn the_system_clock_never_runs_backwards() {
    let clock = SystemClock;
    let a = clock.now();
    let b = clock.now();
    assert!(b >= a);
}

#[test]
fn the_manual_clock_moves_only_when_advanced() {
    let clock = ManualClock::new();
    let start = clock.now();
    assert_eq!(clock.now(), start);
    clock.advance(Duration::from_secs(30));
    assert_eq!(clock.now() - start, Duration::from_secs(30));

    // The same clock is usable as a shared trait object.
    let shared: Arc<dyn Clock> = Arc::new(clock);
    assert_eq!(shared.now() - start, Duration::from_secs(30));
}
