//! Caller-mode SRT diagnostic client. Connects to a listener (e.g.,
//! the `srt_serve_ts_file` example) and pretty-prints every demux
//! event the stream produces: PMT, video samples (with NAL-unit
//! breakdown), KLV metadata (with ST 0601 decode of common tags),
//! discontinuities, non-conformance issues, and a periodic counts +
//! bitrate summary.
//!
//! This is the **VLC replacement** for shaking out the receive-side
//! pipeline. Where VLC tells you "the video plays" or "it doesn't",
//! this tells you exactly what arrived: which PIDs, which codec on
//! each, which KLV tags were present, what sensor lat/lon/heading
//! looked like at each cell, etc.
//!
//! Usage:
//!   # publisher (file-backed listener)
//!   cargo run --release -p tst-examples --example srt_serve_ts_file -- input.ts 0.0.0.0:9000 --loop
//!
//!   # consumer (this example, dials in)
//!   cargo run --release -p tst-examples --example srt_caller_dump -- 127.0.0.1:9000
//!
//! Flags:
//!   --verbose          Print every event (default: print PMT once,
//!                      every Nth Sample/Metadata, plus all
//!                      Discontinuity / NonConformant events).
//!   --every <n>        Throttle Sample/Metadata prints to one in
//!                      every `n`. Default: 30 (~1/sec at 30fps video,
//!                      1/sec at 30Hz sync KLV). `--verbose` overrides.
//!   --decode-klv       Run `klv::st0601::decode` on each Metadata
//!                      payload and print decoded tags. On by default;
//!                      use `--no-decode-klv` to suppress for very
//!                      high-rate KLV.
//!   --no-decode-klv    See above.
//!   --latency-ms <ms>  TSBPD latency budget. Defaults to 200ms;
//!                      bump to 500–1000 over lossy WAN.
//!   --summary-ms <ms>  Period for the rolling stats line. Default
//!                      2000ms. Set to 0 to disable.
//!
//! Why caller mode here? `srt_recv_typed.rs` is the *listener*
//! analogue — used when the publisher dials in to us. This example
//! is the converse: the publisher (a `srt_serve_ts_file` instance,
//! or any libsrt-compatible streamer in listener mode) waits for
//! a caller. We dial in like VLC would, but instead of decoding
//! pixels, we dump structured events.

use std::env;
use std::time::{Duration, Instant};
use tst_core::klv::st0601;
use tst_core::mpegts::demux::{
    DemuxEvent, MetadataKind, NalUnit, ProgramMap, SamplePayload, StreamId, StreamKind, VideoCodec,
    VideoPayload, split_video,
};
use tst_pipeline::{DemuxReceiver, ShellErrorKind};
use tst_srt::SrtSocketBuilder;
use tst_srt::SrtTransport;

struct Args {
    addr: String,
    verbose: bool,
    every: u64,
    decode_klv: bool,
    latency: Duration,
    summary: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut iter = env::args().skip(1);
    let addr = iter.next().ok_or_else(|| usage("missing <addr>"))?;
    let mut verbose = false;
    let mut every: u64 = 30;
    let mut decode_klv = true;
    let mut latency = Duration::from_millis(200);
    let mut summary = Duration::from_millis(2000);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--decode-klv" => decode_klv = true,
            "--no-decode-klv" => decode_klv = false,
            "--every" => {
                let v = iter.next().ok_or_else(|| usage("--every needs a value"))?;
                every = v.parse().map_err(|e| format!("bad --every: {e}"))?;
                if every == 0 {
                    every = 1;
                }
            }
            "--latency-ms" => {
                let v = iter
                    .next()
                    .ok_or_else(|| usage("--latency-ms needs a value"))?;
                let ms: u64 = v.parse().map_err(|e| format!("bad --latency-ms: {e}"))?;
                latency = Duration::from_millis(ms);
            }
            "--summary-ms" => {
                let v = iter
                    .next()
                    .ok_or_else(|| usage("--summary-ms needs a value"))?;
                let ms: u64 = v.parse().map_err(|e| format!("bad --summary-ms: {e}"))?;
                summary = Duration::from_millis(ms);
            }
            other => return Err(usage(&format!("unknown arg: {other}"))),
        }
    }
    Ok(Args {
        addr,
        verbose,
        every,
        decode_klv,
        latency,
        summary,
    })
}

