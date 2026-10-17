//! Example: serve a `.ts` file over SRT in **listener mode** so a third
//! party (VLC, ffmpeg, mpv, GStreamer, libsrt's `srt-live-transmit`, …)
//! can connect as the *caller* and pull the stream.
//!
//! Run with:
//!   cargo run --release -p tst-examples --example srt_serve_ts_file -- input.ts 0.0.0.0:9000
//!   cargo run --release -p tst-examples --example srt_serve_ts_file -- input.ts 0.0.0.0:9000 --loop
//!
//! Then in VLC:
//!   Media → Open Network Stream → `srt://<host>:9000`
//!   (or from the CLI: `vlc srt://127.0.0.1:9000`)
//!
//! ## Why listener mode here?
//!
//! VLC's SRT input defaults to **caller** mode (`srt://host:port` →
//! VLC dials out). Easiest interop: we listen, VLC dials in. The
//! converse — `ts_relay_from_file.rs` — assumes the receiver is
//! already listening, so it's the wrong shape for ad-hoc VLC tests.
//!
//! ## Pacing — **PCR-driven by default**
//!
//! Recorded `.ts` files are gigabytes-on-disk; blasting one at line
//! rate over SRT overruns VLC's recv buffer immediately ("No room to
//! store incoming packet… TSBPD ready in -NNNms" in VLC's log). VLC
//! disconnects, reconnects, sees the same flood, loops.
//!
//! The fix is to send each TS packet at the wall-clock time its PCR
//! says it should arrive — i.e., real-time playback. We:
//!
//!   1. Walk the input 188 bytes at a time.
//!   2. Parse PCR out of any packet whose adaptation field carries
//!      one (`adaptation_field_control` bits + `PCR_flag`).
//!   3. Anchor on the first PCR: `t_zero_wall = Instant::now()`,
//!      `t_zero_pcr = first_pcr`.
//!   4. For each subsequent PCR-bearing packet, sleep until
//!      `now - t_zero_wall ≈ pcr_now - t_zero_pcr`.
//!   5. Bytes between PCRs ride out at line rate within their inter-
//!      PCR window — that window is small (typically 30–100 ms in
//!      well-formed streams) so the resulting wire pacing tracks
//!      the file's encoded bitrate within a frame or two.
//!
//! This matches what `ffmpeg -re` and `srt-live-transmit -t` do.
//!
//! ## Other pacing modes
//!
//! - `--rate <bits/sec>` — token-bucket at a fixed rate. Use if the
//!   file has no PCRs (rare) or you want to simulate a constrained
//!   uplink.
//! - `--no-pace` — blast at line rate. Useful for a `srt-live-transmit`-
//!   to-`srt-live-transmit`-style relay where the receiver is another
//!   SRT-aware tool that buffers correctly. Not for VLC.
//!
//! ## TS-only (`Sender`)
//!
//! `Sender` is the right shape here, not `MuxSender` (which expects
//! pre-elementary-stream NAL+KLV inputs and does its own muxing).
//! The corpus file is *already* a muxed TS, so we shovel its bytes
//! through `Sender::send_ts` and let its 7-packet bundling +
//! sync-byte verification pass them through unchanged.

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};
use tst_pipeline::{Sender, SenderConfig, SenderError};
use tst_srt::SrtListenerBuilder;
use tst_srt::SrtTransport;

/// Read granularity. Must be a multiple of 188 so we hand `Sender`
/// already-aligned packets and our PCR walker doesn't have to buffer
/// partial packets across reads. 188 * 350 ≈ 65 KB.
const READ_CHUNK: usize = 188 * 350;

#[derive(Copy, Clone, PartialEq, Eq)]
enum PaceMode {
    /// Default: walk PCRs, sleep to match real-time playback.
    Pcr,
    /// Token-bucket at a fixed bitrate.
    FixedRate(u64),
    /// No pacing — blast at line rate.
    None,
}

struct Args {
    path: String,
    bind: String,
    loop_forever: bool,
    pace: PaceMode,
    /// SRT TSBPD latency budget. Higher = more recv-side buffering;
    /// 200ms is a reasonable LAN default that VLC accepts cleanly.
    latency: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut iter = env::args().skip(1);
    let path = iter.next().ok_or_else(|| usage("missing <file.ts>"))?;
    let bind = iter.next().ok_or_else(|| usage("missing <bind-addr>"))?;
    let mut loop_forever = false;
    let mut pace = PaceMode::Pcr;
    let mut latency = Duration::from_millis(200);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--loop" => loop_forever = true,
            "--no-pace" => pace = PaceMode::None,
            "--rate" => {
                let v = iter
                    .next()
                    .ok_or_else(|| usage("--rate needs a value (bits/sec)"))?;
                let bps: u64 = v.parse().map_err(|e| format!("bad --rate: {e}"))?;
                pace = PaceMode::FixedRate(bps);
            }
            "--latency-ms" => {
                let v = iter
                    .next()
                    .ok_or_else(|| usage("--latency-ms needs a value"))?;
                let ms: u64 = v.parse().map_err(|e| format!("bad --latency-ms: {e}"))?;
                latency = Duration::from_millis(ms);
            }
            other => return Err(usage(&format!("unknown arg: {other}"))),
        }
    }
    Ok(Args {
        path,
        bind,
        loop_forever,
        pace,
        latency,
    })
}

