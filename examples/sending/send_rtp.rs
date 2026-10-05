//! `tst-rtp` quickstart sender.
//!
//! Demonstrates the smallest useful path: parse a URL, open an
//! `RtpTransport`, send a few hand-built MPEG-TS packets through it.
//!
//! Why this is teaching code: the URL form (`rtp://239.x.x.x:port?ttl=N`)
//! mirrors the libsrt-URL shape callers already know from `tst-srt`; the
//! example calls out the differences explicitly (no encryption, no
//! reconnect, no PTS supplied by the transport — just bytes).
//!
//! Run with:
//!   cargo run -p tst-examples --example send_rtp
//!
//! There's no receiver here; the kernel happily sends UDP datagrams
//! into the void. To see the bytes, run `recv_rtp` in
//! another terminal first.

use std::error::Error;
use std::thread;
use std::time::Duration;

use tst_core::transport::Transport;
use tst_rtp::{RtpTransport, RtpUrl};

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Parse the URL. `rtp://` requires an explicit port; multicast
    //    addresses (`239.x.x.x`) get TTL/iface knobs via query params.
    //
    //    Why a fixed multicast group: the canonical STANAG 4609 / gimbaled
    //    deployment binds to a known multicast group on the local LAN so
    //    one sender feeds N receivers (ground station + log server +
    //    operator console) with no extra cost.
    let url = "rtp://239.55.55.1:5004?ttl=8";
    let parsed = RtpUrl::parse(url)?;
    println!("dest={}:{} ttl={:?}", parsed.host, parsed.port, parsed.ttl);

    // 2. Open the transport. `connect_with` is the URL-aware
    //    constructor; the bare `connect(url_str)` is sugar that calls
    //    RtpUrl::parse internally.
    let mut tx = RtpTransport::connect_with(&parsed)?;
    println!(
        "max_payload = {} (RTP header reserves 12 of {} UDP-bytes)",
        tx.max_payload(),
        1316
    );

    // 3. Send 100 hand-built TS packets. Pattern picked to make
    //    `tcpdump -i lo udp port 5004 -X` visually obvious. Real
    //    callers use `MuxSender<RtpTransport>` and push H.264/H.265/
    //    KLV/etc.; the raw `Transport::send_bytes` path is shown here
    //    so the example doesn't pull in tst-pipeline.
    let mut payload = vec![0u8; 1316 - 12];
    payload[0] = 0x47; // TS sync byte
    for i in 0..100u32 {
        // Vary one byte per send so a packet sniffer sees the stream
        // moving.
        payload[1] = (i & 0xFF) as u8;
        tx.send_bytes(&payload)?;
        thread::sleep(Duration::from_millis(20));
    }

    println!("sent 100 packets. exit");
    Ok(())
}
