//! Clean RTSP teardown must end the `DemuxReceiver` path as a
//! clean `Ok(None)` end-of-stream, not a `TransportBroken` error.
//!
//! Companion regression pin to `stream_end_reason.rs`'s
//! `pump_read_error_records_transport_failed`: that test already checks
//! the raw `RtpRecvTransport::recv_timeout` shape after a wire reset;
//! this file checks the SAME two scenarios (clean TEARDOWN vs. abortive
//! RST) one layer up, through `DemuxReceiver::recv_event`, which is what
//! the `MPSC_PUMP_DISCONNECTED` remap in `transport.rs`'s
//! `recv_bytes_inner` is actually for — a demux consumer must be able to
//! tell "the peer politely hung up" apart from "the wire broke" without
//! reaching for `end_reason()`.

use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use tst_pipeline::{DemuxReceiver, ShellErrorKind};
use tst_rtp::RtspClient;

use crate::fixtures::raw_rtsp_server::{answer_describe_and_setup, force_reset};
use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle};

/// Clean TEARDOWN: `DemuxReceiver::recv_event` must return `Ok(None)`
/// (clean end-of-stream), not surface the pump's `Broken` disconnect. A
/// bounded persistent deadline is a safety net only, so a regression
/// here fails fast (Backpressure/Broken) instead of hanging the test.
#[test]
fn clean_teardown_ends_demux_receiver_cleanly() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(Duration::from_secs(5)));
    let mut demux = DemuxReceiver::new(transport);

    client.teardown().unwrap();

    let result = demux.recv_event();
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None) after a clean TEARDOWN, got {result:?}"
    );

    drop(client);
    drop(h);
}

/// Regression pin: an abortive RST (not a clean TEARDOWN) must still
/// surface as `TransportBroken` — the CleanTeardown remap must not
/// blanket-convert every pump disconnect into a clean end-of-stream.
/// Mirrors `stream_end_reason.rs`'s `pump_read_error_records_transport_failed`
/// one layer up (through `DemuxReceiver` instead of the raw transport).
#[test]
fn wire_reset_ends_demux_receiver_as_transport_broken() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Ready-signal handshake — see the identical comment in
    // `stream_end_reason.rs`'s `pump_read_error_records_transport_failed`:
    // resetting before the client has fully consumed the SETUP response
    // races that read; only reset after `into_recv_transport()` returns.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(Duration::from_secs(5)));
    ready_tx.send(()).unwrap();
    let mut demux = DemuxReceiver::new(transport);

    let result = demux.recv_event();
    match result {
        Err(e) => assert_eq!(
            e.kind,
            ShellErrorKind::TransportBroken,
            "expected TransportBroken after a wire reset, got kind {:?}",
            e.kind
        ),
        other => panic!("expected Err(TransportBroken), got {other:?}"),
    }

    server.join().unwrap();
    drop(client);
}
