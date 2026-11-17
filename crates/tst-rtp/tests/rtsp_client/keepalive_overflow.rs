//! Regression: a long-lived TCP-interleaved receive session must survive
//! sustained keepalive OPTIONS responses.
//!
//! The failure it pins: the keepalive pinger of the time never read
//! responses, and the interleaved reader routed EVERY RTSP response into a
//! bounded control queue of 32. The caller's thread drained that queue
//! only while a request of its own was in flight — so on a receive-only
//! session (SETUP/PLAY then nothing but data), each keepalive 200 OK
//! accumulated until the 33rd overflowed the queue, which the reader
//! treated as a hostile control-response flood and answered by failing
//! the session. At the default 30 s cadence that killed every session at
//! exactly 16.5 minutes, surfacing to callers as a clean EOS.
//!
//! This test compresses the timeline: a ~20 ms keepalive cadence pushes
//! well past 33 responses within the first second, so a client that
//! queues them dies at ~1 s while one that consumes keepalive responses
//! survives to the 3 s watchdog.

use std::time::Duration;

use tst_core::transport::{RecvTransport, TransportError};

use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn interleaved_session_survives_sustained_keepalive_responses() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);

    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
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
    // ~150 keepalive responses have reached the client — 4.5× a queuing
    // client's kill threshold of 33.
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