fn usage(why: &str) -> String {
    format!(
        "{why}\nusage: srt_caller_dump <host:port> [--verbose] [--every <n>] [--no-decode-klv] [--latency-ms <ms>] [--summary-ms <ms>]\n  e.g. srt_caller_dump 127.0.0.1:9000 -v"
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args().map_err(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    })?;

    eprintln!(
        "connecting to srt://{}  (latency = {:?})",
        args.addr, args.latency
    );
    // Caller-side SRT socket. `connect` blocks through the handshake.
    // Match latency to the listener — libsrt negotiates the max of the
    // two sides at handshake, but matching avoids surprise.
    //
    // Bind-then-step shape (`SrtSocketBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `connect`.
    let mut sb = SrtSocketBuilder::new();
    sb.latency(args.latency);
    let socket = sb.connect(args.addr.as_str())?;
    eprintln!("connected; reading events");
    let mut rx = DemuxReceiver::new(SrtTransport::new(socket));

    let mut stats = Stats::new(args.summary);

    for item in &mut rx {
        match item {
            Ok(event) => handle_event(&event, &args, &mut stats),
            // `Closed` (peer-graceful-close) surfaces as iterator termination
            // after a final demuxer flush — not as an `Err` arm. `Broken`
            // is what the user sees on a hard hangup; both outcomes are
            // terminal here so we break on either.
            //
            // `err.kind` is the binding-author-facing pattern:
            //   - `ShellErrorKind::TransportBroken` — socket broken (peer
            //     vanished, ECONNRESET, libsrt session teardown).
            //   - `ShellErrorKind::EndOfStream` — clean peer-initiated close
            //     that bypassed the normal flush path (should not appear via
            //     iterator but defensively handled).
            // Using `err.kind` here avoids matching the inner `TransportError`
            // variant directly — the same check works across all DemuxReceiver
            // transport adapters (SRT, TCP, in-memory for tests).
            Err(ref err) if err.kind == ShellErrorKind::TransportBroken => {
                eprintln!("peer hung up");
                break;
            }
            Err(other) => {
                eprintln!("receiver error: {other}");
                break;
            }
        }
        stats.maybe_print();
    }
    stats.print_final();
    eprintln!("session done");
    Ok(())
}

fn handle_event(event: &DemuxEvent, args: &Args, stats: &mut Stats) {
    match event {
        DemuxEvent::ProgramMap(pmap) => {
            stats.pmaps += 1;
            // PMT is verbose by default — it's once-per-program-version
            // information you almost always want to see.
            print_program_map(pmap);
        }
        DemuxEvent::Sample {
            stream,
            pts,
            payload,
            ..
        } => {
            stats.samples += 1;
            stats.sample_bytes += payload_size(payload) as u64;
            let should_print = args.verbose || stats.samples.is_multiple_of(args.every);
            if should_print {
                print_sample(stream, pts.as_ticks(), payload);
            }
        }
        DemuxEvent::Metadata {
            stream,
            pts,
            kind,
            payload,
        } => {
            stats.metas += 1;
            stats.meta_bytes += payload.len() as u64;
            let should_print = args.verbose || stats.metas.is_multiple_of(args.every);
            if should_print {
                print_metadata(stream, pts.as_ticks(), kind, payload, args.decode_klv);
            }
        }
        DemuxEvent::Discontinuity { stream, kind } => {
            stats.discontinuities += 1;
            // Always print these — they're rare and diagnostic.
            eprintln!("[disc] PID=0x{:04X} {kind:?}", stream.pid);
        }
        DemuxEvent::NonConformant { stream, issue } => {
            stats.nonconformant += 1;
            eprintln!("[nc]   PID=0x{:04X} {issue:?}", stream.pid);
        }
        // Only emitted by `ManagedDemuxReceiver` (tst-pipeline) shells
        // that own reconnect; not produced by plain `DemuxReceiver`.
        // Included for exhaustive matching.
        DemuxEvent::ReconnectDiscontinuity => {}
    }
}

fn print_program_map(pmap: &ProgramMap) {
    eprintln!(
        "[PMT]  program={} pcr_pid=0x{:04X} streams={} klv_links={}",
        pmap.program_number,
        pmap.pcr_pid,
        pmap.streams.len(),
        pmap.klv_links.len(),
    );
    for s in &pmap.streams {
        eprintln!(
            "       PID=0x{:04X} stream_type=0x{:02X} kind={}",
            s.pid,
            s.stream_type.as_byte(),
            describe_stream_kind(&s.kind),
        );
    }
    for link in &pmap.klv_links {
        eprintln!(
            "       link: KLV PID=0x{:04X} → video PID=0x{:04X} ({:?})",
            link.klv_pid, link.video_pid, link.source,
        );
    }
}

