//! send_tcp — read an MPEG-TS file and ship it over TCP.
//!
//! WHY this example exists:
//!   TCP is the reliable-bytestream sibling of UDP for MPEG-TS shipping.
//!   Whereas UDP is best-effort and aligned-datagram, TCP is bytestream and
//!   acknowledged — useful when packet loss matters more than latency, or
//!   when the receiver is firewall-gated and a TCP connect-out is easier
//!   than UDP multicast. `ffmpeg -listen 1 -i tcp://...` is a common
//!   counterpart on the receiver side.
//!
//! HOW to run:
//!   cargo run -p tst-examples --example send_tcp -- input.ts tcp://127.0.0.1:7001
//!
//! HOW to verify with ffmpeg (run BEFORE send_tcp):
//!   ffmpeg -listen 1 -i tcp://127.0.0.1:7001 -c copy out.ts
//!   # then in another terminal:
//!   cargo run -p tst-examples --example send_tcp -- input.ts tcp://127.0.0.1:7001

use std::fs::File;
use std::io::Read;

use tst_core::transport::Transport;
use tst_core::url::redact_url;
use tst_tcp::TcpTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: send_tcp <input.ts> <tcp://host:port>");
    let url = args
        .next()
        .expect("usage: send_tcp <input.ts> <tcp://host:port>");

    // Build the TCP sender (caller side). `tcps://` works the same way and
    // wraps the stream in rustls 0.23 using the OS native cert store.
    let mut tx = TcpTransport::connect(&url)?;

    // TCP is a bytestream — `max_payload` reports the per-call send chunk
    // limit (default 64 KiB), not a datagram boundary. We use that as the
    // file-read chunk size.
    let pkt_size = tx.max_payload();

    let mut file = File::open(&input)?;
    let mut buf = vec![0u8; pkt_size];
    let mut total = 0u64;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        tx.send_bytes(&buf[..n])?;
        total += n as u64;
    }

    let stats = tx.stats();
    // A URL can carry a password or a secret query value, so it is printed
    // through `redact_url`.
    eprintln!(
        "sent {} bytes ({} send calls) to {}",
        total,
        stats.send_calls,
        redact_url(&url)
    );
    Ok(())
}
