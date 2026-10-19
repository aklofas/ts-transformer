//! Implementing the `Transport` trait for a non-SRT sink.
//!
//! Defines `MemTransport` — collects every TS packet into a `Vec<Vec<u8>>` —
//! and runs `MuxSender` against it. At the end, dumps the collected bytes to a
//! file. Validates the muxer's output without any networking.
//!
//!   cargo run -p tst-examples --example custom_transport -- /tmp/custom_transport_out.ts
//!
//! Demonstrates: `Transport` trait, `MuxSender` is generic over T: Transport.

use std::env;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::{MuxSender, Transport, TransportError};

// ---------------------------------------------------------------------------
// In-memory transport: every `send_bytes` call appends to `packets`.
//
// Why `Arc<Mutex<...>>` for *both* fields, not just one:
//
// We hand out a clone of the transport via `transport.clone()` so the main
// thread can hold a `collector` view while the `MuxSender` consumes the
// original. Both views must read and write the *same* underlying packet
// vec — `Arc` is what gives them shared ownership, and the `Mutex` is
// what makes that ownership thread-safe across the `MuxSender` boundary
// (the `Transport` trait requires `Send`, and a future async or
// threaded sender shell could legitimately call `send_bytes` from
// another thread). For `alive` the same reasoning applies: `close()`
// flips the flag, and `is_alive()` (called from any view, including
// `ManagedTransport`'s reconnect poll if it were wrapping this) needs
// to see the update.
//
// `max_payload: 1316` deliberately matches `SrtTransport::DEFAULT_PAYLOAD`,
// so `MemTransport` accepts the same message sizes a real SRT socket
// would. Aligning the cap means the muxer's chunking layer produces
// identical output regardless of whether the bytes go to libsrt or to
// this in-memory collector — useful when the example doubles as a
// regression check on muxer output.
// ---------------------------------------------------------------------------
#[derive(Clone)]
struct MemTransport {
    packets: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: Arc<Mutex<bool>>,
    max_payload: usize,
}

impl MemTransport {
    fn new() -> Self {
        Self {
            packets: Arc::new(Mutex::new(Vec::new())),
            alive: Arc::new(Mutex::new(true)),
            max_payload: 1316,
        }
    }

