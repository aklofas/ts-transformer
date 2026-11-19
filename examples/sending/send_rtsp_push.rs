//! RTSP push: publish a stream TO an RTSP server.
//!
//! In the other RTSP client examples the application pulls: it sends
//! DESCRIBE / SETUP / PLAY and the server sends media. Here the media goes
//! the other way. The application connects to a server (mediamtx, this
//! library's own `RtspServer`, ffmpeg in listen mode), describes what it
//! will send with ANNOUNCE, sets each track up with SETUP `mode=record`,
//! sends RECORD (RFC 2326 §10.3, §10.11), and then sends the media as RTP
//! interleaved on the same TCP connection.
//!
//! Two forms, one function each:
//!
//! - `ts` (the default): one MPEG-TS track (RFC 2250). `RtspPush` is a
//!   `Transport`, so a `MuxSender` owns it and the application calls the
//!   same `send_video` / `send_klv` it would call over SRT or UDP. The TS
//!   reaches the server unchanged.
//! - `es`: elementary H.264 (RFC 6184) and a KLV track (RFC 6597), sent
//!   with `RtspPush::send_video` / `send_klv`. The server sees two tracks
//!   and places each KLV unit against the video from the push's RTCP
//!   sender reports.
//!
//! Both send 10 s of synthetic 30 fps H.264 (a key frame every second)
//! with one MISB ST 0601 record per frame, paced in real time, and then
//! end the session with TEARDOWN.
//!
//! # Against this library's server
//!
//! `recv_rtsp_publish` runs an `RtspServer` on port 8554 that turns any
//! name a publisher announces into a publish mount and prints what it
//! demuxes from each push:
//!
//! ```text
//! cargo run -p tst-examples --example recv_rtsp_publish            # terminal 1
//! cargo run -p tst-examples --example send_rtsp_push -- \
//!     rtsp://127.0.0.1:8554/cam-ts ts                               # terminal 2
//! cargo run -p tst-examples --example send_rtsp_push -- \
//!     rtsp://127.0.0.1:8554/cam-es es
//! ```
//!
//! That server takes MPEG-TS, H.264, and H.264 + KLV. It answers an
//! H.265 track, or KLV without video, with `415 Unsupported Media Type`.
//!
//! # Against mediamtx
//!
//! mediamtx's bundled `mediamtx.yml` listens for RTSP on port 8554 and
//! lets anyone publish (read on mediamtx 1.21.4):
//!
//! ```text
//! ./mediamtx                                                        # terminal 1
//! cargo run -p tst-examples --example send_rtsp_push -- \
//!     rtsp://127.0.0.1:8554/demo ts                                 # terminal 2
//! ffprobe -v quiet -rtsp_transport tcp -of compact \
//!     -show_entries stream=codec_type,codec_name,width,height \
//!     rtsp://127.0.0.1:8554/demo                                    # terminal 3, while it runs
//! ```
//!
//! mediamtx serves a path only while it has a recording publisher, and a
//! reader gets what arrives after its PLAY: start the reader while the
//! push runs. ffprobe then lists a 320x240 H.264 stream and the KLV
//! stream (`klv` in the `ts` form; a data stream it has no codec for in
//! the `es` form). Without `-v quiet` it also logs decode errors: the
//! slices here are filler, not pictures.
//!
//! # Against ffmpeg in listen mode
//!
//! ```text
//! ffmpeg -rtsp_flags listen -rtsp_transport tcp \
//!     -i rtsp://127.0.0.1:8554/demo -map 0 -c copy -f mpegts out.ts  # start first
//! cargo run -p tst-examples --example send_rtsp_push -- rtsp://127.0.0.1:8554/demo ts
//! ```
//!
//! ffmpeg 6.1's listener takes one connection and exits when it ends. It
//! answers TEARDOWN with `501 Not Implemented`: `RtspPush::close()`
//! returns that as `RtspError::Protocol { code: 501, .. }`, which the
//! `es` form prints as a warning; `MuxSender::finish()` in the `ts` form
//! does not report a TEARDOWN's answer. Use the `ts` form with ffmpeg:
//! with the `es` form's filler slices, ffmpeg 6.1's `-c copy -f mpegts`
//! output stopped at the first access unit ("first pts and dts value must
//! be set").
//!
//! # What you should see
//!
//! A `recording` line once the server has accepted ANNOUNCE, SETUP and
//! RECORD, a progress line every second, the push's counters, and how the
//! session ended. The `es` form's counters include the sender reports
//! sent and the receiver reports the server sent back; neither mediamtx
//! 1.21.4 nor ffmpeg 6.1 sends any to a TCP publisher, so that one reads 0.
//!
//! # Compile gate (not run in CI)
//!
//! Running needs a server, so CI only compiles it:
//!
//! ```text
//! cargo build -p tst-examples --example send_rtsp_push
//! ```

