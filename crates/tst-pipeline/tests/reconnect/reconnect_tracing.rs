//! Verifies `tracing` instrumentation on the sender-side reconnect loop
//! (`ManagedTransport::reconnect_and_drain`).
//!
//! Uses `tracing-test` to assert that the documented log emissions
//! (info on each reconnect attempt, warn on terminal give-up) actually
//! fire during a real reconnect-and-give-up flow.

use crate::common::{ScriptedSend, broken, wait_for};

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;
use tracing_test::traced_test;
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, ReconnectMode, ReconnectPolicy, Transport, TransportError,
};

#[traced_test]
#[test]
fn sender_reconnect_emits_info_on_attempt_and_warn_on_give_up() {
    // Factory always fails so the reconnect budget is exhausted and the
    // give-up branch fires.
    let factory =
        || -> Result<ScriptedSend, TransportError> { Err(broken("test factory always fails")) };

    // Cap attempts low + zero backoff so the test is fast.
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)));

    let inner = ScriptedSend::always(broken("always broken (test)"))
        .with_counter(Arc::new(AtomicU32::new(0)));
    let mut managed = ManagedTransport::new(inner, factory, policy).expect("the policy validates");

    // Drive the reconnect path: a single send triggers Broken → enqueue →
    // reconnect_and_drain → 3 factory failures → give-up.
    let err = managed.send_bytes(b"trigger").unwrap_err();
    assert!(
        matches!(err, TransportError::Broken { .. }),
        "expected Broken after give-up, got {err:?}"
    );

    // INFO  "reconnect attempt"   (one per attempt, 3 total)
    // WARN  "reconnect gave up"   (one terminal)
    assert!(
        logs_contain("reconnect attempt"),
        "expected at least one INFO 'reconnect attempt' event"
    );
    assert!(
        logs_contain("reconnect gave up"),
        "expected a WARN 'reconnect gave up' event after exhausting the budget"
    );
}

/// A managed sender whose inner is dead and whose factory answers every
/// call with `factory_answer`.
fn dead_sender(
    factory_answer: TransportError,
    policy: ReconnectPolicy,
) -> ManagedTransport<ScriptedSend> {
    let factory = move || -> Result<ScriptedSend, TransportError> { Err(factory_answer.clone()) };
    let inner = ScriptedSend::always(broken("always broken (test)"));
    ManagedTransport::new(inner, factory, policy).expect("the policy validates")
}

fn three_fast_attempts() -> ReconnectPolicy {
    ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)))
}

/// Whether one captured line, from any thread, holds every needle.
///
/// `logs_contain` only sees lines written inside the test's own span, and a
/// span does not follow an event onto the reconnect worker's thread. The
/// worker's lines are in the same capture buffer, without the span, so a
/// test that reads them here must look for text no other test writes.
fn any_thread_line_has(needles: &[&str]) -> bool {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    String::from_utf8_lossy(&buf)
        .lines()
        .any(|line| needles.iter().all(|n| line.contains(n)))
}

#[traced_test]
#[test]
fn a_failed_blocking_reconnect_attempt_logs_its_cause() {
    let mut managed = dead_sender(broken("test factory always fails"), three_fast_attempts());
    managed.send_bytes(b"trigger").unwrap_err();

    assert!(
        logs_contain("test factory always fails"),
        "the factory's error reaches the log"
    );
    logs_assert(|lines: &[&str]| {
        let failed: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("reconnect attempt failed"))
            .collect();
        if failed.len() != 3 {
            return Err(format!("one line per failed call, got {}", failed.len()));
        }
        for (i, l) in failed.iter().enumerate() {
            let attempt = format!("attempt={}", i + 1);
            for needle in [
                "WARN",
                attempt.as_str(),
                "max_attempts=3",
                "test factory always fails",
            ] {
                if !l.contains(needle) {
                    return Err(format!("no {needle:?} in {l:?}"));
                }
            }
        }
        Ok(())
    });
}

/// The transport's own error code rides along when the factory's error has
/// one.
#[traced_test]
#[test]
fn a_failed_reconnect_attempt_logs_the_error_code_of_a_broken_dial() {
    let refused = TransportError::Broken {
        msg: "dial refused (test)".into(),
        errno_code: Some(2001),
        cause: tst_core::transport::BrokenCause::Unspecified,
    };
    let mut managed = dead_sender(refused, three_fast_attempts());
    managed.send_bytes(b"trigger").unwrap_err();
    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .any(|l| l.contains("reconnect attempt failed") && l.contains("errno_code=2001"))
            .then_some(())
            .ok_or_else(|| "no failed-attempt line carries errno_code=2001".to_string())
    });
}

