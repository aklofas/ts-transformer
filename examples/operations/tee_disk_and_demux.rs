//! Record raw MPEG-TS to disk and demux typed `DemuxEvent`s in one pass.
//!
//! Why this exists: a common ISR workflow is "save the raw `.ts`
//! capture AND parse for KLV/video metadata in one pass." Two passes
//! waste I/O; two threads with the demuxer reading off a channel is more
//! code than the problem warrants. The answer is a composition, not a
//! callback: a `Receiver` hands you each aligned 188-byte packet, and you
//! send that packet to a `FileTransport` and feed it to a `Demuxer`. Your
//! own loop owns both consumers, so there is no user code running on a
//! library thread and every error is an ordinary `Result` you can handle.
//!
//! Usage: `cargo run -p tst-examples --example tee_disk_and_demux -- <input.ts> <output.ts>`
//!
//! This example uses a hand-rolled `RecvTransport` (`ChunkedFileSource`)
//! over a `.ts` file so it's runnable without a live SRT publisher. In
//! production you would plug a real `SrtRecvTransport` in instead — the
//! composition works identically. See `srt_recv_typed.rs` for the SRT side.
//!
//! What to look for in the output: the output `.ts` file is byte-for-
//! byte the input file, rounded down to a 188-byte TS packet boundary
//! (`Receiver` only emits whole, sync-locked packets). The `samples=N`
//! count tells you the demux side ran in lock-step with the disk write —
//! both observed the same packets.

use std::collections::VecDeque;
use std::env;
use std::fs;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig};
use tst_core::transport::Transport;
use tst_pipeline::ext::file_transport::FileTransport;
use tst_pipeline::{Receiver, ReceiverConfig, ShellErrorKind};
use tst_pipeline::{RecvTransport, TransportError};

/// Minimal `RecvTransport` that hands out pre-chunked byte slices and
/// returns `Closed` once the queue is empty. Useful for offline replay
/// of `.ts` captures through the same `Receiver` plumbing that
/// `SrtRecvTransport` uses live.
///
/// In a real consumer you'd use `SrtRecvTransport` instead of rolling your
/// own. This impl exists so the example is runnable without a network peer.
struct ChunkedFileSource {
    chunks: VecDeque<Vec<u8>>,
}

impl RecvTransport for ChunkedFileSource {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.chunks.pop_front() {
            Some(c) => {
                // Cap the copy at `buf.len()` even though our chunks
                // are sized to fit. `Receiver` always passes a buffer
                // sized at least `max_payload()`, but defensive
                // bookkeeping is cheap and keeps the impl honest.
                let n = c.len().min(buf.len());
                buf[..n].copy_from_slice(&c[..n]);
                Ok(n)
            }
            // Empty queue is the canonical end-of-stream signal: the
            // `Receiver` surfaces it as `ShellErrorKind::EndOfStream`, which
            // the loop below treats as the clean end.
            None => Err(TransportError::Closed),
        }
    }

    fn max_payload(&self) -> usize {
        // 1316 = the SRT live-mode default `SRTO_PAYLOADSIZE`. It's
        // not arbitrary — 1316 = 7 × 188, where 188 is the MPEG-TS
        // packet size, so SRT's payload exactly fits seven TS packets
        // with no waste. We chunk our file at the same boundary
        // below so this transport behaves like a real SRT link's
        // recv pattern.
        1316
    }

    fn is_alive(&self) -> bool {
        !self.chunks.is_empty()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: tee_disk_and_demux <input.ts> <output.ts>");
        std::process::exit(2);
    }
    let input = &args[1];
    let output = &args[2];
    let bytes = fs::read(input)?;

    // Pretend the bytes arrived in 1316-byte SRT packets. This makes
    // the offline replay match the chunk granularity that a live
    // transport would produce.
    let chunks: VecDeque<Vec<u8>> = bytes.chunks(1316).map(<[u8]>::to_vec).collect();
    let mut rx = Receiver::new(ChunkedFileSource { chunks }, ReceiverConfig::default());

    // Consumer 1: the recording. `FileTransport` is the library's
    // write-to-file `Transport`; `send_bytes` appends verbatim.
    let mut recording = FileTransport::create(output)?;
    // Consumer 2: the typed events. A bare `Demuxer` fed one aligned packet
    // at a time with `feed_aligned`.
    let mut demux = Demuxer::with_config(DemuxerConfig::default());

    let mut samples = 0usize;
    let mut metadata = 0usize;
    // One closure counts events so the in-loop drain and the final drain
    // after `flush` share a single definition.
    let mut tally = |demux: &mut Demuxer| {
        while let Some(ev) = demux.next_event() {
            match ev {
                DemuxEvent::Sample { .. } => samples += 1,
                DemuxEvent::Metadata { .. } => metadata += 1,
                _ => {}
            }
        }
    };

    // The first fatal error. It ends the loop, but the flush and the
    // recording's `finish` still run before it is returned: a terminal error
    // must not cost the last access unit, and a truncated capture must not
    // be reported as success.
    let mut failure: Option<Box<dyn std::error::Error>> = None;
    loop {
        // `next_packet` returns whole, sync-locked 188-byte packets and
        // nothing else: the syncer's alignment work is already done.
        let pkt = match rx.next_packet() {
            Ok(pkt) => pkt,
            // Peer closed cleanly: the normal end of a file replay.
            Err(e) if e.kind == ShellErrorKind::EndOfStream => break,
            // Any other error ends the loop too, but we still fall through
            // to the flush below. The composition leaves that duty to you:
            // a terminal error must flush the demuxer exactly as a clean
            // end does, or the last partial access unit is lost.
            Err(e) => {
                eprintln!("receiver error: {e}");
                failure = Some(e.into());
                break;
            }
        };

        // Record first, then demux. Doing the write first means a demux
        // error can never cost you bytes of the capture.
        if let Err(e) = recording.send_bytes(&pkt) {
            eprintln!("recording error: {e}");
            failure = Some(e.into());
            break;
        }
        if let Err(e) = demux.feed_aligned(&pkt) {
            eprintln!("demux error: {e}");
            failure = Some(e.into());
            break;
        }
        tally(&mut demux);
    }

    // The caller's second duty: end of input means `flush`, which completes
    // the final length-0 (unbounded) PES, then one last drain.
    //
    // (With a `ManagedRecvTransport` there is a third duty. When
    // `reconnects_handle()` rises, the packet read across the rise belongs
    // to the new connection: drop it, and call `reset_sync()` on the
    // `Receiver` AND the `Demuxer`, so neither half stitches a
    // packet from the dead link onto the new one. `ManagedDemuxReceiver`
    // does this for you; the composition does not.)
    demux.flush();
    tally(&mut demux);

    // `finish` flushes the buffered tail and surfaces an I/O error that the
    // implicit `Drop` would swallow (disk full). A recording whose tail
    // matters must end with it, even after a failure: it still flushes what
    // was written. The first fatal error is the one returned; `finish`'s
    // own error is returned only when nothing failed before it. (After a
    // failed write `FileTransport` has already dropped its writer, and
    // `finish` returns `Ok(None)`.)
    let finished = recording.finish();
    if let Some(e) = failure {
        return Err(e);
    }
    finished?;
    eprintln!("wrote {output} from {input}; samples={samples} metadata={metadata}");
    Ok(())
}
