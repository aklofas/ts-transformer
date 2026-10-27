//! `ReconnectMode::Blocking`: the stats observer answers while the replay of
//! the gap buffer into a fresh inner is parked in the inner send.
//!
//! The replay runs on the caller's thread with the transport lock held. The
//! gap-buffer lock, the only one `ManagedStatsHandle::stats` takes, must not
//! be held across that send, or a sink that stops draining stalls every
//! observer with it.

use crate::common::{ScriptedSend, SendStep, fast_policy, wait_for};

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tst_core::transport::{Transport, TransportError};
use tst_pipeline::ManagedTransport;
use tst_test_helpers::timing::scaled;

/// A `ScriptedSend` that, when armed, parks its first send until the test
/// releases it. `parked` goes up once the send is parked.
struct ParkOnce {
    inner: ScriptedSend,
    park: Option<Park>,
}

struct Park {
    parked: Arc<AtomicBool>,
    release: Arc<Mutex<Receiver<()>>>,
}

impl Transport for ParkOnce {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if let Some(park) = self.park.take() {
            park.parked.store(true, SeqCst);
            // A dropped sender (the test failed first) releases it too.
            let _ = park.release.lock().unwrap().recv();
        }
        self.inner.send_bytes(msg)
    }
    fn max_payload(&self) -> usize {
        self.inner.max_payload()
    }
    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

/// The inner the factory builds parks its first send until released.
#[test]
fn stats_return_while_a_blocking_replay_is_parked() {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let parked = Arc::new(AtomicBool::new(false));
    let first = ParkOnce {
        inner: ScriptedSend::scripted(
            Arc::new(Mutex::new(VecDeque::from([SendStep::Broken]))),
            Arc::default(),
        ),
        park: None,
    };
    let factory = {
        let parked = Arc::clone(&parked);
        let release = Arc::new(Mutex::new(release_rx));
        move || -> Result<ParkOnce, TransportError> {
            Ok(ParkOnce {
                inner: ScriptedSend::ok(),
                park: Some(Park {
                    parked: Arc::clone(&parked),
                    release: Arc::clone(&release),
                }),
            })
        }
    };
    let managed = ManagedTransport::new(first, factory, fast_policy(Some(3)));
    let stats = managed.stats_handle();
    let mut sender = managed;
    let send = std::thread::spawn(move || sender.send_bytes(&[0x47; 188]));
    assert!(wait_for(scaled(Duration::from_secs(5)), || parked.load(SeqCst)));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(stats.stats());
    });
    let snap = rx
        .recv_timeout(scaled(Duration::from_secs(2)))
        .expect("stats() must not wait for the parked send");
    assert_eq!(
        snap.unwrap().gap_len,
        1,
        "the in-flight message is still queued"
    );
    release_tx.send(()).unwrap();
    send.join().unwrap().unwrap();
}
