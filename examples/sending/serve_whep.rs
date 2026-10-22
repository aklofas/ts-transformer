//! serve_whep — MPEG-TS over SRT in, WebRTC (WHEP) out.
//!
//! An encoder pushes an MPEG-TS stream (H.264 or H.265 video plus MISB KLV)
//! to this program over SRT; browsers play it over WebRTC with every KLV
//! record locked to its frame (in an SEI message and on the `klv` data
//! channel). No media server sits in between: `WhepServer` answers the
//! WHEP offer and sends the media itself.
//!
//! # Run
//!
//! ```text
//! cargo run -p tst-examples --example serve_whep -- --help
//! cargo run -p tst-examples --example serve_whep
//! cargo run -p tst-examples --example serve_whep -- --advertise 203.0.113.5
//! cargo run -p tst-examples --example serve_whep -- --srt 'srt://:7000?mode=listener&latency=300' --h265
//! ```
//!
//! Defaults: SRT listener on `srt://:9000?mode=listener`, WHEP endpoint on
//! `whep://0.0.0.0:8889`, mount `/live`, H.264. A browser POSTs its offer
//! to `http://<this host>:8889/live`; the reference player under
//! `docs/webrtc/` does that and shows the KLV (see
//! `docs/cookbook/sending/whep-browser-playback.md`).
//!
//! `--advertise <ip>` is the address viewers send their media to: the one
//! they can reach, which behind NAT is the public address. With a wildcard
//! bind (the default `0.0.0.0`) and no `--advertise`, viewers are offered
//! only the loopback address, so the stream plays on this host alone. A
//! `--whep` URL that binds one specific address offers that address.
//!
//! # Feed it
//!
//! Any SRT caller that sends MPEG-TS works. From this repository:
//!
//! ```text
//! cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
//! ```
//!
//! or with ffmpeg: `ffmpeg -re -i input.ts -c copy -f mpegts srt://127.0.0.1:9000`.
//!
//! # Stopping
//!
//! The example adds no signal-handling dependency. Press Enter to stop:
//! the SRT receiver is cancelled, the loop ends and the WHEP server is
//! stopped so every viewer's session closes cleanly. With stdin closed or
//! redirected from `/dev/null` it runs until killed; a kill (Ctrl-C) ends
//! the process without `stop()`, and viewers notice the silence through
//! their own disconnect timeout instead.
//!
//! # Compile gate (not run in CI)
//!
//! Running needs a live SRT sender, so CI only compiles it:
//!
//! ```text
//! cargo build -p tst-examples --example serve_whep
//! ```

use std::env;
use std::error::Error;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::CancelSlot;
use tst_core::transport::{RecvTransport, TransportError};
use tst_pipeline::{FactoryCancel, Receiver, ReceiverConfig, ReconnectPolicy, ShellErrorKind};
use tst_srt::shells::managed_recv_transport_from_url;
use tst_srt::{SrtError, SrtUrl};
use tst_webrtc::{MountConfig, MountHandle, MountKind, VideoCodec, WhepServer};

/// One SRT live-mode message carries up to 1316 bytes: seven TS packets.
/// The receive loop pushes the mount one such batch at a time.
const PACKETS_PER_PUSH: usize = 7;

/// How often the stats line is printed while bytes flow.
const STATS_PERIOD: Duration = Duration::from_secs(2);

struct Args {
    srt_url: String,
    whep_url: String,
    video: VideoCodec,
}

fn usage() -> &'static str {
    "usage: serve_whep [--srt <srt-url>] [--whep <whep-url>] [--advertise <ip>] [--h265]\n\
     \n\
     --srt <url>        SRT URL to receive the TS on; listener mode binds and waits\n\
     \x20                  for the encoder (default srt://:9000?mode=listener)\n\
     --whep <url>       whep:// or wheps:// bind URL (default whep://0.0.0.0:8889)\n\
     --advertise <ip>   the address viewers send media to, added to the WHEP URL\n\
     \x20                  as ?advertise=<ip>; with the default wildcard bind and\n\
     \x20                  no --advertise, only this host can play\n\
     --h265             the stream's video is H.265 (default H.264)\n\
     \n\
     Viewers POST their WHEP offer to http://<host>:8889/live. Press Enter to stop."
}

