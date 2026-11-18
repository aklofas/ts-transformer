//! `tst-rtp` RTSP server quickstart.
//!
//! Demonstrates the smallest useful path: bind an `RtspServer`, register
//! a unicast mount, push a synthetic Annex-B H.264 IDR NAL through the
//! mount's broadcast channel. A client connecting to the published URL
//! will SETUP / PLAY against the mount and receive RTP-over-UDP frames.
//!
//! Why this is teaching code:
//! - The RtspServerBuilder shape is chainable `&mut self -> &mut Self`
//!   so callers can write `b.max_sessions(N).fanout_capacity(M)` without
//!   the surface fighting them.
//! - The MountHandle push surface MIRRORS `MuxSender::send_*` on method
//!   names and signatures — the caller writes the same code regardless
//!   of whether they're feeding a `MuxSender<SrtTransport>` or a
//!   `MountHandle` that fans out via RTSP. This is the v1 "one push API,
//!   many delivery shapes" goal.
//! - Server lifecycle is sync: `bind` → `add_mount` → `start` block
//!   until the listener binds; loop pushing frames; `stop()` for graceful
//!   shutdown (sends TEARDOWN to clients + drains).
//! - There's no client here — drive with the `recv_rtsp_camera`
//!   example or any RTSP client (e.g. `ffprobe rtsp://127.0.0.1:8554/live`)
//!   in another terminal once the server prints its bound port.
//!
//! Run with:
//!   cargo run -p tst-examples --example send_rtsp_server
//!
//! The server runs forever until Ctrl-C. To make it graceful in a
//! production deployment, install a signal handler that calls
//! `server.stop()` — for this example we just `drop()` on exit, which
//! fires the hard-cancel path (Drop semantics, sub-design §D3).

use std::error::Error;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspServer, RtspServerBuilder};

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Build the muxer config. v1 of the canonical
    //    MPEG-TS-over-RTP shape: single program, one H.264 video stream.
    //
    //    Why these PIDs:
    //    - 0x1000 = PMT PID; freely chosen but must be unique within the
    //      mux. The PAT PID (0x0000) and PMT PID (0x1000 here) are
    //      reserved for table data; codec streams use other PIDs.
    //    - 0x1011 = video elementary stream PID. Anything in the range
    //      0x0020..=0x1FFF (excluding reserved values) works; 0x1011 is
    //      the canonical first-video-stream choice in our test corpora.
    let cfg = build_muxer_config()?;

    // 2. Build the server. We bind to 127.0.0.1:0 — loopback + let the
    //    kernel pick an ephemeral port (printed below). For real
    //    deployments, change to 0.0.0.0 (all IPv4 interfaces) or a
    //    specific interface IP.
    //
    //    Why 127.0.0.1: the example is designed to run alongside an
    //    RTSP client on the same machine for the simplest demo. Replace
    //    with `0.0.0.0:8554` to accept remote clients.
    //
    //    Why builder over direct `RtspServer::bind`: the builder gives
    //    us access to knobs like `session_timeout`, `fanout_capacity`,
    //    `graceful_shutdown_drain`. For v1 defaults the direct
    //    `RtspServer::bind(url)` is equivalent.
    let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0")?;
    builder
        .max_sessions(16)
        .session_timeout(Duration::from_secs(60))
        .fanout_capacity(256);
    let server: RtspServer = builder.build()?;

    // 3. Register a unicast mount at "/live". The MountHandle returned
    //    is what we push frames into; cloning the handle gives multiple
    //    threads independent push entry-points feeding the same broadcast.
    //
    //    Multicast variant (commented out — uncomment + remove the
    //    unicast line above to publish to a multicast group):
    //    let mount = server.add_multicast_mount(
    //        "/live",
    //        cfg,
    //        "rtp://239.0.0.1:5004?ttl=4",
    //    )?;
    let mount = server.add_mount("/live", cfg)?;

    // 4. Start accepting connections. `start()` spawns the internal
    //    tokio listener task and waits (up to 5 s) for the listener to
    //    report its bind. After it returns, `local_addr()` reflects the
    //    actual bound port.
    server.start()?;
    let bound = server.local_addr().expect("listener bound");
    eprintln!("RTSP server bound at rtsp://{bound}/live");
    eprintln!("Drive with:  ffprobe rtsp://{bound}/live");
    eprintln!("            or any RTSP client. Ctrl-C to stop.");

    // 5. Push loop. v1 demo pushes a synthetic Annex-B IDR NAL once
    //    every 33ms (~30fps cadence). Real applications wire a NAL
    //    source (encoder output, gstreamer appsink, etc.) here.
    //
    //    Why a synthetic NAL: the example doesn't depend on an external
    //    H.264 encoder or fixture file; the bytes are illegal-as-video
    //    but the muxer / RTP path don't care — they handle bytes.
    //    Real clients connecting will see the synthetic stream as a
    //    sequence of single-NAL RTP frames with no actual decoded video.
    let nal: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut pts_ticks: i64 = 0;
    loop {
        // PTS in 90kHz units. 33ms ≈ 2970 ticks.
        let pts = Pts90khz::new(pts_ticks);
        if let Err(e) = mount.push_video(&nal, pts, /* key_frame= */ true) {
            // The push API surfaces MountError (muxer failures / the
            // mount being closed). A lagging peer's dropped frames are
            // NOT an error here — they're tracked separately in
            // `MountStats::frames_dropped_total`.
            eprintln!("push_video error (continuing): {e}");
        }
        pts_ticks = pts_ticks.wrapping_add(2970_i64);
        thread::sleep(Duration::from_millis(33));

        // For demo brevity, exit after ~5 seconds. Comment this out
        // for an indefinite stream.
        if pts_ticks > 90_000 * 5 {
            break;
        }
    }

    // 6. Graceful shutdown. server.stop() flips the cancel token,
    //    signals all active sessions, waits graceful_shutdown_drain
    //    (default 100ms) + 1s, then returns. The runtime is dropped
    //    on `server` going out of scope; the runtime's
    //    shutdown_timeout(5s) caps any in-flight task cleanup.
    eprintln!("graceful shutdown...");
    server.stop()?;
    eprintln!("stopped.");
    Ok(())
}

fn build_muxer_config() -> Result<MuxerConfig, Box<dyn Error>> {
    let mut prog =
        MuxerProgramConfigBuilder::new(/* program_number= */ 1, /* pmt_pid= */ 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Ok(b.build()?)
}
