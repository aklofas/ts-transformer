//! Shared scaffolding for the tst-pipeline integration tests: in-memory
//! transports, cancel flags, reconnect policies and TS byte helpers.
//!
//! Each test binary declares `mod common;` once at its root and reaches the
//! items as `crate::common::…`. The file is `tests/common/mod.rs`, not
//! `tests/common.rs`, because Cargo compiles every `tests/*.rs` as its own
//! test binary. A helper belongs here when two or more test files in this
//! crate need it; it moves to `tst-test-helpers` only when another crate
//! does.

#![allow(dead_code)] // each binary uses a subset

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tst_core::mpegts::mux::Muxer;
use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportCancel, TransportError};
use tst_pipeline::{BackoffStrategy, ReconnectPolicy};

/// `TransportError::Broken` with no errno and an unspecified cause — the
/// shape every mock uses for "the link died".
pub fn broken(msg: &str) -> TransportError {
    TransportError::Broken {
        msg: msg.into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    }
}

pub fn flag(set: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(set))
}

/// A cancel handle that is nothing but a shared flag.
pub struct FlagCancel(pub Arc<AtomicBool>);

impl TransportCancel for FlagCancel {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Reconnect immediately (zero backoff), `max_attempts` times; every other
/// field at its default.
pub fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    }
}

/// Latch-and-poll with a bounded watchdog — never a wall-clock assert.
/// Returns whether `f` held before `deadline` (checked once more at the end).
pub fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    f()
}

/// Minimal valid Annex-B H.264 AU (AUD + IDR slice), 14 bytes. `marker`
/// is the byte after the slice NAL header, so each AU is distinguishable.
pub fn build_h264_au(marker: u8) -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, marker, 0xBB, 0xCC,
    ]
}

/// Tiny ST 0601 local set: UL + BER length + a 4-byte body. It does not
/// decode; the pairer and the muxer treat it as opaque bytes.
pub fn dummy_klv() -> Vec<u8> {
    let mut v = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    v.push(4); // BER short-form length
    v.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    v
}

/// Pull everything the muxer has queued, as one contiguous TS byte run.
pub fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Split packet-aligned muxer output into 188-byte packets.
pub fn to_ts_packets(bytes: &[u8]) -> Vec<[u8; 188]> {
    assert_eq!(
        bytes.len() % 188,
        0,
        "muxer output must be TS-packet aligned"
    );
    bytes
        .chunks_exact(188)
        .map(|c| {
            let mut a = [0u8; 188];
            a.copy_from_slice(c);
            a
        })
        .collect()
}

/// Replays `chunks` one per `recv_bytes` (truncated to the caller's
/// buffer), then returns `on_exhaust` on every later call. `is_alive` is
/// `alive` when set, otherwise "chunks remain". `recvs` counts calls.
pub struct ScriptedRecv {
    pub chunks: VecDeque<Vec<u8>>,
    pub on_exhaust: TransportError,
    pub max_payload: usize,
    pub alive: Option<bool>,
    pub recvs: Arc<AtomicU32>,
}

impl ScriptedRecv {
    pub fn new(chunks: impl IntoIterator<Item = Vec<u8>>, on_exhaust: TransportError) -> Self {
        Self {
            chunks: chunks.into_iter().collect(),
            on_exhaust,
            max_payload: 1316,
            alive: None,
            recvs: Arc::default(),
        }
    }

    /// One 188-byte TS packet per call; the ceiling is one packet.
    pub fn packets(packets: Vec<[u8; 188]>, on_exhaust: TransportError) -> Self {
        Self::new(packets.into_iter().map(|p| p.to_vec()), on_exhaust).with_max_payload(188)
    }

    /// Nothing to deliver: every call returns `err`; not alive.
    pub fn failing(err: TransportError) -> Self {
        Self::new(Vec::<Vec<u8>>::new(), err)
    }

    pub fn with_max_payload(mut self, n: usize) -> Self {
        self.max_payload = n;
        self
    }