fn describe_stream_kind(k: &StreamKind) -> String {
    match k {
        StreamKind::Video(VideoCodec::H264) => "Video(H.264)".into(),
        StreamKind::Video(VideoCodec::H265) => "Video(H.265)".into(),
        StreamKind::Video(VideoCodec::H266) => "Video(H.266)".into(),
        StreamKind::Video(VideoCodec::Av1) => "Video(AV1)".into(),
        StreamKind::Audio(_) => "Audio".into(),
        StreamKind::Subtitle(_) => "Subtitle".into(),
        StreamKind::KlvSync { declared_link } => match declared_link {
            Some(pid) => format!("KlvSync(linked → 0x{pid:04X})"),
            None => "KlvSync(unlinked)".into(),
        },
        StreamKind::KlvAsync => "KlvAsync".into(),
        StreamKind::Unknown(t) => format!("Unknown(stream_type=0x{t:02X})"),
    }
}

fn payload_size(p: &SamplePayload) -> usize {
    match p {
        // Raw-first: the demuxer hands us the encoded AU bytes; the NAL
        // byte tally is now an opt-in `split_video` away. (OBU-shaped AV1
        // contributes 0 here, matching the prior behavior.)
        SamplePayload::Video {
            codec,
            raw,
            av1_carriage,
            ..
        } => match split_video(raw, *codec, av1_carriage.unwrap_or_default()).0 {
            VideoPayload::Nals(nals) => nals.iter().map(nal_payload_len).sum(),
            VideoPayload::Obus(_) => 0,
        },
        SamplePayload::Audio { frames, .. } => frames.len(),
        SamplePayload::Subtitle { payload, .. } => payload.len(),
        SamplePayload::Unknown { raw, .. } => raw.len(),
    }
}

fn nal_payload_len(n: &NalUnit) -> usize {
    match n {
        NalUnit::H264 { payload, .. }
        | NalUnit::H265 { payload, .. }
        | NalUnit::H266 { payload, .. } => payload.len(),
    }
}

fn print_sample(stream: &StreamId, pts: i64, payload: &SamplePayload) {
    match payload {
        SamplePayload::Video {
            codec,
            // Raw-first: the demuxer emits the encoded AU; split into NAL/OBU
            // units via the opt-in `split_video` to inspect them.
            raw,
            // RAI sourced from the TS adaptation-field bit on the PES_start
            // packet (ISO/IEC 13818-1 §2.4.3.4); marker for AUs the encoder
            // treats as decoder-resync points.
            random_access_indicator,
            av1_carriage,
            ..
        } => match split_video(raw, *codec, av1_carriage.unwrap_or_default()).0 {
            VideoPayload::Nals(nals) => {
                // NAL-unit type tally so you can see slice/IDR/SPS/PPS/SEI
                // distribution at a glance. Indexed by raw nal_type so the
                // line stays compact even when there's a long tail of types.
                let kinds = nal_kind_summary(&nals);
                let total: usize = nals.iter().map(nal_payload_len).sum();
                let rai = if *random_access_indicator { " RAI" } else { "" };
                eprintln!(
                    "[vid]  PID=0x{:04X} pts={pts:>10} ({}) codec={codec:?} nals={} bytes={} {kinds}{rai}",
                    stream.pid,
                    fmt_pts(pts),
                    nals.len(),
                    total,
                );
            }
            VideoPayload::Obus(_) => {
                // OBU-shaped video (AV1); not tallied in this dump.
            }
        },
        SamplePayload::Audio { codec, frames } => {
            eprintln!(
                "[aud]  PID=0x{:04X} pts={pts} codec={codec:?} bytes={}",
                stream.pid,
                frames.len()
            );
        }
        SamplePayload::Subtitle { codec, payload } => {
            eprintln!(
                "[sub]  PID=0x{:04X} pts={pts} codec={codec:?} bytes={}",
                stream.pid,
                payload.len()
            );
        }
        SamplePayload::Unknown { stream_type, raw } => {
            eprintln!(
                "[unk]  PID=0x{:04X} pts={pts} stream_type=0x{:02X} bytes={}",
                stream.pid,
                stream_type.as_byte(),
                raw.len()
            );
        }
    }
}