    // Concatenate every collected per-message frame into one contiguous TS
    // byte stream. Pre-allocates the output `Vec` with the exact final size
    // (sum of every frame's length) so the `extend_from_slice` calls don't
    // trigger any growth-and-copy reallocations.
    //
    // This method exists for the example only — a real network transport
    // (UDP, etc.) wouldn't keep the bytes around to dump them later;
    // it would write them straight out to the wire from `send_bytes`.
    fn into_bytes(self) -> Vec<u8> {
        let pkts = self.packets.lock().unwrap();
        let mut out = Vec::with_capacity(pkts.iter().map(|p| p.len()).sum());
        for p in pkts.iter() {
            out.extend_from_slice(p);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// `Transport` trait impl — the contract `MuxSender`/`Sender`/`RawSender` (and
// `ManagedTransport`) expect. Four methods, each one a small, well-defined
// hook into the byte-sink lifecycle:
//
//   send_bytes  — write exactly one outbound message (one TS chunk in our
//                 muxer's case). This is where a real transport would do
//                 the actual I/O — `socket.send(bytes)` for UDP, a
//                 `File::write_all` for file output, a pipe write, etc.
//                 For us it's just a `Vec` push.
//   max_payload — reports the maximum size of any single message the
//                 transport will accept. The sender shells use this to
//                 size their chunking buffers and to validate raw-sender
//                 input before handing bytes off.
//   is_alive    — advisory liveness check. `ManagedTransport` polls this
//                 on the reconnect path; plain senders don't have to.
//   close       — one-way state transition into "dead." After `close`,
//                 `send_bytes` returns `TransportError::Closed`. The
//                 transition is guarded by the `alive` mutex, so
//                 double-close (and concurrent close + send) is safe —
//                 the second close just rewrites `false` over `false`.
// ---------------------------------------------------------------------------
impl Transport for MemTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        // Size check first — the contract says oversized messages get a
        // `TooLarge` error rather than a partial send. Mirrors what a real
        // SRT live-mode socket would do when handed a payload larger than
        // `SRTO_PAYLOADSIZE`.
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.max_payload,
            });
        }
        // Aliveness check — once `close` has been called, no further sends
        // are accepted. The `Closed` variant is distinct from `Broken`
        // (which means "rebuild me") because `Closed` is terminal: the
        // caller asked for shutdown, there's no rebuild story.
        if !*self.alive.lock().unwrap() {
            return Err(TransportError::Closed);
        }
        // The actual "send" — for an in-memory collector this is just a
        // push onto the shared packet vec. Real transports do their I/O
        // here: UDP `send`, file `write_all`, pipe write, etc.
        self.packets.lock().unwrap().push(msg.to_vec());
        Ok(())
    }

    fn max_payload(&self) -> usize {
        self.max_payload
    }

    // Returns `true` until `close` flips the flag. After that, `false`.
    fn is_alive(&self) -> bool {
        *self.alive.lock().unwrap()
    }

    // One-way transition into "closed." Idempotent — calling close twice
    // just rewrites `false` over `false`.
    fn close(&mut self) {
        *self.alive.lock().unwrap() = false;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to a cross-platform temp path when no argv path is supplied.
    let out_path = env::args().nth(1).unwrap_or_else(|| {
        env::temp_dir()
            .join("custom_transport_out.ts")
            .to_string_lossy()
            .into_owned()
    });

    // `transport` is the original we'll hand into `MuxSender`; `collector` is
    // the `transport.clone()` clone the main thread keeps to read out the
    // collected bytes after the sender has finished. Both views point at
    // the *same* `Arc<Mutex<Vec<Vec<u8>>>>` packet store — that's the
    // whole point of the `Arc<Mutex<...>>` wrapping above.
    let transport = MemTransport::new();
    let collector = transport.clone();

    // The canonical sender shell. `MuxSender` composes the muxer
    // (`MuxerConfig::default`) with the transport. End-to-end the path is
    // NAL+KLV → mux → 188-byte TS packets → MemTransport's packet vec.
    let sender = MuxSender::new(transport, MuxerConfig::default())?;

    // 10 frames is shorter than `managed_reconnect`'s 30 — there's no
    // reconnect machinery to exercise, so we can keep the run small and
    // self-contained. At 30 fps this is one-third of a second of "video"
    // wall time; the example is a smoke test, not a throughput demo.
    for i in 0..10i64 {
        // 90 kHz TS clock. 90000 Hz / 30 fps = 3000 ticks per frame, so
        // `i * 3000` advances PTS at exactly 30 fps cadence.
        let pts = i * 3000;
        let nal = synthetic_nal_au(800);
        let klv = synthetic_klv(50, i);
        // `key_frame: i == 0` — the first frame is the IDR; subsequent
        // frames are non-IDR. The synthetic NAL is tagged accordingly
        // (see `synthetic_nal_au`).
        sender.send_video(&nal, Pts90khz::new(pts), i == 0)?;
        // `metadata_service_id` goes into the AU cell header per H.222.0
        // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
        // streams (stream_type 0x15); silently ignored for PrivateData
        // streams (0x06) like the one configured above. The spec default is
        // 0x00 — use a non-zero value only when mirroring a metadata_klva()
        // PMT descriptor `service_id` you supplied at config time.
        sender.send_klv(&klv, Pts90khz::new(pts), 0x00)?;
    }
    // `close` flushes any pending TS bytes to the transport before we
    // collect — without this, the tail of the muxer's output would be
    // stuck inside the sender's internal buffer and missing from the
    // captured byte stream.
    sender.close();

    // Drain the collector. After `into_bytes` the `MemTransport` clone is
    // consumed; the original (now owned by the closed `MuxSender`) is fine
    // to drop.
    let bytes = collector.into_bytes();
    // 188 is the canonical TS packet size — printing the count helps
    // verify the muxer's output makes sense (10 frames × ~5 packets each
    // ≈ 50 packets minimum).
    println!(
        "collected {} bytes ({} TS packets) into {out_path}",
        bytes.len(),
        bytes.len() / 188
    );
    // Save the in-memory bytes to disk so the reader can `file out.ts`
    // and confirm libmagic recognizes it as a real MPEG transport
    // stream.
    File::create(&out_path)?.write_all(&bytes)?;
    Ok(())
}

// Synthetic H.264 access unit. The muxer doesn't parse NAL contents — it
// just wraps whatever bytes you give it in PES packets — but a real-looking
// AU helps if you tcpdump the output and load it in a tool that *does* parse.
//
// Layout:
//   0x00 0x00 0x00 0x01   Annex-B 4-byte start code
//   0x65                  NAL header byte for nal_unit_type=5 (IDR /
//                         coded slice of an IDR picture) with
//                         nal_ref_idc=0b11 (highest priority)
//   0xAA × n              filler payload (arbitrary; the muxer is
//                         opaque to NAL contents)
fn synthetic_nal_au(n: usize) -> Vec<u8> {
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    buf.extend(std::iter::repeat_n(0xAA, n));
    buf
}

// Synthetic KLV blob. The 16-byte prefix is the canonical ST 0601 UAS
// Datalink LS key (`UniversalLabel::ST_0601_LS`, MISB ST 0601 §6) —
// fine to hardcode here because the muxer is opaque to KLV contents
// (no parsing on the send path); it just wraps the blob in a
// metadata-stream PES and emits it. Real ST 0601 KLV is built via
// `tst_core::klv::st0601` (see the `klv_encode_minimal` example).
//
// `buf.push(n as u8)` is a BER short-form length byte, valid for n < 128
// (the high bit reserved for long-form indicator). This example keeps n
// well under that bound.
fn synthetic_klv(n: usize, seq: i64) -> Vec<u8> {
    let mut buf = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    buf.push(n as u8);
    buf.extend(std::iter::repeat_n(seq as u8, n));
    buf
}
