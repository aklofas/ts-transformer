//! The `?recv_timeout=<ms>` URL knob on `RtspUrl` must reach the
//! `RtpRecvTransport` returned by `RtspSession::into_recv_transport` — not
//! just the raw `rtp://` construction path covered in `transport.rs`'s
//! unit tests.
//!
//! Uses the loopback RTSP fixture with an empty `play_data` (no RTP ever
//! arrives after PLAY), so a session with no configured deadline would
//! block `recv_bytes` forever. The fixture default SDP is PT=33 MP2T,
//! same as `setup_play.rs`'s `setup_mp2t_auto` tests.

use tst_core::transport::{RecvTransport, TransportError};

use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle};

#[test]
fn recv_timeout_query_arms_into_recv_transport() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?recv_timeout=200", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    client.play().unwrap();
    let mut transport = session.into_recv_transport();

    let mut buf = vec![0u8; 2048];
    let result = transport.recv_bytes(&mut buf);

    match result {
        Err(TransportError::Backpressure { .. }) => {}
        other => panic!("expected Backpressure on expiry, got {other:?}"),
    }

    client.teardown().unwrap();
    drop(client);
    drop(h);
}