use std::env;
use std::error::Error;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::MuxSender;
use tst_rtp::{PushDescription, RtspClientBuilder, RtspError, RtspPush, VideoTrack};

/// 30 frames a second on the 90 kHz clock MPEG-TS and RTP video share.
const FRAME_TICKS: i64 = 3_000;
const FRAME_INTERVAL: Duration = Duration::from_micros(33_333);
/// Ten seconds of frames, a key frame every second.
const FRAMES: i64 = 300;
const KEY_FRAME_EVERY: i64 = 30;

fn main() -> Result<(), Box<dyn Error>> {
    // ── (0) Arguments ────────────────────────────────────────────────────
    //
    // Argv layout: [<rtsp-url>] [ts|es]. The URL names the path to publish
    // on; it may carry credentials (`rtsp://user:pass@host:8554/demo`),
    // which the client sends when the server challenges the ANNOUNCE.
    let args: Vec<String> = env::args().collect();
    let url = args
        .get(1)
        .map_or("rtsp://127.0.0.1:8554/demo", String::as_str);
    let mode = args.get(2).map_or("ts", String::as_str);

    // ── (1) The client builder ───────────────────────────────────────────
    //
    // `RtspClientBuilder` rather than `RtspClient::connect(url)`: only a
    // client made by the builder sends keepalive pings (OPTIONS on the
    // control connection). A push does not need them while media flows,
    // but a server may end a publisher it hears nothing from: mediamtx
    // 1.21.4 closes a recording publisher after 10 s without a byte, while
    // it advertises a 55 s session timeout, and the default ping cadence
    // (half the advertised timeout) would come too late. A 5 s ping keeps
    // a push that pauses its media longer than that.
    //
    // The push itself is TCP only: the media rides the control connection
    // (`RTP/AVP/TCP`, interleaved). A `?transport=udp` URL is refused.
    let builder = RtspClientBuilder::new(url)?.keepalive_interval(Duration::from_secs(5));

    match mode {
        "ts" => push_mpeg_ts(builder, url),
        "es" => push_elementary(builder, url),
        other => Err(format!("unknown form {other:?}: use `ts` or `es`").into()),
    }
}