fn usage(why: &str) -> String {
    format!(
        "{why}\nusage: srt_serve_ts_file <file.ts> <bind-addr> [--loop] [--rate <bps>] [--no-pace] [--latency-ms <ms>]\n  default pacing: PCR-driven (real-time playback)\n  e.g. srt_serve_ts_file capture.ts 0.0.0.0:9000 --loop"
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args().map_err(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    })?;

    if !Path::new(&args.path).exists() {
        return Err(format!("input file not found: {}", &args.path).into());
    }

    // Listener config:
    //  - `latency()` advertises the TSBPD budget at handshake; both
    //    ends negotiate the max of caller's and listener's value.
    //  - We don't crank `recv_buf_bytes` here — listener-as-sender
    //    doesn't really receive payloads, just SRT control packets.
    //
    // Bind-then-step shape (`SrtListenerBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `bind`. Same pattern as
    // `SrtSocketBuilder` — translates uniformly across language bindings.
    let mut lb = SrtListenerBuilder::new();
    lb.latency(args.latency);
    let mut listener = lb.bind(args.bind.as_str())?;
    let local = listener.local_addr()?;
    eprintln!("listening on srt://{local}  (latency = {:?})", args.latency);
    eprintln!("    in VLC:  srt://{local}");
    match args.pace {
        PaceMode::Pcr => eprintln!("    pacing:  PCR-driven (real-time)"),
        PaceMode::FixedRate(bps) => eprintln!("    pacing:  fixed rate {bps} bps"),
        PaceMode::None => eprintln!("    pacing:  none (blast)"),
    }
    if args.loop_forever {
        eprintln!("    --loop:  rewind on EOF; re-accept on disconnect");
    }

    // Outer loop: re-`accept` after each peer disconnect so a single
    // `cargo run` survives multiple VLC sessions. Ctrl-C to exit.
    loop {
        eprintln!("waiting for caller to connect…");
        let (socket, peer) = match listener.accept() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("accept error: {e} — re-arming");
                continue;
            }
        };
        eprintln!("caller connected from {peer}");

        let transport = SrtTransport::new(socket);
        let mut sender = Sender::new(transport, SenderConfig::default());

        match stream_file(&mut sender, &args.path, args.pace) {
            Ok(bytes) => eprintln!("session done: streamed {bytes} bytes"),
            Err(e) => eprintln!("session ended: {e}"),
        }
        // Best-effort flush so the very last bundle leaves our side.
        // Then a brief drain so libsrt's send buffer empties before
        // close — closing immediately drops in-flight packets, which
        // VLC then sees as a truncated stream tail.
        let _ = sender.flush();
        std::thread::sleep(args.latency + Duration::from_millis(200));
        sender.close();

        if !args.loop_forever {
            break;
        }
        // Tiny pause so a misbehaved peer hammering reconnects doesn't
        // spin our accept loop. 250ms is invisible to humans, painful
        // to a flap-loop bug.
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Pump the file through the sender at the chosen pacing.
fn stream_file(
    sender: &mut Sender<SrtTransport>,
    path: &str,
    pace: PaceMode,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total: u64 = 0;
    let mut pacer = Pacer::new(pace);

    loop {
        // Read in packet-aligned multiples. `read_exact` would be
        // strict but rejects EOF mid-buffer; loop reads until we
        // have a packet-aligned chunk or hit real EOF.
        let mut filled = 0usize;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if filled == 0 {
            break; // clean EOF
        }
        // Truncate to packet boundary in case the file isn't a clean
        // multiple of 188 (rare, but cheap to handle).
        let aligned = (filled / 188) * 188;
        if aligned == 0 {
            break;
        }

        pacer.send(sender, &buf[..aligned])?;
        total += aligned as u64;
    }
    Ok(total)
}

/// Pacing dispatcher. Mode-specific state is kept inside the variants;
/// `send` is the single entry point the streamer uses regardless of
/// mode so the outer loop stays simple.
enum Pacer {
    Pcr(PcrPacer),
    Fixed(FixedRatePacer),
    None,
}

impl Pacer {
    fn new(mode: PaceMode) -> Self {
        match mode {
            PaceMode::Pcr => Pacer::Pcr(PcrPacer::new()),
            PaceMode::FixedRate(bps) => Pacer::Fixed(FixedRatePacer::new(bps)),
            PaceMode::None => Pacer::None,
        }
    }

    fn send(&mut self, sender: &mut Sender<SrtTransport>, bytes: &[u8]) -> Result<(), SenderError> {
        match self {
            Pacer::Pcr(p) => p.send(sender, bytes),
            Pacer::Fixed(p) => p.send(sender, bytes),
            Pacer::None => sender.send_ts(bytes),
        }
    }
}