fn parse_args() -> Result<Args, String> {
    let mut srt_url = "srt://:9000?mode=listener".to_string();
    let mut whep_url = "whep://0.0.0.0:8889".to_string();
    let mut advertise: Option<String> = None;
    let mut video = VideoCodec::H264;

    let mut iter = env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "--srt" => srt_url = iter.next().ok_or("--srt needs a URL")?,
            "--whep" => whep_url = iter.next().ok_or("--whep needs a URL")?,
            "--advertise" => advertise = Some(iter.next().ok_or("--advertise needs an address")?),
            "--h265" => video = VideoCodec::H265,
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    // `advertise` is a query key of the WHEP URL, so the flag is folded into
    // the URL: `?` when the URL has no query yet, `&` when it has one. The
    // server parses the result and refuses an address that is not an IP
    // literal, so no validation is repeated here.
    if let Some(ip) = advertise {
        let sep = if whep_url.contains('?') { '&' } else { '?' };
        whep_url = format!("{whep_url}{sep}advertise={ip}");
    }

    Ok(Args {
        srt_url,
        whep_url,
        video,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args().map_err(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    })?;

    // ── (1) The WHEP server and its mount ────────────────────────────────
    //
    // The server comes up first so that viewers can connect and wait for
    // the stream: a session attached to a mount with no frames yet simply
    // receives nothing until the first keyframe is pushed.
    //
    // The URL carries every server setting (media port, viewer cap, bearer
    // token, CORS origin, TLS files); the bind address and port are the
    // signalling (HTTP) side, and the media (UDP) port defaults to the same
    // number. Nothing else is configured in code.
    let server = WhepServer::bind(&args.whep_url)?;

    // `MountConfig::default()` is an elementary mount, fed access units and
    // KLV records one by one. This program has a muxed TS, so it keeps the
    // other defaults (audio dropped, KLV in SEI and on the data channel, a
    // 64 KiB SEI budget, a 2048-packet viewer queue) and swaps the kind:
    // an MP2T mount demultiplexes the stream itself. The video codec must
    // match the stream's: a stream of the other codec is counted in
    // `demux_nonconformant` and dropped, so viewers connect but never see
    // a frame. A browser that cannot decode the mount's codec is refused
    // with 406 (`sessions_rejected_codec`); the stream plays no part in
    // that negotiation.
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    cfg.video = args.video;
    let mount = server.add_mount("/live", cfg)?;
    println!(
        "WHEP endpoint http://{}/live (media on udp {}); {:?} video",
        server.local_addr(),
        server.media_addr(),
        args.video
    );

    // ── (2) Stop, from stdin ─────────────────────────────────────────────
    //
    // A `CancelSlot` is a latched cancel target: whatever handle is
    // installed in it is fired by `cancel()`, and a handle installed after
    // the cancel is fired at once. Two slots are needed, because they have
    // different owners:
    //
    //   - `factory_slot` belongs to the managed receiver's factory. Every
    //     accept (the first one and each re-accept after the encoder drops)
    //     installs the listener's wake handle in it and clears it when the
    //     accept returns. A handle this program put there would be
    //     replaced by the next re-accept, so nothing else goes in it.
    //   - `stop` belongs to this program. It receives the managed
    //     transport's cancel handle once the open has returned (3), and
    //     keeps it for the life of the receiver.
    //
    // Enter fires both. Before the first accept returns, `factory_slot`
    // wakes it and the open reports `ExplicitClose`. Afterwards `stop`
    // fires the managed cancel, which latches the receive loop, wakes a
    // parked recv or re-accept and closes the live socket; its latch
    // covers an Enter that lands between the open returning and the
    // install. EOF on stdin (closed or `/dev/null`) fires nothing: a
    // backgrounded run then keeps going until it is killed.
    let factory_slot = Arc::new(FactoryCancel::new());
    let stop = Arc::new(CancelSlot::new());
    {
        let factory_slot = Arc::clone(&factory_slot);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut line = String::new();
            if let Ok(n) = std::io::stdin().lock().read_line(&mut line)
                && n > 0
            {
                factory_slot.cancel();
                stop.cancel();
            }
        });
    }

    // ── (3) The SRT receiver ─────────────────────────────────────────────
    //
    // Listener mode: the URL's host is empty, so the wildcard address is
    // bound and the call waits for the encoder's handshake. The URL is
    // the whole SRT configuration (latency, passphrase, stream id, ...),
    // the same keys the C, Python and JVM bindings accept.
    //
    // The transport is the MANAGED receive decorator rather than a bare
    // socket: when the encoder drops (a restart, a link outage), the
    // decorator re-accepts on this thread and the receive loop resumes,
    // while the WHEP viewers keep their sessions and wait for the next
    // keyframe. A bare socket would end the stream at the first drop and
    // every viewer would have to reconnect. The default policy gives up
    // after ten failed re-opens with exponential backoff (100 ms to 10 s);
    // for a listener, a failed re-open means the bind failed, and waiting
    // for a peer is not an attempt.
    //
    // `factory_slot` is handed in as the factory's cancel slot, which
    // makes the first accept reachable from the stdin thread: Enter before
    // any encoder arrives returns `ExplicitClose` here instead of parking
    // forever.
    let url = SrtUrl::parse(&args.srt_url)?;
    println!(
        "waiting for an SRT sender at {} in {:?} mode (press Enter to stop)",
        if url.host.contains(':') {
            format!("[{}]:{}", url.host, url.port)
        } else {
            format!("{}:{}", url.host, url.port)
        },
        url.mode
    );
    let managed = match managed_recv_transport_from_url(
        &url,
        ReconnectPolicy::default(),
        Arc::clone(&factory_slot),
    ) {
        Ok(t) => t,
        Err(SrtError::Transport(TransportError::ExplicitClose)) => {
            println!("stopped before a sender connected");
            server.stop();
            return Ok(());
        }
        Err(e) => return Err(Box::new(e)),
    };

    // The observers are taken BEFORE the transport moves into the shell,
    // the way the bindings do it. The cancel handle latches the close,
    // wakes a parked re-accept and closes the live socket, all from any
    // thread; installing it in `stop` routes a later Enter to it, or
    // fires it now if Enter already happened during the accept. The
    // reconnect counter tells the loop when the socket was replaced.
    let cancel = managed
        .cancel_handle()
        .expect("ManagedRecvTransport always has a cancel handle");
    let reconnects = managed.reconnects_handle();
    stop.install(cancel);

    // `Receiver` is the TS-bytes shell: it reads whatever the transport
    // delivers, finds the 0x47 sync pattern and hands back one aligned
    // 188-byte packet per call. The mount's demuxer would resynchronise
    // on its own too; the shell's syncer keeps the bytes it receives from
    // being pushed before alignment is known, and counts what it skipped.
    let mut rx = Receiver::new(managed, ReceiverConfig::default());
    println!("sender connected; serving /live");

    // ── (4) Receive and push ─────────────────────────────────────────────
    //
    // `push_ts` takes bytes in any chunking: a TS packet may span two
    // calls. The loop batches seven packets (one SRT message's worth) per
    // push so that the mount runs one demux pass per message rather than
    // per packet. The push never blocks: the demuxer runs on this thread
    // and each access unit is queued to the server's media thread, where
    // a full queue drops the frame and counts it in
    // `viewer_packets_dropped_queue_full`.
    //
    // The mount emits an access unit when the next one starts, so the
    // viewers run one frame behind the bytes received here.
    let mut batch: Vec<u8> = Vec::with_capacity(PACKETS_PER_PUSH * 188);
    let mut seen_reconnects = 0u64;
    let mut last_stats = Instant::now();
    let outcome = loop {
        match rx.next_packet() {
            Ok(pkt) => {
                // The socket was replaced during this call. The syncer
                // now holds the old connection's tail and the new one's
                // first message (this packet among them); `reset_sync`
                // drops both, by choice, so the old tail never enters the
                // new stream's demux. The mount recovers at the next
                // PAT/PMT and keyframe, which it waits for anyway. The
                // pending batch is old-connection bytes and goes too.
                let n = reconnects.load(Ordering::Acquire);
                if n != seen_reconnects {
                    seen_reconnects = n;
                    rx.reset_sync();
                    batch.clear();
                    println!("sender reconnected ({n} so far)");
                    continue;
                }
                batch.extend_from_slice(&pkt);
                if batch.len() >= PACKETS_PER_PUSH * 188 {
                    mount.push_ts(&batch)?;
                    batch.clear();
                }
            }
            // `Closed` is OUR cancel (Enter): the handle fired, the loop
            // is done. `EndOfStream` is the reconnect budget running out:
            // the transport can never deliver again. Anything else is a
            // transport fault the decorator could not absorb.
            Err(e) => match e.kind {
                ShellErrorKind::Closed => break "stopped",
                ShellErrorKind::EndOfStream => break "sender gone, reconnect budget exhausted",
                _ => return Err(Box::new(e)),
            },
        }

        if last_stats.elapsed() >= STATS_PERIOD {
            last_stats = Instant::now();
            print_stats(&server, &mount, &rx);
        }
    };
    // Whatever was batched is still worth pushing: the demuxer may be able
    // to complete an access unit from it.
    if !batch.is_empty() {
        // `Closed` should not happen before `stop()` below runs; if it
        // does, `?` returns and `Drop` still stops the server.
        mount.push_ts(&batch)?;
    }

    // ── (5) Shutdown ─────────────────────────────────────────────────────
    //
    // `stop` closes every viewer session (the browser sees the peer
    // connection end), closes the mounts (the server's map keeps them) and
    // joins the server's two threads, waiting at most two seconds for each.
    // Dropping the server does the same; the explicit call makes the order
    // clear: the last stats line reads the counters before they are frozen.
    print_stats(&server, &mount, &rx);
    println!("{outcome}");
    server.stop();
    Ok(())
}

/// One line per period. `server.stats()` asks the media thread for the
/// session counters and may wait up to 2 s for its reply; `mount.stats()`
/// and `rx.stats()` read counters in place and never wait.
fn print_stats(server: &WhepServer, mount: &MountHandle, rx: &Receiver<impl RecvTransport>) {
    let s = server.stats();
    let m = mount.stats();
    let r = rx.stats();
    println!(
        "srt packets={} resyncs={} | viewers={} (waiting_keyframe={}) sessions={}/{} \
         frames={} keyframes={} pts_reordered={} klv={} sei={} dc={} \
         rtp_packets={} queue_drops={} nonconformant={}",
        r.packets_received,
        r.resync_events,
        m.viewers,
        m.viewers_waiting_keyframe,
        s.active_sessions,
        s.sessions_total,
        m.frames_pushed,
        m.keyframes_pushed,
        m.frames_pts_reordered,
        m.klv_records_pushed,
        m.klv_sei_inserted,
        m.klv_dc_messages_sent,
        s.rtp_packets_sent,
        m.viewer_packets_dropped_queue_full,
        m.demux_nonconformant,
    );
}
