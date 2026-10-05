//! RTSP H.264 → Muxer → `.ts` file gateway.
//!
//! Demonstrates the full RTSP H.264 ingest pipeline using the RFC 6184
//! depacketizer:
//!
//! 1. Connect to an RTSP server exposing a single H.264 m-line.
//! 2. DESCRIBE — fetch the SDP to learn payload type and out-of-band
//!    parameter sets (SPS/PPS via `sprop-parameter-sets`).
//! 3. `setup_h264_auto` — pick the unique H.264 m-line and SETUP it.
//!    Returns the session + an `H264DepayConfig` carrying the negotiated
//!    payload type and decoded `sprop-parameter-sets` NALUs.
//! 4. `into_h264_receiver` — consume the session into an `H264Receiver`
//!    ready to call `recv_au`.
//! 5. PLAY — start the RTP flow.
//! 6. Loop `recv_au` → push each AU to a `Muxer` → drain TS packets to
//!    a `.ts` file.
//! 7. Drop the client — sends RTSP TEARDOWN automatically.
//!
//! # Why this example exists
//!
//! The RTSP-over-MP2T path (see `recv_rtsp_camera.rs`) is the preferred
//! shape for STANAG 4609 cameras: the entire MPEG-TS stream is delivered
//! as a single m-line (PT=33), video + KLV + audio all multiplexed by
//! the camera. This H.264 example covers the complementary case: cameras
//! that expose a bare H.264 elementary stream over RTSP — no enclosing
//! MPEG-TS, no KLV in the same RTP flow. The gateway re-muxes the
//! elementary stream into MPEG-TS so downstream consumers get the same
//! file format either way.
//!
//! # Compile gate — not run in CI
//!
//! This example requires a live RTSP camera or a MediaMTX / ffmpeg
//! server. It is intentionally excluded from CI test runs. The CI gate
//! is a compile check only:
//!
//! ```text
//! cargo build -p tst-examples --example recv_rtsp_h264
//! ```
//!
//! To run against a local MediaMTX instance publishing a test stream:
//!
//! ```text
//! # In one terminal:
//! #   mediamtx  (config: path /live/cam with source of your choice)
//! # In another:
//! cargo run -p tst-examples --example recv_rtsp_h264 -- \
//!     rtsp://127.0.0.1:8554/live/cam
//! ```
//!
//! Force TCP-interleaved when UDP is blocked by a NAT or firewall:
//!
//! ```text
//! cargo run -p tst-examples --example recv_rtsp_h264 -- \
//!     'rtsp://user:pass@cam.local/stream?transport=tcp'
//! ```

use std::env;
use std::error::Error;
use std::fs::File;
use std::io::Write;

use tst_core::mpegts::mux::{Muxer, MuxerConfig};
use tst_rtp::RtspClient;

