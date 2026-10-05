//! send_pipeline_to_socket — synthetic frames + KLV → MuxSender → SRT socket.
//!
//! This is the SRT-side analogue of `mux_to_file.rs` — same synthetic-AU
//! shape, but the file sink is replaced with an `SrtTransport`. Demonstrates
//! the canonical `MuxSender` + `SocketBuilder` shape integrators reach for
//! when the upstream is an encoder and the downstream is an SRT peer.
//!
//! On the receiver side, run something like:
//!   srt-live-transmit srt://:9000 file://con > /tmp/out.ts
//!
//! Usage: `cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000`

use std::env;
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_srt::SocketBuilder;
use tst_srt::SrtTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".into());

    // SocketBuilder applies sender-side defaults via `connect`. The single
    // option we override here is `latency`, the most user-visible knob:
    //
    //   - SRT's reliability comes from a fixed-latency receiver buffer; the
    //     value is the time budget the receiver has to request retransmits
    //     before the packet's deadline passes and it gets dropped.
    //   - Both sides should agree (libsrt picks max(snd, rcv) at handshake).
    //   - 120 ms is a reasonable LAN/regional WAN default; bump to 300 ms+
    //     for transcontinental or cellular paths.
    //
    // Other knobs worth knowing about (not set here, defaults apply):
    //   - `max_bandwidth` — caps the sender's outbound rate. Default is
    //     unlimited, which is fine on closed networks but should be set on
    //     shared links so SRT plays fair with other traffic.
    //   - `passphrase` — enables AES encryption (10..=79 chars). Must match
    //     on both ends. See `encrypted_send_recv.rs` for the full pattern.
    //
    // Bind-then-step shape (`SocketBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `connect`. Same shape every
    // example uses — see `docs/reference/binding-authors.md` for how Kotlin/Swift/
    // Python bindings spell the same idiom.
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect(addr.as_str())?;

    // `SrtTransport` adapts `Socket` to the `Transport` trait MuxSender wants.
    // The trait split keeps `tst-core` libsrt-free; only `tst-srt` pulls in
    // the SRT dependency.
    let transport = SrtTransport::new(socket);

    // `MuxSender::new(transport, config)` — argument order matches
    // `Sender::new` and `RawSender::new`.
    // Default config: program 1, H.264 video on PID 0x1011, async KLV on
    // PID 0x1031. See `mux_to_file.rs` for the same defaults written to a
    // file instead.
    let sender = MuxSender::new(transport, MuxerConfig::default())?;

    eprintln!("sending 5 synthetic frames + KLV to {addr}");
    for i in 0..5 {
        // 33 ms wall-clock cadence ≈ 30 fps. PTS uses ms here (`i * 33_000`)
        // and gets converted to the 90 kHz MPEG-TS clock for the KLV PES
        // (`* 90 / 1000`); the muxer normalizes whatever scale `send_video`
        // sees as the wall clock.
        let nal = synthetic_nal_au(500);
        let klv = synthetic_klv(64, i);
        // First frame is the key frame (i == 0) — drives the
        // `random_access_indicator` bit so a fresh receiver can attach.
        sender.send_video(&nal, Pts90khz::new(i * 33_000), i == 0)?;
        // `metadata_service_id` goes into the AU cell header per H.222.0
        // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
        // streams (stream_type 0x15); silently ignored for PrivateData
        // streams (0x06) like the one used here. The spec default is 0x00.
        sender.send_klv(&klv, Pts90khz::new((i * 33_000) * 90 / 1000), 0x00)?;
        std::thread::sleep(Duration::from_millis(33));
    }
    eprintln!("done. closing.");
    // `close()` cancels the send-side first (interrupting any in-flight
    // libsrt blocking call), then drains. Without the cancel-first path a
    // peer drop could leave us stuck in `srt_sendmsg` until SRTO_SNDTIMEO
    // expires.
    sender.close();
    // Brief settle window so the OS-level FIN actually leaves the socket
    // before we exit. Not strictly required (Drop also closes), but makes
    // the example's "done" log line line up with the wire reality.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn synthetic_nal_au(payload_size: usize) -> Vec<u8> {
    // Annex-B IDR: start code 0x00000001 + NAL header byte 0x65 (forbidden=0,
    // nal_ref_idc=3, nal_unit_type=5 = IDR slice). Filler bytes after.
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    buf.extend(std::iter::repeat(0xAA).take(payload_size));
    buf
}

fn synthetic_klv(size: usize, seq: i64) -> Vec<u8> {
    // Minimal ST 0601-shaped record: 16-byte UL + BER short-form length +
    // payload. The UL bytes here are the canonical ST 0601 UAS Datalink LS
    // key per MISB ST 0601 §6 — placeholder is fine because the muxer is
    // opaque to KLV contents (no parsing on the send path).
    let mut buf = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    buf.push(size as u8); // BER short-form length (≤127)
    buf.extend(std::iter::repeat(seq as u8).take(size));
    buf
}
