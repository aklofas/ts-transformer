//! Check 4 — `no_std` `DemuxReceiver<LoopbackRecvTransport>` recovers the
//! same video AU(s) that a `MuxSender<SmoltcpUdpTransport>` send path put on
//! the wire. This is the runtime proof for the receiver-path `no_std` work
//! (`Receiver`/`DemuxReceiver` under `--no-default-features`): a real
//! `RecvTransport` impl, driven through `DemuxReceiver::recv_event`, over a
//! real smoltcp UDP/IP stack — the mirror image of check 3's send-side
//! proof, one layer up the pipeline shell stack.
//!
//! # Why this check pushes 3 AUs, not check 3's single-AU golden
//!
//! `DemuxReceiver`'s inner `Receiver` recovers TS packet alignment via a
//! byte-level `Syncer` that requires **4 confirming `0x47` sync bytes 188
//! bytes apart** before it trusts the alignment and starts emitting packets
//! (`crates/tst-pipeline/src/receiver/sync.rs` — a deliberate anti-false-lock
//! heuristic mirroring libavformat's `mpegts_resync`). Check 3's golden is
//! exactly 3 TS packets (PAT + PMT + one video PES) — one confirming sync
//! byte short of the 4 the syncer needs, so it never leaves HUNT/VERIFY and
//! `DemuxReceiver` would sit at EOF having fed nothing to the demuxer at
//! all. This was verified empirically (a host-side scratch harness against
//! `DemuxReceiver` directly): 1-2 AU pushes never lock sync, 3 pushes (5 TS
//! packets, comfortably over the 4-confirmation threshold) locks reliably
//! and every AU decodes byte-exact. Per the module docs' closed-loop note,
//! this check is about proving the receiver *machinery* runs correctly, not
//! about matching check 1/3's pinned golden byte-for-byte — so building a
//! dedicated (still real, still wire-serialized) dataset sized for a real
//! `Receiver` is squarely in scope, not a workaround around the golden.
//!
//! Closed-loop note (deliberate, per spec): the wire *format* is already
//! pinned by check 1's hand-verified golden byte-match. This check proves
//! the no_std receiver *machinery* (transport trait wiring, `DemuxReceiver`
//! event loop, `SamplePayload` extraction) actually runs correctly on
//! target — not TS conformance, which check 1 already covers.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::phy::Loopback;
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpEndpoint};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{BrokenCause, RecvTransport, TransportError};
use tst_pipeline::{DemuxReceiver, MuxSender, ShellErrorKind};

/// Number of `synthetic_h264_idr()` AUs check 4 pushes on the send side —
/// see the module docs above for why 1 (check 3's golden) isn't enough for
/// `Receiver`'s sync-lock heuristic. 3 AUs (5 TS packets total: PAT + PMT +
/// 3×video) clears the 4-confirmation threshold with one packet of margin.
pub const CHECK4_AU_PUSHES: usize = 3;

/// Send `CHECK4_AU_PUSHES` copies of `synthetic_h264_idr()` through a fresh
/// `MuxSender<SmoltcpUdpTransport>` — the same send-side machinery check 3
/// exercises, reusing `crate::new_loopback_udp_stack` via
/// `SmoltcpUdpTransport::new` — and return the captured wire datagrams.
fn build_check4_datagrams() -> Vec<Vec<u8>> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let acc = Arc::new(Mutex::new(Vec::new()));
    let transport = crate::SmoltcpUdpTransport::new(Arc::clone(&acc));
    let sender = MuxSender::new(transport, cfg).expect("mux sender");
    for i in 0..CHECK4_AU_PUSHES {
        sender
            .send_video(
                &crate::synthetic_h264_idr(),
                Pts90khz::new(i as i64 * 3000),
                /*key_frame=*/ true,
            )
            .expect("send_video");
    }
    sender.close();
    // `let` binding drops the MutexGuard at end-of-statement before `acc`
    // drops, same rationale as check 3's `mux_sender_over_udp_loopback`.
    let out = acc.lock().clone();
    out
}

/// `no_std` `RecvTransport` over a smoltcp loopback UDP socket. `to_send`
/// holds the datagrams captured on the send side, fed onto the wire one at
/// a time as TX room allows; `remaining` is the count not yet delivered to
/// the caller.
///
/// Each `recv_bytes` call: (1) injects the next not-yet-sent datagram if the
/// socket has TX room — `Loopback` echoes every transmitted frame straight
/// back as an inbound one, so this is how the previously wire-captured bytes
/// re-enter a fresh smoltcp stack for step (3) to observe; (2) polls the
/// interface once; (3) tries to drain one datagram. `Backpressure` when
/// nothing has looped back yet (the caller retries — the first datagram
/// typically needs several retries while the loopback echo settles, the
/// same multi-pass behavior check 3's `SmoltcpUdpTransport::send_bytes`
/// polls 16 times per send to absorb). `Closed` once every datagram from the
/// send side has been delivered — that's what drives `DemuxReceiver`'s
/// EOF/flush path.
struct LoopbackRecvTransport {
    device: Loopback,
    iface: Interface,
    sockets: SocketSet<'static>,
    handle: SocketHandle,
    clock_ms: i64,
    endpoint: IpEndpoint,
    to_send: VecDeque<Vec<u8>>,
    remaining: usize,
}