fn main() -> Result<(), Box<dyn Error>> {
    // ── (0) URL + output path from argv ──────────────────────────────────
    //
    // Argv layout: <rtsp-url> [<out.ts>]
    // The RTSP URL may carry inline credentials (`user:pass@host`) and an
    // optional `?transport=tcp` query knob that forces TCP-interleaved
    // (RFC 7826 §14) without waiting for a 461 rejection response.
    let args: Vec<String> = env::args().collect();
    let url = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("rtsp://admin:secret@cam.local/h264");

    // Default output lands in the OS temp dir so the example works
    // cross-platform without needing write permission in the cwd.
    // (CLAUDE.md cross-platform-paths rule: use std::env::temp_dir().)
    let default_out = env::temp_dir().join("recv_rtsp_h264.ts");
    let out_path = args
        .get(2)
        .map(std::path::PathBuf::from)
        .unwrap_or(default_out);

    eprintln!("Connecting to {url}");
    eprintln!("Output: {}", out_path.display());

    // ── (1) Connect ───────────────────────────────────────────────────────
    //
    // Opens the TCP control connection. For `rtsps://` this would go
    // through rustls (requires the `tls` cargo feature on tst-rtp); plain
    // `rtsp://` is a bare TCP connection.
    //
    // `RtspClientBuilder` is available when you need explicit timeouts,
    // custom User-Agent, or inline auth that's not in the URL. For the
    // common case `RtspClient::connect` reads everything from the URL.
    let mut client = RtspClient::connect(url)?;

    // ── (2) DESCRIBE ──────────────────────────────────────────────────────
    //
    // Fetches the SDP. We inspect it only to count m-lines (for the
    // diagnostic log); `setup_h264_auto` does the real SDP parsing.
    let sdp = client.describe()?;
    eprintln!(
        "SDP: {} m-line(s) (looking for exactly one H.264 rtpmap)",
        sdp.media.len()
    );

    // ── (3) SETUP: setup_h264_auto ────────────────────────────────────────
    //
    // What this does:
    //   a) Calls `pick_h264(&sdp)` — scans all m-lines for `a=rtpmap:N H264/90000`.
    //      Fails with `RtspError::NoH264Media` if none found, or
    //      `RtspError::MultipleH264Media` if more than one is found (ambiguous;
    //      explicit `setup(&media)` is the right tool for that case).
    //   b) Rejects packetization-mode 2 (interleaved — not implemented) with
    //      `RtspError::UnsupportedPacketizationMode(2)`. Modes 0 (single-NALU)
    //      and 1 (non-interleaved FU-A / STAP-A) proceed normally. Most cameras
    //      advertise mode 1.
    //   c) Issues SETUP over UDP (tries RTP_AVP first, auto-falls-back to
    //      TCP-interleaved on a 461 response, or immediately if `?transport=tcp`
    //      is in the URL). Returns the negotiated `RtspSession` + an
    //      `H264DepayConfig` ready for `into_h264_receiver`.
    //
    // What sprop-parameter-sets is:
    //   The SDP `a=fmtp:N sprop-parameter-sets=<base64>,<base64>,...` attribute
    //   carries the encoder's SPS and PPS NALUs out-of-band (before any RTP
    //   packets arrive). Many cameras omit them (they re-send in-band before the
    //   first IDR); some cameras only send them in the SDP. `setup_h264_auto`
    //   decodes these from base64 into raw Annex-B NALUs and stores them in
    //   `H264DepayConfig::initial_parameter_sets`. With
    //   `ParameterSetInjection::BeforeIdr` (the default), the depacketizer
    //   prepends the stored SPS/PPS before every IDR frame — giving decoders a
    //   clean self-contained start point even when the camera omits in-band
    //   parameter sets.
    let (session, depay_config) = client.setup_h264_auto(&sdp)?;
    eprintln!(
        "SETUP: transport={:?}, PT={}, sprop NALUs={}",
        session.transport_kind(),
        depay_config.payload_type,
        depay_config.initial_parameter_sets.len(),
    );

    // ── (4) into_h264_receiver ────────────────────────────────────────────
    //
    // Consumes the session into an `H264Receiver`. For UDP this takes the
    // SETUP-allocated RTP socket. For TCP-interleaved it takes the consumer
    // side of the pump's mpsc channel (the pump thread was spawned during
    // SETUP so no early frames are lost). The `depay_config` carries:
    //   - `payload_type` — filters out non-H.264 RTP packets by PT.
    //   - `initial_parameter_sets` — SPS/PPS from the SDP (may be empty).
    //   - `parameter_set_injection` — default `BeforeIdr` prepends them.
    let mut h264_rx = session.into_h264_receiver(depay_config);

    // ── (5) PLAY ──────────────────────────────────────────────────────────
    //
    // Sends PLAY; the server starts emitting RTP packets. The returned
    // `RtpInfo` may carry the first sequence number and RTP timestamp the
    // server will use (useful for tight clock-alignment). Many cameras
    // omit both; the receiver tolerates `None` for either field.
    let play_info = client.play()?;
    eprintln!(
        "PLAY: first_seq={:?}, first_rtptime={:?}",
        play_info.seq, play_info.rtptime,
    );

    // ── (6) Gateway loop: recv_au → Muxer → .ts file ─────────────────────
    //
    // `MuxerConfig::default()` opens program 1 with video PID 0x1011
    // (H.264) and KLV PID 0x1031. We push only video here; KLV is absent
    // in a bare H.264 RTSP stream.
    //
    // MISB gateway shape: if this were a STANAG 4609 gateway the KLV would
    // arrive on a separate feed (e.g. a UDP push from the sensor or a
    // second RTSP m-line). You would pair it with the video PTS using
    // `tst_pipeline::pairing::Pairer`, then call `mux.push_klv(&klv, pts,
    // 0x00)` to interleave it into the same program. See `pair_sync_klv.rs`
    // for the pairing recipe.
    let mut mux = Muxer::new(MuxerConfig::default()).expect("valid default muxer config");
    let mut out = File::create(&out_path)?;
    // 7 TS packets at 188 bytes each = 1316 bytes, matching SRT's default
    // payload size. The muxer emits whole TS packets; the buffer size
    // determines how many come back per `pull` call.
    let mut ts_buf = [0u8; 1316];
    let mut au_count = 0u64;
    let mut ts_bytes = 0u64;

    loop {
        // `recv_au` blocks the calling thread until a complete H.264 AU is
        // reassembled from one or more RTP packets (FU-A mode) or is
        // already complete in a single packet (single-NALU / STAP-A mode).
        // It returns:
        //   Ok(Some(au)) — a ready AU; loop continues.
        //   Ok(None)     — EOS: for TCP-interleaved sessions this signals a
        //                  clean TEARDOWN or cancel. Plain UDP sessions have
        //                  no EOS signal; `recv_au` blocks indefinitely unless
        //                  the cancel handle is set externally.
        //   Err(e)       — hard I/O error; propagate.
        let au = match h264_rx.recv_au()? {
            Some(a) => a,
            None => {
                eprintln!("EOS from H264Receiver — done.");
                break;
            }
        };
        au_count += 1;

        // AU-level PTS passes straight through to push_video.
        //
        // The RTP clock is 90 kHz, same as MPEG-TS PTS — no rescaling
        // needed. The depacketizer derives `au.pts` from the RTP timestamp
        // of the first packet in each AU.
        //
        // B-frame caveat: H.264 can use decode-order timestamps (DTS ≠
        // PTS) when B-frames are present. This example uses the RTP
        // timestamp as PTS directly, which is correct for low-latency live
        // encoders (no B-frames, PTS == DTS). If the source uses B-frames,
        // use `Muxer::push_video_with_dts` (or the `*_wire_to_with_dts`
        // variant) and derive DTS from the encoder's coded-order metadata.
        // See `docs/reference/conventions.md` for the DTS story.
        mux.push_video(&au.annexb, au.pts, au.key_frame)
            .expect("push_video");

        if au_count % 100 == 0 {
            eprintln!(
                "  {au_count} AUs received, key_frame={}, pts={}",
                au.key_frame,
                au.pts.as_ticks(),
            );
        }

        // Drain the muxer after every push to keep memory bounded.
        // `pull` returns 0 when there's nothing more to emit right now,
        // otherwise a multiple of 188 (one or more whole TS packets).
        loop {
            let n = mux.pull(&mut ts_buf);
            if n == 0 {
                break;
            }
            out.write_all(&ts_buf[..n])?;
            ts_bytes += n as u64;
        }
    }

    // ── (7) Teardown ──────────────────────────────────────────────────────
    //
    // Dropping `client` sends RTSP TEARDOWN (best-effort). The explicit
    // `drop` is for clarity; it happens at end-of-scope anyway.
    drop(client);

    // Print final stats from the RFC 6184 depacketizer (AU counts, sequence
    // gaps, parameter-set updates, etc.).
    let depay = h264_rx.depay_stats();
    eprintln!(
        "Done: {au_count} AUs, {ts_bytes} TS bytes written to {}",
        out_path.display(),
    );
    eprintln!(
        "Depay stats: aus_emitted={}, aus_dropped={}, seq_gaps={}, param_updates={}",
        depay.aus_emitted, depay.aus_dropped, depay.seq_gaps, depay.parameter_set_updates,
    );
    Ok(())
}
