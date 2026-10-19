//! recv_tcp — listen for an MPEG-TS TCP stream and write it to a file.
//!
//! WHY this example exists:
//!   The symmetric listener for send_tcp. Accepts a single inbound caller
//!   on a fixed port, then drains the TS bytestream to disk. Pairs with
//!   send_tcp, ffmpeg's `-f mpegts tcp://host:port`, GStreamer's tcpsink,
//!   or any other TCP MPEG-TS producer.
//!
//! HOW to run:
//!   cargo run -p tst-examples --example recv_tcp -- 0.0.0.0:7001 out.ts
//!
//! Stop with Ctrl-C; the file flushes on drop.

use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;

use tst_core::transport::RecvTransport;
use tst_tcp::TcpListener;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let bind_addr = args
        .next()
        .expect("usage: recv_tcp <bind_addr:port> <out.ts>");
    let out_path = args
        .next()
        .expect("usage: recv_tcp <bind_addr:port> <out.ts>");

    let bind: SocketAddr = bind_addr.parse()?;
    let listener = TcpListener::bind(bind)?;
    eprintln!("listening on {bind_addr} → writing to {out_path} (waiting for caller…)");

    // Accept exactly one inbound connection then drain the stream.
    let mut rx = listener.accept_blocking()?;
    eprintln!("accepted connection from {}", rx.peer());

    let mut file = File::create(&out_path)?;
    let mut buf = vec![0u8; rx.max_payload()];
    let mut total = 0u64;

    loop {
        match rx.recv_bytes(&mut buf) {
            Ok(n) => {
                file.write_all(&buf[..n])?;
                total += n as u64;
                if total.is_multiple_of(1024 * 1024) {
                    eprintln!("received {} MiB", total / (1024 * 1024));
                }
            }
            Err(e) => {
                eprintln!("recv ended: {e}");
                break;
            }
        }
    }
    eprintln!("done — wrote {total} bytes to {out_path}");
    Ok(())
}
