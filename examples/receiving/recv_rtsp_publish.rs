//! RTSP publisher ingest: let encoders PUSH into `RtspServer`.
//!
//! The other RTSP examples pull: the application is the RTSP client and
//! fetches a stream with DESCRIBE / SETUP / PLAY. This one inverts the
//! roles. The application runs the server, and a publisher (ffmpeg,
//! GStreamer, a hardware encoder) connects and pushes with ANNOUNCE /
//! SETUP `mode=record` / RECORD (RFC 2326 §10.3, §10.11). Each pushed
//! stream reaches the application as MPEG-TS through an ordinary
//! `RtpRecvTransport`, so the usual `DemuxReceiver` loop reads it, and
//! the same mount keeps serving PLAY readers (`ffplay rtsp://host/<name>`).
//!
//! What the example does:
//!
//! 1. Binds `rtsp://0.0.0.0:8554` (or the URL given as the first argument)
//!    with `accept_unregistered_publishers(true)`: any name a publisher
//!    announces is created on demand, no `add_publish_mount` call needed.
//! 2. Loops `next_publisher(1 s)`, which hands back each new mount.
//! 3. Per mount, spawns a thread that turns the mount into a
//!    `DemuxReceiver` and prints a summary of the demuxed events: video
//!    access units (first one, then every 30th) and the first KLV record,
//!    decoded as MISB ST 0601 when it is one.
//! 4. Every 5 s prints each mount's publisher and `PublishMountStats`.
//! 5. Stops when you press Enter: `server.stop()` ends every mount's
//!    transport with `Closed`, which ends each demux loop cleanly.
//!
//! # Push commands
//!
//! Start the example, then push from another terminal. `demo` is any
//! name; each distinct name becomes its own mount.
//!
//! ```text
//! cargo run -p tst-examples --example recv_rtsp_publish
//!
//! # ffmpeg, TCP-interleaved (elementary H.264, re-muxed by the server):
//! ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 \
//!     -c:v libx264 -preset ultrafast -tune zerolatency \
//!     -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/demo
//!
//! # ffmpeg, UDP (same stream, RTP over separate UDP sockets):
//! ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 \
//!     -c:v libx264 -preset ultrafast -tune zerolatency \
//!     -f rtsp -rtsp_transport udp rtsp://127.0.0.1:8554/demo
//!
//! # GStreamer, MPEG-TS over RTP (video + KLV muxed by the publisher).
//! # rtspclientsink payloads its input itself (rtpmp2tpay, MP2T/90000):
//! gst-launch-1.0 filesrc location=in.ts ! tsparse set-timestamps=true \
//!     ! rtspclientsink location=rtsp://127.0.0.1:8554/demo
//!
//! # GStreamer, elementary H.264 + KLV (two tracks, re-muxed by the server;
//! # rtspclientsink picks rtph264pay and rtpklvpay). The KLV PES packets
//! # must carry a PTS (synchronous KLV does): untimed KLV leaves every KLV
//! # RTP packet on one timestamp, which the server cannot place.
//! gst-launch-1.0 filesrc location=in.ts ! tsdemux name=d \
//!     d. ! queue ! h264parse ! s.sink_0 \
//!     d. ! queue ! meta/x-klv ! s.sink_1 \
//!     rtspclientsink name=s location=rtsp://127.0.0.1:8554/demo
//! ```
//!
//! ffmpeg cannot push KLV over RTSP in any shape (it announces elementary
//! tracks only and stops with `Unsupported codec klv`). To get KLV into a
//! publish mount, push from GStreamer as above, or send the TS over SRT or
//! UDP instead.
//!
//! # Stopping
//!
//! The example adds no signal-handling dependency. It stops when a line
//! arrives on stdin (press Enter). With stdin closed or redirected from
//! `/dev/null` it runs until killed.
//!
//! # Compile gate (not run in CI)
//!
//! Running needs a live publisher, so CI only compiles it:
//!
//! ```text
//! cargo build -p tst-examples --example recv_rtsp_publish
//! ```

use std::env;
use std::error::Error;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::klv::st0601;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_pipeline::DemuxReceiver;
use tst_rtp::{PublishMountHandle, RtspServer, RtspServerBuilder};

