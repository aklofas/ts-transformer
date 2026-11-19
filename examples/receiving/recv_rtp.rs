//! `tst-rtp` quickstart receiver.
//!
//! Pairs with `send_rtp.rs`. Bind to an `rtp://` URL, recv N packets,
//! print byte counts + the malformed-packet counter.
//!
//! Why this is teaching code: the recv API mirrors the libsrt-side
//! `Receiver<SrtTransport>` shape. The two interesting Phase-1
//! differences are:
//!   1. RTP has no handshake — `listen()` binds the socket and is
//!      done; there's no peer to authenticate.
//!   2. RTP-specific protocol stats (malformed packets) live on
//!      `RtpRecvTransport::rtp_stats()`, separate from
//!      `socket_stats()` which carries the wire-level counters.
//!
//! Run with (alongside `send_rtp`):
//!   cargo run -p tst-examples --example recv_rtp

use std::error::Error;
use std::time::{Duration, Instant};

use tst_core::transport::RecvTransport;
use tst_core::url::redact_url;
use tst_rtp::RtpRecvTransport;

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Bind. For multicast, host is the group address — recv binds to
    //    ANY:port internally and joins. Pass `?iface=127.0.0.1` to scope
    //    the join to the loopback interface (useful for single-host tests).
    let url = "rtp://239.55.55.1:5004?iface=127.0.0.1";
    let mut rx = RtpRecvTransport::listen(url)?;
    // A URL can carry a password or a secret query value, so it is printed
    // through `redact_url`.
    println!("listening on {}", redact_url(url));

    // 2. recv loop. RtpRecvTransport already strips the 12-byte RTP
    //    header; `recv_bytes` returns the TS payload directly. Cap the
    //    loop at 5 seconds so the example terminates without external
    //    coordination.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = vec![0u8; rx.max_payload() + 64];
    let mut total_bytes = 0u64;
    let mut total_pkts = 0u64;
    while Instant::now() < deadline {
        match rx.recv_bytes(&mut buf) {
            Ok(n) => {
                total_pkts += 1;
                total_bytes += n as u64;
            }
            Err(e) => {
                eprintln!("recv error: {e}");
                break;
            }
        }
    }

    // 3. Stats. socket_stats() carries the wire counters; rtp_stats()
    //    carries the protocol counters.
    let socket_stats = rx.socket_stats().unwrap_or_default();
    let rtp_stats = rx.rtp_stats();
    println!(
        "recv summary — {} packets, {} bytes, {} malformed",
        total_pkts, total_bytes, rtp_stats.malformed_packets,
    );
    println!("socket counters: {:?}", socket_stats);
    Ok(())
}