/// The MPEG-TS form: a `MuxSender` over the push.
fn push_mpeg_ts(builder: RtspClientBuilder, url: &str) -> Result<(), Box<dyn Error>> {
    // ── (2) Connect, ANNOUNCE, SETUP, RECORD ─────────────────────────────
    //
    // `RtspPush::connect` runs the four steps in one call. A server that
    // refuses one of them returns `RtspError::Protocol` with its status
    // (`404` no such path, `403` the path already has a publisher on this
    // library's server, `415` a codec it does not take), and the
    // connection is closed. `RtspClient::announce`, `setup_record` and
    // `record` are the same steps one at a time.
    let push = RtspPush::connect(builder, &PushDescription::Mp2t)?;
    println!("recording on {url} (session {})", push.session_id());

    // ── (3) A muxer that owns the push ───────────────────────────────────
    //
    // One program: H.264 on PID 0x1011, KLV on PID 0x1031 with a PTS on
    // each record so a receiver can pair it with its frame. The push takes
    // at most 1 316 bytes (seven TS packets) per RTP packet, and the
    // `MuxSender` sizes its sends from the transport's `max_payload`, so
    // nothing here deals with packet sizes. (mediamtx 1.21.4 drops an
    // MPEG-TS RTP payload above 1 440 bytes; ffmpeg 6.1 loses data above
    // 8 272.)
    let mut program = MuxerProgramConfigBuilder::new(1, 0x1000);
    program.add_video(0x1011, VideoCodec::H264);
    program.add_klv(0x1031, KlvStreamType::PrivateData, true);
    let mut config = MuxerConfig::builder();
    config.add_program(program.build());
    let sender = MuxSender::new(push, config.build()?)?;

    // ── (4) Send frames in real time ─────────────────────────────────────
    //
    // Pace the sends as an encoder would. A send returns once its packets
    // are queued for the client's I/O thread, not once they are on the
    // wire, so a loop without pacing would hand the server ten seconds of
    // media in a burst; mediamtx in its default configuration times what
    // it relays by arrival, so its readers would see that burst too.
    let start = Instant::now();
    for i in 0..FRAMES {
        wait_until(start + FRAME_INTERVAL * i as u32);
        let pts = Pts90khz::new(i * FRAME_TICKS);
        let key_frame = i % KEY_FRAME_EVERY == 0;
        // An error here ends the push: the server closed the connection
        // (`TransportError::Broken` inside the `MuxSenderError`), the
        // connection failed, or it took no byte for 5 s.
        sender.send_video(&access_unit(i), pts, key_frame)?;
        sender.send_klv(&klv_record(i)?, pts, 0)?;
        if key_frame && i > 0 {
            println!("{} s sent", i / KEY_FRAME_EVERY);
        }
    }
    if let Some(stats) = sender.socket_stats() {
        println!(
            "sent {} RTP packets, {} bytes",
            stats.packets_sent, stats.bytes_sent
        );
    }

    // ── (5) End without loss ─────────────────────────────────────────────
    //
    // `finish()` sends what the sender still holds, then closes the push,
    // which waits until every queued packet is written and then sends
    // TEARDOWN. `close()` on a `MuxSender` cancels first, by design, and
    // can drop packets queued a moment before: use it to stop at once, not
    // to end a stream. The shell does not report the TEARDOWN's answer;
    // the `es` form below, which closes the push itself, does.
    sender.finish()?;
    println!("finished: every queued packet written, TEARDOWN sent");
    Ok(())
}