impl LoopbackRecvTransport {
    /// `datagrams` is the send-side's exact wire capture — the bytes a live
    /// sender would have put on the wire, fed back in here as the "remote"
    /// side of this receive-path proof.
    fn new(datagrams: Vec<Vec<u8>>) -> Self {
        let (device, iface, sockets, handle) = crate::new_loopback_udp_stack();
        let remaining = datagrams.len();
        Self {
            device,
            iface,
            sockets,
            handle,
            clock_ms: 0,
            endpoint: IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), crate::LOOPBACK_PORT),
            to_send: datagrams.into(),
            remaining,
        }
    }
}

impl RecvTransport for LoopbackRecvTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.remaining == 0 {
            return Err(TransportError::Closed);
        }

        // Step 1: inject the next not-yet-sent datagram if there's TX room.
        // Peek (don't pop) the front of the queue: only remove it once
        // `send_slice` actually succeeds, so a send failure leaves the
        // datagram in `to_send` for a future retry instead of losing it
        // (a lost datagram here would mean `remaining` never reaches 0 and
        // the caller loops forever).
        if !self.to_send.is_empty() {
            let can_send = self.sockets.get_mut::<udp::Socket>(self.handle).can_send();
            if can_send {
                let next = self.to_send.front().expect("checked non-empty above");
                let send_result = self
                    .sockets
                    .get_mut::<udp::Socket>(self.handle)
                    .send_slice(next, self.endpoint);
                match send_result {
                    Ok(()) => {
                        self.to_send.pop_front();
                    }
                    Err(_) => {
                        return Err(TransportError::Broken {
                            msg: String::from("udp loopback recv: send_slice failed"),
                            errno_code: None, cause: BrokenCause::Unspecified,
                        });
                    }
                }
            }
        }

        // Step 2: advance the manual clock and poll once.
        self.clock_ms += 1;
        self.iface.poll(
            Instant::from_millis(self.clock_ms),
            &mut self.device,
            &mut self.sockets,
        );

        // Step 3: try to drain one datagram.
        let socket = self.sockets.get_mut::<udp::Socket>(self.handle);
        if !socket.can_recv() {
            return Err(TransportError::Backpressure {
                msg: String::from("udp loopback recv: no datagram ready yet"),
                errno_code: None,
            });
        }
        match socket.recv_slice(buf) {
            Ok((n, _meta)) => {
                self.remaining -= 1;
                Ok(n)
            }
            // `can_recv()` just returned true, so a datagram IS queued — a
            // failure here (e.g. `buf` too small, truncation) is a genuine
            // wire-level error, not "nothing ready yet". Map to `Broken`
            // (terminal) rather than `Backpressure` (retryable): treating
            // it as retryable risks a stuck loop if the failed call already
            // dequeued the datagram (`remaining` would never reach 0).
            Err(_) => Err(TransportError::Broken {
                msg: String::from("udp loopback recv: recv_slice failed after can_recv() was true"),
                errno_code: None, cause: BrokenCause::Unspecified,
            }),
        }
    }

    fn max_payload(&self) -> usize {
        // loopback MTU ceiling: a safe upper bound on any datagram the send
        // side can produce (`SmoltcpUdpTransport::max_payload()` is 1316).
        1500
    }

    fn is_alive(&self) -> bool {
        self.remaining > 0
    }
}

/// Run check 4 end to end: build the dedicated send-side dataset (see the
/// module docs for why it's 3 AUs, not check 3's single-AU golden), wrap a
/// fresh `LoopbackRecvTransport` fed with those datagrams in a `no_std`
/// `DemuxReceiver`, drain every event, and return the concatenated video-AU
/// payload bytes recovered from the demuxed `DemuxEvent::Sample` stream.
/// Non-video events (`ProgramMap`, etc.) are ignored — this check is about
/// the receiver machinery running correctly on target, not TS conformance
/// (see the module docs above). The caller compares the result against
/// `synthetic_h264_idr()` repeated `CHECK4_AU_PUSHES` times.
///
/// `Backpressure` from `recv_event()` means "poll again", not failure, so
/// the loop retries it. The `spins` bound only guards against a stuck
/// transport hanging the QEMU run forever (60 s CI timeout) — a healthy
/// loopback drains in a handful of polls per datagram, far under this.
pub fn udp_recv_check() -> Vec<u8> {
    let datagrams = build_check4_datagrams();
    let transport = LoopbackRecvTransport::new(datagrams);
    let mut rx = DemuxReceiver::new(transport);
    let mut collected = Vec::new();
    let mut spins: u32 = 0;
    loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => collected.extend_from_slice(&raw),
            Ok(Some(_other_event)) => {} // PAT/PMT etc. — not this check's concern
            Ok(None) => break,           // clean EOF: demuxer flushed, queue drained
            Err(e) if e.kind == ShellErrorKind::Backpressure => {
                spins += 1;
                assert!(
                    spins < 10_000,
                    "udp_recv: stuck retrying Backpressure — loopback never delivered"
                );
            }
            Err(e) => panic!("udp_recv: DemuxReceiver error: {e}"),
        }
    }
    collected
}