    pub fn with_alive(mut self, alive: bool) -> Self {
        self.alive = Some(alive);
        self
    }

    pub fn with_counter(mut self, recvs: Arc<AtomicU32>) -> Self {
        self.recvs = recvs;
        self
    }
}

impl RecvTransport for ScriptedRecv {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.recvs.fetch_add(1, Ordering::SeqCst);
        match self.chunks.pop_front() {
            Some(c) => {
                let n = c.len().min(buf.len());
                buf[..n].copy_from_slice(&c[..n]);
                Ok(n)
            }
            None => Err(self.on_exhaust.clone()),
        }
    }
    fn max_payload(&self) -> usize {
        self.max_payload
    }
    fn is_alive(&self) -> bool {
        self.alive.unwrap_or(!self.chunks.is_empty())
    }
}

/// What the next scripted send does; an empty script means `Ok`.
#[derive(Clone, Copy, Debug)]
pub enum SendStep {
    Ok,
    Backpressure,
    Broken,
    TooLarge,
}

/// A send transport driven by a shared script. In order, each send:
/// counts itself in `sends`; returns `every_send` if set; refuses a message
/// over `max_payload` as `TooLarge`; refuses with `Broken("dead")` once not
/// alive; then pops the script — `Ok` appends the message to the shared
/// `wire` log, `Broken` also marks the transport dead. The script and the
/// wire are `Arc`s so a factory's rebuilt transports share them.
#[derive(Clone)]
pub struct ScriptedSend {
    pub script: Arc<Mutex<VecDeque<SendStep>>>,
    pub wire: Arc<Mutex<Vec<Vec<u8>>>>,
    pub sends: Arc<AtomicU32>,
    pub every_send: Option<TransportError>,
    pub max_payload: usize,
    pub alive: bool,
}

impl ScriptedSend {
    /// Accepts everything into its own wire log.
    pub fn ok() -> Self {
        Self {
            script: Arc::default(),
            wire: Arc::default(),
            sends: Arc::default(),
            every_send: None,
            max_payload: 1316,
            alive: true,
        }
    }

    /// Fails every send with `err`.
    pub fn always(err: TransportError) -> Self {
        Self {
            every_send: Some(err),
            ..Self::ok()
        }
    }

    /// Follows `script`, logging accepted sends into `wire`.
    pub fn scripted(
        script: Arc<Mutex<VecDeque<SendStep>>>,
        wire: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> Self {
        Self {
            script,
            wire,
            ..Self::ok()
        }
    }

    pub fn with_max_payload(mut self, n: usize) -> Self {
        self.max_payload = n;
        self
    }

    pub fn with_alive(mut self, alive: bool) -> Self {
        self.alive = alive;
        self
    }

    pub fn with_counter(mut self, sends: Arc<AtomicU32>) -> Self {
        self.sends = sends;
        self
    }

    /// Everything accepted so far, as one byte run.
    pub fn wire_bytes(&self) -> Vec<u8> {
        self.wire.lock().unwrap().concat()
    }
}

impl Transport for ScriptedSend {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = &self.every_send {
            return Err(e.clone());
        }
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.max_payload,
            });
        }
        if !self.alive {
            return Err(broken("dead"));
        }
        let step = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SendStep::Ok);
        match step {
            SendStep::Ok => {
                self.wire.lock().unwrap().push(msg.to_vec());
                Ok(())
            }
            SendStep::Backpressure => Err(TransportError::Backpressure {
                msg: "scripted backpressure".into(),
                errno_code: None,
            }),
            SendStep::TooLarge => Err(TransportError::TooLarge {
                len: msg.len(),
                max: 0,
            }),
            SendStep::Broken => {
                self.alive = false;
                Err(broken("scripted break"))
            }
        }
    }
    fn max_payload(&self) -> usize {
        self.max_payload
    }
    fn is_alive(&self) -> bool {
        self.alive
    }
    fn close(&mut self) {
        self.alive = false;
    }
}
