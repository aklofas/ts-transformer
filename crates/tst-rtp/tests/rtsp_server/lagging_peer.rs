//! Lagging-peer behavior.
//!
//! A slow/stalled peer must not block the muxer (or, by tokio broadcast's
//! per-receiver cursor semantics, other peers). The producer pushes through a
//! `broadcast::Sender`, which never blocks on a slow subscriber — it drops the
//! oldest frames for that subscriber and bumps `MountStats::frames_dropped_total`
//! (the per-peer→mount aggregation is unit-tested deterministically in
//! `fanout.rs::mount_total_aggregates_peer_drops`).
//!
//! This integration test reproduces the real wire scenario: a raw
//! TCP-interleaved peer that completes SETUP+PLAY and then STOPS reading its
//! socket. Its server-side fanout task blocks on the TCP write once the kernel
//! send buffer fills — yet the muxer push path stays responsive. If the push
//! path were ever coupled to peer drain (e.g. a bounded blocking channel), this
//! loop would stall and nextest's per-test timeout would fail it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::RtspServer;

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Read an RTSP response off `stream` up to the blank-line header terminator.
fn read_response(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).expect("read RTSP response");
        assert_ne!(n, 0, "server closed before sending a full response");
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Extract the bare session id from a `Session: <id>;timeout=N` header.
fn session_id(response: &str) -> String {
    for line in response.lines() {
        if let Some(rest) = line
            .strip_prefix("Session: ")
            .or_else(|| line.strip_prefix("session: "))
        {
            return rest.split(';').next().unwrap().trim().to_string();
        }
    }
    panic!("no Session header in SETUP response:\n{response}");
}

/// A stalled TCP-interleaved peer does not block the muxer push path.
#[test]
fn slow_peer_does_not_block_muxer() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let base = format!("rtsp://127.0.0.1:{port}/live");

    // Raw TCP-interleaved handshake. SETUP uses the bare mount path (the
    // server's extract_mount_path does not strip a trailing trackID segment).
    let mut peer = TcpStream::connect(("127.0.0.1", port)).unwrap();
    peer.write_all(
        format!(
            "SETUP {base} RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let setup_resp = read_response(&mut peer);
    assert!(
        setup_resp.starts_with("RTSP/1.0 200"),
        "SETUP failed:\n{setup_resp}"
    );
    let sid = session_id(&setup_resp);

    peer.write_all(format!("PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n").as_bytes())
        .unwrap();
    let play_resp = read_response(&mut peer);
    assert!(
        play_resp.starts_with("RTSP/1.0 200"),
        "PLAY failed:\n{play_resp}"
    );

    // From here the peer NEVER reads again — its server-side fanout task will
    // block on the TCP write once the kernel send buffer fills. `peer` stays in
    // scope so the connection (and the blocked fanout task) stays alive.

    // Minimal IDR NAL; the muxer doesn't parse the payload.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    // Push far more than the 256-frame default fanout capacity so the stalled
    // peer is genuinely overwhelmed. Each push is a non-blocking broadcast.
    let start = Instant::now();
    for i in 0..3000i64 {
        mount
            .push_video(&nal, Pts90khz::new(i * 3000), true)
            .expect("push must not fail or block on a stalled peer");
    }
    let elapsed = start.elapsed();

    // The 3000 non-blocking pushes complete in well under a second locally; a
    // generous bound still fails loudly (and well within nextest's per-test
    // timeout) if the push path ever became coupled to peer drain.
    assert!(
        elapsed < Duration::from_secs(10),
        "muxer push loop stalled behind the slow peer: took {elapsed:?}"
    );

    drop(peer);
    server.stop().ok();
}