fn main() -> Result<(), Box<dyn Error>> {
    // ── (0) Bind URL from argv ───────────────────────────────────────────
    //
    // The server binds an IP literal (no DNS on the server side).
    // `0.0.0.0` accepts publishers from any interface; 8554 is the
    // conventional RTSP alternate port (554 needs privileges).
    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "rtsp://0.0.0.0:8554".to_string());

    // ── (1) Build the server ─────────────────────────────────────────────
    //
    // The builder's setters take `&mut self` and return `&mut Self`, so
    // they chain; `build` takes the builder by value, so call it on the
    // binding, not at the end of the chain.
    //
    // `accept_unregistered_publishers(true)` lets an ANNOUNCE on a name no
    // mount is registered under create a publish mount there. The new
    // mount's handle waits on a queue (64 deep) until `next_publisher`
    // takes it; an ANNOUNCE past that bound answers `503`. With the flag
    // off (the default) such an ANNOUNCE answers `404`, and the
    // application registers names up front with `add_publish_mount`.
    //
    // Security: with the flag on, anyone who can reach the port can create
    // mounts. Add `auth_basic` / `auth_digest_*` to require the server's
    // credential (readers and publishers share it).
    let mut builder = RtspServerBuilder::new(&url)?;
    builder.accept_unregistered_publishers(true);
    let server: RtspServer = builder.build()?;

    // `start` binds the listener and spawns the server's internal runtime.
    // Every server method used below is blocking and is called from plain
    // threads, never from inside a tokio runtime.
    server.start()?;
    println!(
        "listening on {} (push to rtsp://<host>:<port>/<name>; press Enter to stop)",
        server
            .local_addr()
            .map_or_else(|| url.clone(), |a| a.to_string()),
    );

    // ── (2) Stop flag, set from stdin ────────────────────────────────────
    //
    // A line on stdin sets the flag. EOF (stdin closed or `/dev/null`)
    // does NOT: a backgrounded run then keeps going until it is killed.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        thread::spawn(move || {
            let mut line = String::new();
            if let Ok(n) = std::io::stdin().lock().read_line(&mut line) {
                if n > 0 {
                    stop.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    // ── (3) Accept loop ──────────────────────────────────────────────────
    //
    // `next_publisher` waits up to the timeout for the next on-demand
    // mount and returns `Ok(None)` when none arrived. The 1 s timeout is
    // what lets this loop notice the stop flag and print stats on time;
    // it is not a liveness bound on publishers.
    //
    // The announcing publisher already holds the mount when it comes out
    // of the queue. A later ANNOUNCE on the same name (that publisher
    // reconnecting, or a new one after it left) reuses the mount and
    // queues nothing: the existing transport simply resumes. A second
    // publisher while the first is live is refused with `403`.
    let mut mounts: Vec<PublishMountHandle> = Vec::new();
    let mut workers = Vec::new();
    let mut last_stats = Instant::now();

    while !stop.load(Ordering::SeqCst) {
        match server.next_publisher(Duration::from_secs(1)) {
            Ok(Some(mount)) => {
                println!("[{}] new publish mount", mount.mount_path());
                // Keep a clone for stats: `into_recv_transport` consumes
                // its handle, and `stats()` / `publisher()` live on the
                // handle, not on the transport. Clones share one mount.
                mounts.push(mount.clone());
                workers.push(spawn_demux_thread(mount));
            }
            Ok(None) => {}
            // `Shutdown`: the server was stopped from elsewhere.
            Err(e) => {
                eprintln!("next_publisher: {e}");
                break;
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(5) {
            last_stats = Instant::now();
            print_stats(&server, &mounts);
        }

        // A real service would also expire idle names here: an on-demand
        // mount stays in the table after its publisher leaves, until
        // `server.remove_mount(path)` removes it. The table holds at most
        // 256 on-demand mounts; an ANNOUNCE past that answers `503`.
    }

    // ── (4) Shutdown ─────────────────────────────────────────────────────
    //
    // `stop` sends each session the server-initiated TEARDOWN notice,
    // closes it, and ends every publish mount's application transport
    // with `TransportError::Closed`. `DemuxReceiver` reports `Closed` as
    // end of stream, so each worker's `for` loop ends on its own.
    print_stats(&server, &mounts);
    server.stop()?;
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

/// Turn one publish mount into a demux loop on its own thread.
fn spawn_demux_thread(mount: PublishMountHandle) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let path = mount.mount_path().to_string();

        // `into_recv_transport` hands out the mount's application side as
        // an `RtpRecvTransport`, the same type an RTSP client session
        // produces, so `DemuxReceiver` reads it unchanged. It is
        // take-once per mount: a second call, from any clone, returns
        // `RtspServerError::TransportTaken`.
        //
        // The transport outlives publishers. When one leaves, it goes
        // quiet; when the next one arrives, bytes resume and the demuxer
        // sees ordinary continuity discontinuities. It ends only on
        // `cancel()` (`ExplicitClose`), `remove_mount` or `stop()`
        // (`Closed`).
        let transport = match mount.into_recv_transport() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[{path}] into_recv_transport: {e}");
                return;
            }
        };

        // Whatever the publisher pushed, this is MPEG-TS: an MPEG-TS
        // publisher's bytes pass through, and elementary H.264 (+ KLV)
        // tracks are re-muxed by the server into one program (video PID
        // 0x100, KLV PID 0x101).
        let mut demux = DemuxReceiver::new(transport);
        let mut video_aus = 0u64;
        let mut klv_records = 0u64;

        for ev in &mut demux {
            let event = match ev {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[{path}] demux error: {e}");
                    break;
                }
            };
            match event {
                DemuxEvent::Sample {
                    stream,
                    pts,
                    payload:
                        SamplePayload::Video {
                            codec,
                            raw,
                            random_access_indicator,
                            ..
                        },
                    ..
                } => {
                    video_aus += 1;
                    if video_aus == 1 || video_aus % 30 == 0 {
                        println!(
                            "[{path}] video AU #{video_aus} PID=0x{:04X} {codec:?} pts={} bytes={} rai={random_access_indicator}",
                            stream.pid,
                            pts.as_ticks(),
                            raw.len(),
                        );
                    }
                }
                DemuxEvent::Metadata {
                    stream,
                    pts,
                    payload,
                    ..
                } => {
                    klv_records += 1;
                    if klv_records == 1 {
                        // The demuxer hands over the bare KLV local set
                        // (any sync AU-cell wrapper already removed), so it
                        // goes straight to the ST 0601 decoder. Not every
                        // KLV stream is ST 0601; report and move on.
                        let summary = match st0601::decode(&payload) {
                            Ok(ls) => format!(
                                "ST 0601 timestamp_us={:?} sensor=({:?}, {:?})",
                                ls.timestamp_us, ls.sensor_lat_deg, ls.sensor_lon_deg,
                            ),
                            Err(e) => format!("not decodable as ST 0601 ({e})"),
                        };
                        println!(
                            "[{path}] first KLV PID=0x{:04X} pts={} bytes={}: {summary}",
                            stream.pid,
                            pts.as_ticks(),
                            payload.len(),
                        );
                    }
                }
                // A publisher change shows up here as continuity breaks on
                // each PID. Applications that care which publisher a frame
                // came from read `publisher()` / `generation()`.
                DemuxEvent::Discontinuity { stream, kind } => {
                    println!("[{path}] discontinuity PID=0x{:04X} {kind:?}", stream.pid);
                }
                _ => {}
            }
        }
        println!("[{path}] stream ended: {video_aus} video AUs, {klv_records} KLV records");
    })
}

