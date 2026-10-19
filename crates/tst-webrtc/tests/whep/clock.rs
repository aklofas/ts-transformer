//! The server's time sources: the system clock and the manual clock the
//! reaper tests drive.

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
}

#[test]
fn a_clock_is_usable_as_a_shared_trait_object() {
    let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(ManualClock::default());
    let start = clock.now();
    assert_eq!(clock.now(), start);
}