#[traced_test]
#[test]
fn a_failed_background_reconnect_attempt_logs_its_cause() {
    // Text and budget no other test in this binary uses: the worker's lines
    // are found by them.
    const CAUSE: &str = "background factory down (logs-its-cause test)";
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(4_441))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)))
        .with_mode(ReconnectMode::Background);
    // Refuse two calls, then stay up, so the worker leaves by itself.
    let calls = Arc::new(AtomicU32::new(0));
    let factory = {
        let calls = Arc::clone(&calls);
        move || -> Result<ScriptedSend, TransportError> {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                return Err(broken(CAUSE));
            }
            Ok(ScriptedSend::ok())
        }
    };
    let inner = ScriptedSend::always(broken("always broken (test)"));
    let mut managed = ManagedTransport::new(inner, factory, policy).expect("the policy validates");
    let stats = managed.stats_handle();

    managed.send_bytes(b"trigger").expect("accepted");
    assert!(
        wait_for(Duration::from_secs(10), || !stats
            .stats()
            .unwrap()
            .reconnecting),
        "the worker never left"
    );
    for attempt in ["attempt=1", "attempt=2"] {
        assert!(
            any_thread_line_has(&[
                "WARN",
                "background reconnect attempt failed",
                attempt,
                "max_attempts=4441",
                CAUSE,
            ]),
            "no failed-attempt line for {attempt}"
        );
    }
}

/// A dial that was cancelled was not refused: it ends the wrapper and is not
/// reported as a failed attempt.
#[traced_test]
#[test]
fn a_cancelled_dial_is_not_logged_as_a_failed_attempt() {
    let mut managed = dead_sender(TransportError::ExplicitClose, three_fast_attempts());
    let err = managed.send_bytes(b"trigger").unwrap_err();
    assert!(matches!(err, TransportError::ExplicitClose), "got {err:?}");
    assert!(logs_contain("reconnect attempt"), "the attempt was logged");
    assert!(!logs_contain("reconnect attempt failed"));
}

#[traced_test]
#[test]
fn a_cancelled_background_dial_is_not_logged_as_a_failed_attempt() {
    // A budget no other test uses marks this worker's lines.
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(4_447))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)))
        .with_mode(ReconnectMode::Background);
    let mut managed = dead_sender(TransportError::ExplicitClose, policy);
    let stats = managed.stats_handle();

    managed.send_bytes(b"trigger").expect("accepted");
    assert!(
        wait_for(Duration::from_secs(10), || !stats
            .stats()
            .unwrap()
            .reconnecting),
        "the worker never left"
    );
    assert!(
        any_thread_line_has(&["background reconnect attempt", "max_attempts=4447"]),
        "the worker's lines are captured"
    );
    assert!(!any_thread_line_has(&[
        "reconnect attempt failed",
        "max_attempts=4447"
    ]));
}

#[traced_test]
#[test]
fn the_give_up_warning_names_the_last_cause() {
    let mut managed = dead_sender(broken("test factory always fails"), three_fast_attempts());
    managed.send_bytes(b"trigger").unwrap_err();
    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .any(|l| {
                l.contains("WARN")
                    && l.contains("reconnect gave up")
                    && l.contains("test factory always fails")
            })
            .then_some(())
            .ok_or_else(|| "the give-up line does not name the last cause".to_string())
    });
}

#[traced_test]
#[test]
fn the_background_give_up_warning_names_the_last_cause() {
    const CAUSE: &str = "background factory down (give-up test)";
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(2))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)))
        .with_mode(ReconnectMode::Background);
    let mut managed = dead_sender(broken(CAUSE), policy);
    let stats = managed.stats_handle();

    managed.send_bytes(b"trigger").expect("accepted");
    assert!(
        wait_for(Duration::from_secs(10), || !stats
            .stats()
            .unwrap()
            .reconnecting),
        "the worker never left"
    );
    assert!(any_thread_line_has(&[
        "WARN",
        "background reconnect gave up",
        CAUSE
    ]));
}

/// An unlimited budget has no limit to print: the attempt line carries no
/// `max_attempts` field (it used to print `max_attempts=0`).
#[traced_test]
#[test]
fn an_unlimited_budget_logs_no_max_attempts() {
    let policy = ReconnectPolicy::default()
        .with_max_attempts(None)
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(1)));
    let mut managed = dead_sender(TransportError::ExplicitClose, policy);
    managed.send_bytes(b"trigger").unwrap_err();
    logs_assert(|lines: &[&str]| {
        let attempts: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("reconnect attempt"))
            .collect();
        if attempts.is_empty() {
            return Err("no attempt line".into());
        }
        // `max_attempts=`: the test's own name is on every line.
        match attempts.iter().find(|l| l.contains("max_attempts=")) {
            Some(l) => Err(format!("a limit printed for an unlimited budget: {l:?}")),
            None => Ok(()),
        }
    });
}