fn nal_kind_summary(nals: &[NalUnit]) -> String {
    // Group by nal_type, tag IDR / SPS / PPS / SEI / AUD with letters
    // for instant readability; raw decimals for the long tail.
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for n in nals {
        let label = match n {
            NalUnit::H264 { nal_type, .. } => match *nal_type {
                1 => "P/B".to_string(), // non-IDR slice
                5 => "IDR".to_string(),
                6 => "SEI".to_string(),
                7 => "SPS".to_string(),
                8 => "PPS".to_string(),
                9 => "AUD".to_string(),
                t => format!("h264:{t}"),
            },
            NalUnit::H265 { nal_type, .. } => match *nal_type {
                0..=9 => "P/B".to_string(), // trail_n / tsa_n / etc
                19 | 20 => "IDR".to_string(),
                32 => "VPS".to_string(),
                33 => "SPS".to_string(),
                34 => "PPS".to_string(),
                35 => "AUD".to_string(),
                39 | 40 => "SEI".to_string(),
                t => format!("h265:{t}"),
            },
            // H.266 / VVC labels per H.266 V4 Table 5. Common picture-data
            // types share numeric values with H.265 (IDR_W_RADL=7,
            // IDR_N_LP=8 — both key-frame-shaped); parameter-set types
            // (VPS_NUT=14, SPS_NUT=15, PPS_NUT=16) and AUD_NUT=20 are
            // distinct from H.265.
            NalUnit::H266 { nal_type, .. } => match *nal_type {
                0..=6 => "P/B".to_string(), // trail / stsa / rasl / radl
                7 | 8 => "IDR".to_string(),
                9 => "CRA".to_string(),
                14 => "VPS".to_string(),
                15 => "SPS".to_string(),
                16 => "PPS".to_string(),
                17 | 18 => "APS".to_string(),
                19 => "PH".to_string(),
                20 => "AUD".to_string(),
                23 => "SEI".to_string(),
                t => format!("h266:{t}"),
            },
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    let mut parts: Vec<String> = counts
        .into_iter()
        .map(|(k, v)| format!("{k}×{v}"))
        .collect();
    parts.sort();
    format!("[{}]", parts.join(","))
}

fn fmt_pts(pts_90khz: i64) -> String {
    // Render PTS in seconds.ms so dense output stays scannable.
    let secs = pts_90khz as f64 / 90_000.0;
    format!("{secs:>9.3}s")
}

fn print_metadata(
    stream: &StreamId,
    pts: i64,
    kind: &MetadataKind,
    payload: &[u8],
    decode_klv: bool,
) {
    let kind_str = match kind {
        MetadataKind::KlvAsync => "async",
        MetadataKind::KlvSyncAuCell { .. } => "sync",
        MetadataKind::Unknown(t) => {
            return eprintln!(
                "[meta] PID=0x{:04X} pts={pts} kind=Unknown(0x{:02X}) bytes={}",
                stream.pid,
                t.as_byte(),
                payload.len(),
            );
        }
    };

    let header = format!(
        "[klv]  PID=0x{:04X} pts={pts:>10} ({}) kind={kind_str:<5} bytes={}",
        stream.pid,
        fmt_pts(pts),
        payload.len(),
    );

    if !decode_klv {
        eprintln!("{header}");
        return;
    }

    // ST 0601 decode. KlvAsync carries the bare LS, and the demuxer
    // already unwrapped the AU cell on KlvSyncAuCell — so both paths
    // feed the inner LS directly to `decode`. `decode_unchecked`
    // catches captures whose checksum byte was mishandled by the
    // sender — we report decode-vs-decode_unchecked separately so
    // you can spot a suspect encoder.
    match st0601::decode(payload) {
        Ok(ls) => eprintln!("{header} {}", st0601_summary(&ls)),
        Err(_) => match st0601::decode_unchecked(payload) {
            Ok(ls) => eprintln!("{header} (BAD CHECKSUM) {}", st0601_summary(&ls)),
            Err(e) => eprintln!("{header} decode failed: {e}"),
        },
    }
}

/// One-line summary of the most operationally interesting ST 0601 tags.
/// Bias is toward "what does the sensor and platform see right now" —
/// timestamp, sensor pose, frame center geo, slant range. Less common
/// tags (mission ID, tail number, image source) are surfaced only on
/// the *first* decoded LS so they aren't repeated 30×/sec.
fn st0601_summary(ls: &st0601::UasDatalinkLs) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(8);
    if let Some(t) = ls.timestamp_us {
        // ST 0601 tag 2 is microseconds since UNIX epoch.
        let secs = t / 1_000_000;
        let us = t % 1_000_000;
        parts.push(format!("ts={secs}.{us:06}"));
    }
    if let (Some(lat), Some(lon)) = (ls.sensor_lat_deg, ls.sensor_lon_deg) {
        parts.push(format!("sensor=({lat:.6},{lon:.6})"));
    }
    if let Some(alt) = ls.sensor_alt_m {
        parts.push(format!("alt={alt:.1}m"));
    }
    if let Some(hdg) = ls.platform_heading_deg {
        parts.push(format!("hdg={hdg:.1}°"));
    }
    if let (Some(p), Some(r)) = (ls.platform_pitch_deg, ls.platform_roll_deg) {
        parts.push(format!("pitch/roll=({p:.1},{r:.1})°"));
    }
    if let (Some(flat), Some(flon)) = (ls.frame_center_lat_deg, ls.frame_center_lon_deg) {
        parts.push(format!("frame=({flat:.6},{flon:.6})"));
    }
    if let Some(sr) = ls.slant_range_m {
        parts.push(format!("range={sr:.0}m"));
    }
    if let (Some(h), Some(v)) = (ls.sensor_hfov_deg, ls.sensor_vfov_deg) {
        parts.push(format!("fov=({h:.2},{v:.2})°"));
    }
    if parts.is_empty() {
        "(no geo tags decoded)".to_string()
    } else {
        parts.join(" ")
    }
}

struct Stats {
    started: Instant,
    last_summary: Instant,
    summary_period: Duration,
    pmaps: u64,
    samples: u64,
    metas: u64,
    discontinuities: u64,
    nonconformant: u64,
    sample_bytes: u64,
    meta_bytes: u64,
    last_samples: u64,
    last_metas: u64,
    last_sample_bytes: u64,
    last_meta_bytes: u64,
}

impl Stats {
    fn new(period: Duration) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_summary: now,
            summary_period: period,
            pmaps: 0,
            samples: 0,
            metas: 0,
            discontinuities: 0,
            nonconformant: 0,
            sample_bytes: 0,
            meta_bytes: 0,
            last_samples: 0,
            last_metas: 0,
            last_sample_bytes: 0,
            last_meta_bytes: 0,
        }
    }

    fn maybe_print(&mut self) {
        if self.summary_period.is_zero() {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_summary) < self.summary_period {
            return;
        }
        let dt = now.duration_since(self.last_summary).as_secs_f64();
        let v_rate = (self.samples - self.last_samples) as f64 / dt;
        let m_rate = (self.metas - self.last_metas) as f64 / dt;
        let v_bps = (self.sample_bytes - self.last_sample_bytes) as f64 * 8.0 / dt;
        let m_bps = (self.meta_bytes - self.last_meta_bytes) as f64 * 8.0 / dt;
        eprintln!(
            "[~~~]  Δ{dt:.1}s  vid {v_rate:>5.1}/s {:>6} kbps  klv {m_rate:>5.1}/s {:>5} kbps  cum vid={} klv={} disc={} nc={}",
            (v_bps / 1000.0) as u64,
            (m_bps / 1000.0) as u64,
            self.samples,
            self.metas,
            self.discontinuities,
            self.nonconformant,
        );
        self.last_summary = now;
        self.last_samples = self.samples;
        self.last_metas = self.metas;
        self.last_sample_bytes = self.sample_bytes;
        self.last_meta_bytes = self.meta_bytes;
    }

    fn print_final(&self) {
        let dt = self.started.elapsed().as_secs_f64();
        let v_bps = self.sample_bytes as f64 * 8.0 / dt.max(0.001);
        let m_bps = self.meta_bytes as f64 * 8.0 / dt.max(0.001);
        eprintln!(
            "[end]  ran {dt:.1}s  PMTs={} video={} ({:.0} kbps)  klv={} ({:.0} kbps)  disc={} nc={}",
            self.pmaps,
            self.samples,
            v_bps / 1000.0,
            self.metas,
            m_bps / 1000.0,
            self.discontinuities,
            self.nonconformant,
        );
    }
}
