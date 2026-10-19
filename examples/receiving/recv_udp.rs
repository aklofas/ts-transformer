//! recv_udp — listen for an MPEG-TS UDP stream and write it to a file.
//!
//! WHY this example exists:
//!   The symmetric receiver for send_udp. Lets you verify the on-wire format
//!   against any UDP MPEG-TS producer (our send_udp, ffmpeg, GStreamer, VLC,
//!   STANAG 4609 sensor pods).
//!
//! HOW to run:
//!   # Unicast (bind on any local interface)
//!   cargo run -p tst-examples --example recv_udp -- udp://@0.0.0.0:5004 out.ts
//!
//!   # Multicast (join group 239.10.0.1)
//!   cargo run -p tst-examples --example recv_udp -- 'udp://@239.10.0.1:5004?iface=eth0' out.ts
//!
//! Stop with Ctrl-C; the file flushes on drop.

use std::fs::File;
use std::io::Write;

use tst_core::transport::RecvTransport;
use tst_udp::UdpRecvTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .expect("usage: recv_udp <udp://@host:port> <out.ts>");
    let out_path = args
        .next()
        .expect("usage: recv_udp <udp://@host:port> <out.ts>");

    let mut rx = UdpRecvTransport::listen(&url)?;
    let mut file = File::create(&out_path)?;
    let mut buf = vec![0u8; rx.max_payload()];
    let mut total = 0u64;

    eprintln!("listening on {url} → writing to {out_path}");

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
                eprintln!("recv error: {e}");
                break;
            }
        }
    }
    Ok(())
}