/// Print the server totals and each mount's publisher + stats.
fn print_stats(server: &RtspServer, mounts: &[PublishMountHandle]) {
    let s = server.stats();
    println!(
        "server: active_publishers={} rtp_packets_received={} mounts={}",
        s.active_publishers, s.total_rtp_packets_received, s.mounts,
    );
    for m in mounts {
        // `publisher()` is `None` while the mount is idle (between
        // publishers). `generation` counts publishers that have ended.
        let who = match m.publisher() {
            Some(p) => format!(
                "publisher {} {:?} generation {}",
                p.peer, p.shape, p.generation
            ),
            None => "idle".to_string(),
        };
        // `alignment` says how KLV is placed on the video clock:
        // `NotApplicable` for MPEG-TS and video-only publishers, `Pending`
        // while KLV is held, `SenderReport` once RTCP sender reports for
        // both tracks arrived, `Provisional` after 2 s without them.
        // `aus_reordered` nonzero means the publisher sends B-frames,
        // which the re-muxed TS carries with a PTS and no DTS.
        let st = m.stats();
        println!(
            "[{}] {who}; alignment={:?} aus_emitted={} aus_dropped={} aus_reordered={} klv_units_emitted={} frames_emitted={} frames_dropped_app={} readers={}",
            m.mount_path(),
            st.alignment,
            st.aus_emitted,
            st.aus_dropped,
            st.aus_reordered,
            st.klv_units_emitted,
            st.frames_emitted,
            st.frames_dropped_app,
            st.peer_count,
        );
    }
}
