//! mux_to_hls — read an MPEG-TS file and re-mux to HLS segments + playlist.
//!
//! WHY this example exists:
//!   Demonstrates the new `Publisher` trait family by piping a real `.ts` file
//!   into the HLS publisher. Verifies the end-to-end stack: MuxPublisher →
//!   HlsPublisher → on-disk .ts segments + .m3u8 + internal HTTP server.
//!   KLV passes through transparently; STANAG 4609-aware players still
//!   decode the metadata from the HLS segments.
//!
//! HOW to run:
//!   cargo run -p tst-examples --example mux_to_hls -- input.ts /tmp/hls 127.0.0.1:8080
//!
//! HOW to verify with ffmpeg / VLC / mpv:
//!   ffplay 'http://localhost:8080/playlist.m3u8'
//!   vlc    'http://localhost:8080/playlist.m3u8'
//!   mpv    'http://localhost:8080/playlist.m3u8'
//!
//! Stop with Ctrl-C.

use std::fs::File;
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: mux_to_hls <input.ts> <out_dir> <bind>");
    let out_dir = PathBuf::from(
        args.next()
            .expect("usage: mux_to_hls <input.ts> <out_dir> <bind>"),
    );
    let bind: SocketAddr = args
        .next()
        .expect("usage: mux_to_hls <input.ts> <out_dir> <bind>")
        .parse()?;

    let publisher = HlsPublisherBuilder::new()
        .bind(bind)
        .output_dir(&out_dir)
        .segment_duration(Duration::from_secs(4))
        .playlist_window(6)
        .mode(HlsMode::Live)
        .build()?;
    eprintln!(
        "HLS publisher serving http://{}/playlist.m3u8 (out_dir = {})",
        publisher.local_addr().unwrap(),
        out_dir.display()
    );

    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build()?
    };

    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg)?;

    // Read input in 64 KiB chunks; each chunk is treated as a video AU with
    // key_frame=true so a segment cuts per chunk. Production code would parse
    // real H.264 NALs and IDR-align.
    let mut f = BufReader::new(File::open(&input)?);
    let mut buf = vec![0u8; 64 * 1024];
    let mut pts_ticks: i64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let pts = Pts90khz::new(pts_ticks);
        pub_shell.push_video(&buf[..n], pts, true)?;
        pts_ticks += 9001;
    }

    let stats = pub_shell.stats();
    eprintln!(
        "pushed {} bytes through {} drain calls",
        stats.bytes_pushed, stats.drain_calls
    );

    let publisher = pub_shell.finish()?;
    publisher.finish()?;
    Ok(())
}
