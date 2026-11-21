//! Fixed-iteration workloads of the receive shells and the managed send for
//! instruction counting.
//!
//! The twin of `crates/tst-core/benches/ir.rs`, with the same command line:
//! `scripts/dev/bench-ir.sh` builds this file against two commits, runs every
//! workload under `valgrind --tool=callgrind` and compares instructions per
//! iteration.
//!
//! ```text
//! ir list                               one "<workload> <iterations>" line each
//! ir run <workload> <iterations> [stream-file]
//! ```
//!
//! This file must stay self-contained and keep to long-lived API: the script
//! compiles the newer commit's copy against the older commit too. `cargo
//! bench` runs it with flags of its own; any invocation other than the two
//! above prints a pointer to the script and succeeds.

use std::hint::black_box;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};
use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_pipeline::{DemuxReceiver, ManagedTransport, Receiver, ReceiverConfig, ReconnectPolicy};

/// Workload names with the iteration count the script uses. One iteration of
/// a receive workload is a whole stream through a fresh shell, its end
/// included; one of `managed_send_direct` is one send. The counts keep a
/// callgrind run to a few seconds.
const WORKLOADS: &[(&str, usize)] = &[
    ("receiver_next_packet", 2_000),
    ("demux_receiver_recv_event", 300),
    ("demux_receiver_recv_event_single_packet_reads", 300),
    ("managed_send_direct", 100_000),
];

/// Accepts every send: what remains is the managed wrapper's own cost.
struct Sink;

impl Transport for Sink {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        black_box(msg);
        Ok(())
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn close(&mut self) {}
}

/// Serves one stream in reads of a fixed size, then reports a clean end.
struct Stream {
    bytes: &'static [u8],
    at: usize,
    read: usize,
}

impl RecvTransport for Stream {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let n = self.read.min(buf.len()).min(self.bytes.len() - self.at);
        if n == 0 {
            return Err(TransportError::Closed);
        }
        buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        self.at < self.bytes.len()
    }
}

/// About 1 800 TS packets: 50 video frames (one 50 kB key frame, then 5 kB)
/// with a 200-byte KLV unit each. The same stream tst-core's harness writes
/// with `gen-stream`; used when no stream file is given.
fn generated_stream() -> Vec<u8> {
    let mut cfg = MuxerConfig::default();
    cfg.buffer_packets = 200_000;
    let mut mux = Muxer::new(cfg).expect("muxer");
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    for i in 0..50i64 {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, if i == 0 { 0x65 } else { 0x41 }];
        au.resize(5 + if i == 0 { 50_000 } else { 5_000 }, 0xA5);
        mux.push_video(&au, Pts90khz::new(i * 3000), i == 0)
            .expect("push video");
        mux.push_klv(&[0x42u8; 200], Pts90khz::new(i * 3000), 0x00)
            .expect("push klv");
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
    out
}

fn run(workload: &str, iters: usize, stream_file: Option<&str>) {
    // Two builds are handed the same bytes through the stream file.
    let bytes: &'static [u8] = match stream_file {
        Some(path) => std::fs::read(path).expect("read the stream file"),
        None => generated_stream(),
    }
    .leak();
    let stream = |read| Stream { bytes, at: 0, read };
    let demux = |read: usize| {
        for _ in 0..iters {
            let mut rx = DemuxReceiver::new(stream(read));
            while let Some(event) = rx.recv_event().expect("a clean stream") {
                black_box(event);
            }
        }
    };
    match workload {
        "receiver_next_packet" => {
            for _ in 0..iters {
                let mut rx = Receiver::new(stream(1316), ReceiverConfig::default());
                while let Ok(packet) = rx.next_packet() {
                    black_box(packet);
                }
            }
        }
        "demux_receiver_recv_event" => demux(1316),
        "demux_receiver_recv_event_single_packet_reads" => demux(188),
        "managed_send_direct" => {
            let mut tx = ManagedTransport::new(Sink, || Ok(Sink), ReconnectPolicy::default())
                .expect("the default policy validates");
            let msg = [0x47u8; 1316];
            for _ in 0..iters {
                tx.send_bytes(black_box(&msg)).expect("the sink accepts");
            }
        }
        other => {
            eprintln!("ir: unknown workload {other}");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1), arg(2)) {
        (Some("list"), None, None) => {
            for (name, iters) in WORKLOADS {
                println!("{name} {iters}");
            }
        }
        (Some("run"), Some(workload), Some(iters)) => {
            let iters = iters.parse().expect("iterations is a number");
            run(workload, iters, arg(3));
        }
        _ => println!("ir: instruction-count workloads, driven by scripts/dev/bench-ir.sh"),
    }
}
