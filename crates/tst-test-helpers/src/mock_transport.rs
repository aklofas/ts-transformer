//! Test-only mock `Transport` that records sent messages and can be
//! programmed to fail in deterministic patterns. Used across pipeline_*
//! integration tests.

use std::sync::{Arc, Mutex, MutexGuard};
use tst_core::{BrokenCause, Transport, TransportError};

#[derive(Debug, Clone)]
pub enum FailMode {
    /// Always succeed (default).
    Never,
    /// Return Broken on the next N sends, then succeed.
    BrokenForN(usize),
    /// Return Backpressure on the next N sends, then succeed.
    BackpressureForN(usize),
}

pub struct MockTransport {
    max_payload: usize,
    closed: bool,
    log: Arc<Mutex<Vec<Vec<u8>>>>,
    fail_mode: Arc<Mutex<FailMode>>,
}

impl MockTransport {
    pub fn new(max_payload: usize) -> Self {
        Self {
            max_payload,
            closed: false,
            log: Arc::new(Mutex::new(Vec::new())),
            fail_mode: Arc::new(Mutex::new(FailMode::Never)),
        }
    }

    pub fn log(&self) -> Arc<Mutex<Vec<Vec<u8>>>> {
        Arc::clone(&self.log)
    }

    pub fn fail_handle(&self) -> Arc<Mutex<FailMode>> {
        Arc::clone(&self.fail_mode)
    }

    /// Lock the log mutex, recovering from poisoning by taking the inner
    /// guard. Test helpers should not propagate poison panics — the inner
    /// `Vec<Vec<u8>>` is just an append-only sink, so any prior panic's
    /// partial mutation is harmless to ignore. Mirrors the Wave 4 mutex
    /// sweep pattern in the production crates; for test helpers we use
    /// `unwrap_or_else(|e| e.into_inner())` rather than mapping to a typed
    /// error because the test helper has no error channel to surface it
    /// on (the `Transport::send_bytes` errors are reserved for simulating
    /// transport behavior under `FailMode`).
    fn lock_log(&self) -> MutexGuard<'_, Vec<Vec<u8>>> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock the fail-mode mutex, recovering from poisoning. See
    /// [`MockTransport::lock_log`] for rationale.
    fn lock_fail_mode(&self) -> MutexGuard<'_, FailMode> {
        self.fail_mode.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Transport for MockTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.max_payload,
            });
        }
        let mut mode = self.lock_fail_mode();
        match &mut *mode {
            FailMode::BrokenForN(n) if *n > 0 => {
                *n -= 1;
                return Err(TransportError::Broken {
                    msg: "mock broken".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
            FailMode::BackpressureForN(n) if *n > 0 => {
                *n -= 1;
                return Err(TransportError::Backpressure {
                    msg: "mock backpressure".into(),
                    errno_code: None,
                });
            }
            _ => {}
        }
        self.lock_log().push(msg.to_vec());
        Ok(())
    }

    fn max_payload(&self) -> usize {
        self.max_payload
    }

    fn is_alive(&self) -> bool {
        !self.closed
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    /// Poison both inner mutexes via a panic inside a held guard, then
    /// confirm that `lock_log()` / `lock_fail_mode()` recover and that
    /// `send_bytes` continues to work and the panic-time mutation (the
    /// `BrokenForN` state we set before the panic) is still visible.
    #[test]
    fn mutex_recovers_from_poisoning() {
        let mut t = MockTransport::new(1316);

        // Poison fail_mode by panicking while holding the guard. Mutate
        // it first so we can verify the mutation survives the recovery.
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let mut mode = t.lock_fail_mode();
            *mode = FailMode::BrokenForN(2);
            panic!("intentional panic inside lock_fail_mode scope");
        }));
        assert!(result.is_err(), "expected catch_unwind to capture panic");
        assert!(t.fail_mode.is_poisoned(), "fail_mode should be poisoned");

        // Poison log the same way, with a mutation in place.
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let mut log = t.lock_log();
            log.push(b"pre-poison entry".to_vec());
            panic!("intentional panic inside lock_log scope");
        }));
        assert!(result.is_err(), "expected catch_unwind to capture panic");
        assert!(t.log.is_poisoned(), "log should be poisoned");

        // Subsequent `send_bytes` must not panic. The first two sends
        // should hit `BrokenForN(2)` and decrement; the third should
        // succeed and append to the (already-poisoned) log.
        assert!(matches!(
            t.send_bytes(b"first"),
            Err(TransportError::Broken { .. })
        ));
        assert!(matches!(
            t.send_bytes(b"second"),
            Err(TransportError::Broken { .. })
        ));
        assert!(t.send_bytes(b"third").is_ok());

        // Verify the pre-poison mutation is still visible (recovery via
        // `into_inner` returns the inner state intact).
        let log = t.lock_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], b"pre-poison entry");
        assert_eq!(log[1], b"third");
    }
}