/// The elementary form: H.264 and KLV sent through the push itself.
fn push_elementary(builder: RtspClientBuilder, url: &str) -> Result<(), Box<dyn Error>> {
    // ── (2) Describe the tracks ──────────────────────────────────────────
    //
    // ANNOUNCE carries the SPS and PPS in its SDP. `h264_from_key_frame`
    // takes them from a key frame as an encoder emits it (Annex B, with
    // its parameter sets in front). `klv: true` announces a KLV track
    // beside the video.
    let video = VideoTrack::h264_from_key_frame(&access_unit(0))?;
    let description = PushDescription::Elementary { video, klv: true };
    let mut push = RtspPush::connect(builder, &description)?;
    println!("recording on {url} (session {})", push.session_id());

    // ── (3) Send frames in real time ─────────────────────────────────────
    //
    // `send_video` takes one access unit in Annex B form, in decoding
    // order, with its presentation time; NAL units over 1 400 bytes are
    // fragmented. The parameter sets travel only where an access unit
    // carries them, so each key frame here carries them again for a
    // reader that joins late.
    //
    // `send_klv` takes one whole KLV unit (here a ST 0601 local set,
    // without a metadata access unit header) on the video's timeline. It
    // goes out as one RTP packet up to 65 523 bytes: mediamtx 1.21.4 keeps
    // only the last packet of a unit split over several.
    //
    // Both PTS values are taken as given: pass one that keeps counting.
    // A PTS read from MPEG-TS wraps at 33 bits and must be unwrapped
    // first; the sender reports follow it, and a PTS that jumps back moves
    // their clock back.
    //
    // A send waits when the server reads slower than the push sends. To
    // end such a wait from another thread, take `push.cancel_handle()`
    // before the loop and call `cancel()` on it: the send returns
    // `TransportError::ExplicitClose` within about 100 ms.
    let start = Instant::now();
    for i in 0..FRAMES {
        wait_until(start + FRAME_INTERVAL * i as u32);
        let pts = Pts90khz::new(i * FRAME_TICKS);
        push.send_video(&access_unit(i), pts)?;
        push.send_klv(&klv_record(i)?, pts)?;
        if i % KEY_FRAME_EVERY == 0 && i > 0 {
            println!("{} s sent", i / KEY_FRAME_EVERY);
        }
    }
    let stats = push.stats();
    println!(
        "sent {} RTP packets, {} bytes, {} sender reports; {} receiver reports came back",
        stats.packets_sent, stats.bytes_sent, stats.sender_reports_sent, stats.receiver_reports,
    );

    // ── (4) Close, and read the answer ───────────────────────────────────
    //
    // `close()` waits until every queued packet is written, sends
    // TEARDOWN (500 ms for it and its reply), then gives the client's
    // thread up to 2 s to end. `Ok(())`: the media was written and the
    // server accepted the TEARDOWN. `Protocol { code: 454, .. }`: the
    // server had forgotten the session and queued media was dropped.
    // Another status is a refused TEARDOWN: ffmpeg 6.1's listener answers
    // every TEARDOWN `501` and keeps recording until the connection
    // closes. Dropping a push instead of closing it does not wait: it
    // discards what is still queued.
    match push.close() {
        Ok(()) => println!("closed: TEARDOWN accepted"),
        Err(RtspError::Protocol { code: 454, reason }) => {
            return Err(
                format!("the server had forgotten the session ({reason}): media lost").into(),
            );
        }
        Err(RtspError::Protocol { code, reason }) => {
            eprintln!("warning: the server refused TEARDOWN ({code} {reason})");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Sleep until `due`; return at once when it has passed.
fn wait_until(due: Instant) {
    let now = Instant::now();
    if due > now {
        thread::sleep(due - now);
    }
}

/// An SPS (320x240, High 4:4:4 Predictive profile) and a PPS as an
/// encoder wrote them: each one NAL unit with its header byte and without
/// a start code.
const SPS: [u8; 24] = [
    0x67, 0xF4, 0x00, 0x0C, 0x91, 0x96, 0x81, 0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10,
    0x00, 0x00, 0x03, 0x01, 0xE0, 0xF1, 0x42, 0xAA,
];
const PPS: [u8; 5] = [0x68, 0xCE, 0x0F, 0x19, 0x20];
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Access unit `i` in Annex B form: on a key frame the SPS, the PPS and an
/// IDR slice; otherwise one P slice. The slices are filler, not decodable
/// pictures: the push and the servers carry bytes, and a real application
/// hands over what its encoder wrote. The IDR slice is larger than one RTP
/// packet, so the `es` form shows fragmentation.
fn access_unit(i: i64) -> Vec<u8> {
    let mut au = Vec::new();
    let slice = if i % KEY_FRAME_EVERY == 0 {
        for parameter_set in [&SPS[..], &PPS[..]] {
            au.extend_from_slice(&START_CODE);
            au.extend_from_slice(parameter_set);
        }
        filler_nal(0x65, 3_000, i) // nal_unit_type 5: IDR slice
    } else {
        filler_nal(0x41, 600, i) // nal_unit_type 1: non-IDR slice
    };
    au.extend_from_slice(&START_CODE);
    au.extend_from_slice(&slice);
    au
}

/// A NAL unit of `len` bytes behind `header`. No byte is zero, so the
/// filler never forms a start code inside the unit.
fn filler_nal(header: u8, len: usize, salt: i64) -> Vec<u8> {
    std::iter::once(header)
        .chain((1..len).map(|k| ((k as i64 + salt) % 251) as u8 + 1))
        .collect()
}

/// One MISB ST 0601 local set for frame `i`: a timestamp and a sensor
/// position that moves a little each frame.
fn klv_record(i: i64) -> Result<Vec<u8>, Box<dyn Error>> {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000 + i as u64 * 33_333),
        sensor_lat_deg: Some(38.5 + i as f64 * 1e-5),
        sensor_lon_deg: Some(-121.5 - i as f64 * 1e-5),
        ..Default::default()
    };
    Ok(encode_to_vec(&record)?)
}