/// PCR-driven pacer. Walks the chunk packet-by-packet to find PCRs;
/// at each PCR it flushes the accumulated packets through the sender,
/// then sleeps until wall-clock time matches the PCR offset relative
/// to the first PCR seen.
///
/// 27 MHz units throughout — the PCR is `base * 300 + ext` per
/// ISO/IEC 13818-1, and we never round to 90 kHz so we don't lose
/// the sub-millisecond precision on dense PCRs.
struct PcrPacer {
    first_pcr_27mhz: Option<u64>,
    last_pcr_27mhz: Option<u64>,
    start_wall: Option<Instant>,
}

impl PcrPacer {
    fn new() -> Self {
        Self {
            first_pcr_27mhz: None,
            last_pcr_27mhz: None,
            start_wall: None,
        }
    }

    fn send(&mut self, sender: &mut Sender<SrtTransport>, bytes: &[u8]) -> Result<(), SenderError> {
        // Walk packets; emit accumulated runs at each PCR boundary, then sleep.
        let mut emit_from = 0usize;
        let mut i = 0usize;
        while i + 188 <= bytes.len() {
            let pkt = &bytes[i..i + 188];
            if let Some(pcr) = parse_pcr(pkt) {
                // Detect wraparound on the 33-bit base. PCR base wraps
                // every ~26.5 hours; we treat a backward step >1s as
                // a rollover and re-anchor wall time so playback
                // resumes against the new origin instead of pausing
                // for hours. Rare on real captures but cheap to guard.
                if let Some(prev) = self.last_pcr_27mhz {
                    if pcr < prev && (prev - pcr) > 27_000_000 {
                        // Re-anchor on this PCR.
                        self.first_pcr_27mhz = Some(pcr);
                        self.start_wall = Some(Instant::now());
                    }
                }

                // First-PCR anchor.
                if self.first_pcr_27mhz.is_none() {
                    self.first_pcr_27mhz = Some(pcr);
                    self.start_wall = Some(Instant::now());
                }
                self.last_pcr_27mhz = Some(pcr);

                // Emit everything up to and including this packet.
                let end = i + 188;
                if end > emit_from {
                    sender.send_ts(&bytes[emit_from..end])?;
                    emit_from = end;
                }

                // Sleep until wall-clock matches PCR offset.
                let first = self.first_pcr_27mhz.unwrap();
                let start = self.start_wall.unwrap();
                let pcr_offset_us = (pcr.wrapping_sub(first)) / 27;
                let target = start + Duration::from_micros(pcr_offset_us);
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
            }
            i += 188;
        }
        // Emit any tail packets between the last PCR and end-of-chunk.
        if emit_from < bytes.len() {
            sender.send_ts(&bytes[emit_from..])?;
        }
        Ok(())
    }
}

/// Token-bucket pacer keyed on cumulative bytes sent.
struct FixedRatePacer {
    bps: u64,
    start: Instant,
    total: u64,
}

impl FixedRatePacer {
    fn new(bps: u64) -> Self {
        Self {
            bps,
            start: Instant::now(),
            total: 0,
        }
    }

    fn send(&mut self, sender: &mut Sender<SrtTransport>, bytes: &[u8]) -> Result<(), SenderError> {
        sender.send_ts(bytes)?;
        self.total += bytes.len() as u64;
        let want_secs = (self.total as f64 * 8.0) / self.bps as f64;
        let want = Duration::from_secs_f64(want_secs);
        let elapsed = self.start.elapsed();
        if want > elapsed {
            std::thread::sleep(want - elapsed);
        }
        Ok(())
    }
}

/// Extract the PCR (in 27 MHz units) from a 188-byte TS packet, if any.
///
/// Layout per ISO/IEC 13818-1:
///   byte 0: 0x47 sync
///   byte 3: bits 5..4 = adaptation_field_control (AFC)
///   byte 4: adaptation_field_length (only if AFC has bit 0x2)
///   byte 5: AF flags — bit 0x10 = PCR_flag
///   bytes 6..12: PCR (33-bit base + 6 reserved + 9-bit ext)
///
/// Returns `None` for packets without a PCR (the vast majority).
fn parse_pcr(pkt: &[u8]) -> Option<u64> {
    if pkt.len() < 188 || pkt[0] != 0x47 {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0x3;
    if afc & 0x2 == 0 {
        return None; // no adaptation field
    }
    let af_len = pkt[4] as usize;
    if af_len < 7 {
        return None; // AF too short to carry PCR + flag byte
    }
    let af_flags = pkt[5];
    if af_flags & 0x10 == 0 {
        return None; // PCR_flag clear
    }
    let b = &pkt[6..12];
    let base = ((b[0] as u64) << 25)
        | ((b[1] as u64) << 17)
        | ((b[2] as u64) << 9)
        | ((b[3] as u64) << 1)
        | ((b[4] as u64) >> 7);
    let ext = (((b[4] as u64) & 0x01) << 8) | (b[5] as u64);
    Some(base * 300 + ext)
}
