//! Regression: a long-lived TCP-interleaved receive session must survive
//! sustained keepalive OPTIONS responses.
//!
//! The failure it pins: the keepalive thread never reads responses, and
//! the interleaved pump used to route EVERY RTSP response into the
//! bounded ctrl queue (`CTRL_QUEUE_BOUND` = 32). The main thread only
//! drains that queue while a request of its own is in flight — so on a
//! receive-only session (SETUP/PLAY then nothing but data), each keepalive
//! 200 OK accumulated until the 33rd overflowed the queue, which the pump
//! treats as a hostile control-response flood and answers by failing the
//! session. At the default 30 s cadence that killed every session at
//! exactly 16.5 minutes, surfacing to callers as a clean EOS.
//!
//! This test compresses the timeline: a ~20 ms keepalive cadence pushes
//! well past 33 responses within the first second, so a queuing pump
//! dies at ~1 s while the pump that consumes keepalive responses
//! instead of queuing them survives to the 3 s watchdog.

use std::time::Duration;

use tst_core::transport::{RecvTransport, TransportError};

use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn interleaved_session_survives_sustained_keepalive_responses() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);

    // Tight read timeout so the pump's stream-lock hold per read cycle is
    // ~10 ms — the keepalive thread contends on the same mutex for its
    // writes, and the default 100 ms hold would stretch the effective ping
    // cadence well past the requested 20 ms.
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(Duration::from_millis(10))
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut recv = session.into_recv_transport();
    client.play().unwrap();

    // Start the keepalive only now, after SETUP/PLAY: spawning it at
    // connect time (the builder default) would race its first pings
    // against the DESCRIBE/SETUP exchanges, and this test targets the
    // steady receive-only state where the overflow actually happened.
    client
        .spawn_keepalive_if_needed(Some(Duration::from_millis(20)))
        .unwrap();

    // Watchdog: end the (data-less) blocking recv loop after 3 s. By then
    // ~150 keepalive responses have crossed the pump — 4.5× the queuing
    // pump's kill threshold of 33.
    let cancel = recv
        .cancel_handle()
        .expect("recv transport exposes a cancel handle");
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        cancel.cancel();
    });

    let start = std::time::Instant::now();
    let mut buf = vec![0u8; 65536];
    loop {
        match recv.recv_bytes(&mut buf) {
            // The fixture pushes no media after PLAY; any stray data is fine.
            Ok(_) => continue,
            // Watchdog fired — the session survived the full window.
            Err(TransportError::ExplicitClose) => break,
            Err(e) => panic!(
                "session died after {:?} while only keepalive responses were flowing \
                 (pre-fix signature: pump ctrl-queue overflow at ~33 responses): {e:?}",
                start.elapsed()
            ),
        }
    }
    watchdog.join().unwrap();
    drop(client);
    drop(h);
}
