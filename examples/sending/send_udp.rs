//! send_udp — read an MPEG-TS file and ship it over UDP, datagram-by-datagram.
//!
//! WHY this example exists:
//!   UDP is the lowest-common-denominator MPEG-TS transport in broadcast and
//!   STANAG 4609 / ISR deployments. ffmpeg's `-f mpegts udp://host:port` is
//!   the canonical "send a stream somewhere" command, and any receiver that
//!   accepts UDP can consume what we produce here.
//!
//! HOW to run:
//!   cargo run -p tst-examples --example send_udp -- input.ts udp://239.10.0.1:5004
//!
//! HOW to verify with ffmpeg (run in another terminal):
//!   ffmpeg -i 'udp://@239.10.0.1:5004?fifo_size=1000000' -c copy out.ts
//!   sha256sum input.ts out.ts  # should match on a quiet loopback

use std::fs::File;
use std::io::Read;

use tst_core::transport::Transport;
use tst_core::url::redact_url;
use tst_udp::UdpTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: send_udp <input.ts> <udp://host:port>");
    let url = args
        .next()
        .expect("usage: send_udp <input.ts> <udp://host:port>");

    // Build the UDP sender. For multicast destinations, this also applies
    // the TTL / iface knobs in the URL. For unicast it's just a connect().
    let mut tx = UdpTransport::connect(&url)?;

    // The standard MPEG-TS-over-UDP datagram size is 7 × 188 = 1316 bytes
    // (matches ffmpeg's default and what most operators expect on the wire).
    let pkt_size = tx.max_payload();

    // Read the file in pkt_size-sized chunks. Production code would pace
    // against PCR for real-time streaming — this example ships as fast as
    // the network accepts, which is fine for a smoke test.
    let mut file = File::open(&input)?;
    let mut buf = vec![0u8; pkt_size];
    let mut total = 0u64;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // If the last chunk is shorter than pkt_size, only send `n` bytes —
        // a UDP datagram can be any size up to the link MTU.
        tx.send_bytes(&buf[..n])?;
        total += n as u64;
    }

    let stats = tx.stats();
    // A URL can carry a password or a secret query value, so it is printed
    // through `redact_url`.
    eprintln!(
        "sent {} bytes ({} datagrams) to {}",
        total,
        stats.datagrams_sent,
        redact_url(&url)
    );
    Ok(())
}
