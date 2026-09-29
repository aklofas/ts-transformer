//! `ReconnectMode::Blocking`: a message whose `send_bytes` returned an error
//! has exactly one owner — the caller.
//!
//! The initial inner send reports `Broken`, the message is enqueued in the
//! gap buffer, the factory succeeds, and the drain of the fresh inner reports
//! `Backpressure`. `Transport::send_bytes` documents that error as "the bytes
//! have NOT been partially consumed; callers may retry the identical slice",
//! and the sender shells retain and re-offer on their own. A copy left behind
//! in the gap buffer would reach the wire as well, so every test here counts
//! what the wire carried.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{BrokenCause, Transport, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, MuxSender, OverflowPolicy, ReconnectMode, ReconnectPolicy,
    Sender, SenderConfig, ShellErrorKind,
};

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Backpressure,
    Broken,
}

/// Script-driven transport. An empty script means the send succeeds and the
/// slice is appended to the shared wire log.
struct Scripted {
    script: Arc<Mutex<VecDeque<Outcome>>>,
    wire: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: bool,
}

impl Transport for Scripted {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if !self.alive {
            return Err(TransportError::Broken {
                msg: "dead".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        match self.script.lock().unwrap().pop_front() {
            None => {
                self.wire.lock().unwrap().push(msg.to_vec());
                Ok(())
            }
            Some(Outcome::Backpressure) => Err(TransportError::Backpressure {
                msg: "scripted backpressure".into(),
                errno_code: None,
            }),
            Some(Outcome::Broken) => {
                self.alive = false;
                Err(TransportError::Broken {
                    msg: "scripted break".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
        }
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        self.alive
    }
    fn close(&mut self) {
        self.alive = false;
    }
}

struct Rig {
    wire: Arc<Mutex<Vec<Vec<u8>>>>,
    factory_calls: Arc<AtomicU32>,
}

/// Initial inner: first send breaks. Fresh inner (one factory call expected):
/// first send reports `Backpressure`, everything after succeeds.
fn managed_break_then_backpressure() -> (ManagedTransport<Scripted>, Rig) {
    let wire = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::new(AtomicU32::new(0));
    let initial = Scripted {
        script: Arc::new(Mutex::new(VecDeque::from([Outcome::Broken]))),
        wire: Arc::clone(&wire),
        alive: true,
    };
    let fresh_script = Arc::new(Mutex::new(VecDeque::from([Outcome::Backpressure])));
    let factory = {
        let wire = Arc::clone(&wire);
        let calls = Arc::clone(&factory_calls);
        move || -> Result<Scripted, TransportError> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Scripted {
                // Shared across rebuilds: the single Backpressure is consumed
                // once, whichever fresh inner sees it.
                script: Arc::clone(&fresh_script),
                wire: Arc::clone(&wire),
                alive: true,
            })
        }
    };
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 64,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Blocking,
    };
    (
        ManagedTransport::new(initial, factory, policy),
        Rig {
            wire,
            factory_calls,
        },
    )
}

/// A bare caller following the `Backpressure` contract.
#[test]
fn blocking_backpressure_retry_delivers_once() {
    let (mut managed, rig) = managed_break_then_backpressure();
    let stats = managed.stats_handle();

    let first = managed.send_bytes(b"A");
    assert!(
        matches!(first, Err(TransportError::Backpressure { .. })),
        "precondition: the post-reconnect drain backpressure surfaces to the caller, got {first:?}"
    );
    assert_eq!(
        rig.factory_calls.load(Ordering::SeqCst),
        1,
        "precondition: exactly one successful reconnect"
    );
    assert!(
        rig.wire.lock().unwrap().is_empty(),
        "precondition: nothing on the wire yet"
    );
    let s = stats.stats().expect("gap lock not poisoned");
    assert_eq!(s.gap_len, 0, "the refused message is the caller's again");
    assert_eq!(
        (s.gap_messages_dropped, s.gap_bytes_dropped),
        (0, 0),
        "handing a message back to the caller is not a drop"
    );

    // `Backpressure` contract: the caller may retry the identical slice.
    let retry = managed.send_bytes(b"A");
    assert!(retry.is_ok(), "the retry succeeds, got {retry:?}");

    let wire = rig.wire.lock().unwrap().clone();
    assert_eq!(
        wire,
        vec![b"A".to_vec()],
        "message A must reach the wire exactly once after a Backpressure retry"
    );
}

/// Only THIS call's message is handed back. Messages queued by earlier calls
/// stay queued and reach the wire first, and a `DropOldest` eviction made to
/// fit the message that was later handed back is still a real, counted drop.
#[test]
fn blocking_backpressure_hands_back_only_this_calls_message() {
    let wire = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::new(AtomicU32::new(0));
    let initial = Scripted {
        script: Arc::new(Mutex::new(VecDeque::from([Outcome::Broken]))),
        wire: Arc::clone(&wire),
        alive: true,
    };
    let factory = {
        let wire = Arc::clone(&wire);
        let calls = Arc::clone(&factory_calls);
        move || -> Result<Scripted, TransportError> {
            // Down for the first two sends, back for the third.
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(TransportError::Broken {
                    msg: "factory down".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
            Ok(Scripted {
                script: Arc::new(Mutex::new(VecDeque::from([Outcome::Backpressure]))),
                wire: Arc::clone(&wire),
                alive: true,
            })
        }
    };
    let policy = ReconnectPolicy {
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 2,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Blocking,
    };
    let mut managed = ManagedTransport::new(initial, factory, policy);
    let stats = managed.stats_handle();

    // Two sends across the outage: each gives up and stays queued.
    for m in [b"1", b"2"] {
        let r = managed.send_bytes(m);
        assert!(
            matches!(r, Err(TransportError::Broken { .. })),
            "precondition: the reconnect budget runs out, got {r:?}"
        );
    }
    assert_eq!(stats.stats().expect("not poisoned").gap_len, 2);

    // Third send: evicts "1" to fit, reconnects, and the drain of "2" is
    // refused by the fresh inner.
    let r = managed.send_bytes(b"3");
    assert!(
        matches!(r, Err(TransportError::Backpressure { .. })),
        "precondition: the drain backpressure surfaces, got {r:?}"
    );
    let s = stats.stats().expect("not poisoned");
    assert_eq!(s.gap_len, 1, "\"2\" stays queued; \"3\" went back");
    assert_eq!(
        (s.gap_messages_dropped, s.gap_bytes_dropped),
        (1, 1),
        "exactly the eviction of \"1\" is counted"
    );

    managed.send_bytes(b"3").expect("the retry succeeds");
    assert_eq!(
        *wire.lock().unwrap(),
        vec![b"2".to_vec(), b"3".to_vec()],
        "the earlier message first, then the retried one, each once"
    );
}

/// One 7-packet bundle of TS, tagged so bundles are distinguishable.
fn ts_bundle(tag: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 * 188);
    for i in 0..7u8 {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x01;
        pkt[2] = 0x00;
        pkt[3] = 0x10 | (i & 0x0F);
        pkt[4] = tag;
        out.extend_from_slice(&pkt);
    }
    out
}

/// Composition through `Sender`: the shell retains the bundle the transport
/// refused (`input_consumed == Some(true)`) and re-offers it on the next
/// call — the shell's OWN documented retry, no caller resend involved.
#[test]
fn sender_over_blocking_managed_delivers_each_bundle_once() {
    let (managed, rig) = managed_break_then_backpressure();
    let mut sender = Sender::new(managed, SenderConfig::default());

    let a = ts_bundle(0xA1);
    let err = sender
        .send_ts(&a)
        .expect_err("precondition: first send_ts surfaces the drain backpressure");
    assert_eq!(err.kind, ShellErrorKind::Backpressure, "got {err:?}");
    assert_eq!(
        err.input_consumed,
        Some(true),
        "precondition: the shell retained the bundle, the caller must NOT resend"
    );

    // The caller obeys the contract: it does not resend A. It flushes (which
    // drains the shell's retained bundle).
    sender.flush().expect("flush after backpressure");

    let wire = rig.wire.lock().unwrap().clone();
    assert_eq!(
        wire.len(),
        1,
        "bundle A must reach the wire exactly once; wire carried {} messages \
         (identical to A: {})",
        wire.len(),
        wire.iter().filter(|m| **m == a).count()
    );
    assert_eq!(wire[0], a);
}

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<Vec<u8>>>>);
impl Transport for Sink {
    fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
        self.0.lock().unwrap().push(b.to_vec());
        Ok(())
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn close(&mut self) {}
    fn is_alive(&self) -> bool {
        true
    }
}

fn mux_config() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().expect("valid muxer config")
}

fn idr(seed: u8) -> Vec<u8> {
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    for i in 0u8..15 {
        buf.push(seed ^ i);
    }
    buf
}

/// Composition through `MuxSender`: the wire must carry exactly the chunks a
/// fault-free transport carries for the same two access units.
#[test]
fn mux_sender_over_blocking_managed_delivers_each_chunk_once() {
    // Reference: same input over a transport that never fails.
    let reference = Arc::new(Mutex::new(Vec::new()));
    {
        let s = MuxSender::new(Sink(Arc::clone(&reference)), mux_config()).expect("mux sender");
        s.send_video(&idr(0xA5), Pts90khz::new(0), true).unwrap();
        s.send_video(&idr(0x5A), Pts90khz::new(3000), true).unwrap();
        s.finish().unwrap();
    }
    let reference = reference.lock().unwrap().clone();
    assert!(!reference.is_empty());

    let (managed, rig) = managed_break_then_backpressure();
    let s = MuxSender::new(managed, mux_config()).expect("mux sender");
    let err = s
        .send_video(&idr(0xA5), Pts90khz::new(0), true)
        .expect_err("precondition: first send_video surfaces the drain backpressure");
    assert_eq!(err.kind, ShellErrorKind::Backpressure, "got {err:?}");
    assert_eq!(
        err.input_consumed,
        Some(true),
        "precondition: muxed and retained, the caller must NOT resend"
    );
    s.send_video(&idr(0x5A), Pts90khz::new(3000), true)
        .expect("second send_video");
    s.finish().expect("finish");

    let wire = rig.wire.lock().unwrap().clone();
    assert_eq!(
        wire.len(),
        reference.len(),
        "wire carried {} chunks, the fault-free reference carries {}",
        wire.len(),
        reference.len()
    );
    assert_eq!(wire, reference, "wire chunks differ from the reference");
}
