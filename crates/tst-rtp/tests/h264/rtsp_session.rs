//! RTSP session integration tests for `setup_h264_auto` + `into_h264_receiver`.
//!
//! Uses the existing loopback RTSP fixture (extended with `play_data` and
//! `track_setup` fields) to serve an H.264 SDP and push canned
//! TCP-interleaved `$`-frames after PLAY.
//!
//! Tests:
//! (1) Happy path: mode-1 SDP → `setup_h264_auto` succeeds → `into_h264_receiver`
//!     receives the SPS/PPS-injected AU from a bare-IDR RTP packet.
//! (2) Mode-2 rejection: `setup_h264_auto` returns
//!     `UnsupportedPacketizationMode(2)` BEFORE any SETUP request.

use base64::Engine as _;
use tst_core::transport::TransportError;
use tst_rtp::{
    ParameterSetInjection, RtspClient, RtspClientBuilder, RtspError, RtspTransportKind,
    RtspTransportPref,
};

use crate::common::packetize;
use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle};

/// Minimal but structurally valid SPS NALU (type 7, F=0, NRI=3).
/// Bytes chosen so that `parse_sps` does not hard-reject them.  The bytes
/// match the well-known Baseline Level 3.0 sequence parameter set
/// `67 42 C0 1E D9 00 A0 46 FC B8 04 00` (a real encoder's output).
fn sps_nalu() -> Vec<u8> {
    // type 7 (SPS): 0x67 | NRI=3→0x67 (NRI occupies bits [6:5]; NRI=3 → 0x60; type=7 → 0x07; combined 0x67)
    vec![0x67u8, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x46]
}

/// Minimal but structurally valid PPS NALU (type 8, F=0).
fn pps_nalu() -> Vec<u8> {
    // type 8 (PPS): 0x68
    vec![0x68u8, 0xCE, 0x38, 0x80]
}

/// Build an H.264 SDP body with `packetization-mode=<mode>` and the given
/// `sprop-parameter-sets` base64 strings.
fn h264_sdp(mode: u8, sprop_sps_b64: &str, sprop_pps_b64: &str) -> Vec<u8> {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=H264 test\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode={mode};sprop-parameter-sets={sprop_sps_b64},{sprop_pps_b64}\r\n\
         a=control:trackID=0\r\n"
    )
    .into_bytes()
}

/// Encode a raw RTP packet as a TCP-interleaved `$`-frame on channel 0.
fn interleaved_frame(rtp_pkt: &[u8]) -> Vec<u8> {
    // RFC 7826 §14: `0x24 <channel:u8> <length:u16 BE> <payload>`
    let len = rtp_pkt.len() as u16;
    let mut frame = vec![0x24u8, 0, (len >> 8) as u8, (len & 0xFF) as u8];
    frame.extend_from_slice(rtp_pkt);
    frame
}

/// Build the `play_data` blob: a sequence of interleaved frames.
fn build_play_data(pkts: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    for pkt in pkts {
        buf.extend(interleaved_frame(pkt));
    }
    buf
}

/// Build a minimal RTCP SR payload (28 bytes), routed on channel 1.
///
/// Content is arbitrary — the H.264 receiver discards RTCP frames; this
/// just needs to be non-empty so the pump routes it to `rtcp_tx`.
fn rtcp_sr_frame() -> Vec<u8> {
    // RFC 3550 §6.4.1: 28-byte SR (V=2, RC=0, PT=200, length=6 words, rest zero)
    let sr: [u8; 28] = [
        0x80, 200, 0x00, 0x06, // header: V=2,P=0,RC=0 | PT=200 | length=6
        0, 0, 0, 1, // SSRC of sender
        0, 0, 0, 0, // NTP timestamp MSW
        0, 0, 0, 0, // NTP timestamp LSW
        0, 0, 0, 0, // RTP timestamp
        0, 0, 0, 0, // sender's packet count
        0, 0, 0, 0, // sender's octet count
    ];
    // RFC 7826 §14: `0x24 <channel:u8> <length:u16 BE> <payload>`
    let len = sr.len() as u16;
    let mut frame = vec![0x24u8, 1, (len >> 8) as u8, (len & 0xFF) as u8];
    frame.extend_from_slice(&sr);
    frame
}

/// Happy path: mode-1 H.264 SDP → `setup_h264_auto` → `into_h264_receiver`
/// receives the injected SPS/PPS + IDR AU.
///
/// Flow:
///  1. Fixture serves H.264 SDP (mode=1, sprop=SPS,PPS).
///  2. Client: connect (`?transport=tcp`) → describe → `setup_h264_auto` →
///     play → `into_h264_receiver(config)`.
///  3. Fixture pushes a bare-IDR single-NALU RTP packet as play_data.
///  4. Receiver: `recv_au` yields an AU whose annexb equals
///     `[SC SPS SC PPS SC IDR]` (BeforeIdr injection from the sprop cache).
#[test]
fn setup_h264_auto_mode1_roundtrip() {
    let sps = sps_nalu();
    let pps = pps_nalu();
    let sps_b64 = base64::engine::general_purpose::STANDARD.encode(&sps);
    let pps_b64 = base64::engine::general_purpose::STANDARD.encode(&pps);
    let sdp_body = h264_sdp(1, &sps_b64, &pps_b64);

    // IDR NALU to packetize and push as canned data.
    const PT: u8 = 96;
    const SSRC: u32 = 0xABCD_1234;
    let idr_nalu = vec![0x65u8, 0xAA, 0xBB, 0xCC]; // type 5 = IDR
    let aus = vec![(90_000u32, vec![idr_nalu.clone()])];
    let rtp_pkts = packetize(&aus, 1400, 1, SSRC, PT);

    // Expected Annex B: SPS + PPS + IDR (BeforeIdr injection).
    let mut expected = Vec::new();
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&sps);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&pps);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&idr_nalu);

    let play_data = build_play_data(&rtp_pkts);

    let fixture = FixtureHandle::spawn(FixtureConfig {
        sdp_body,
        play_data,
        ..FixtureConfig::default()
    });

    let url = format!("rtsp://127.0.0.1:{}/?transport=tcp", fixture.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let (session, config) = client.setup_h264_auto(&sdp).unwrap();
    // setup_h264_auto returns BeforeIdr injection by default.
    assert_eq!(
        config.parameter_set_injection,
        ParameterSetInjection::BeforeIdr
    );
    assert_eq!(config.payload_type, PT);
    assert_eq!(config.initial_parameter_sets.len(), 2);

    client.play().unwrap();
    let mut rx = session.into_h264_receiver(config);

    // Wait for the AU.
    let au = rx
        .recv_au()
        .expect("recv_au should not error")
        .expect("should get AU");
    assert_eq!(
        au.annexb, expected,
        "AU Annex B mismatch (SPS/PPS injection not applied?)"
    );
    assert!(au.key_frame, "IDR AU must have key_frame=true");
    rx.close();
}

/// `RtspClientBuilder::transport_preference(ForceTcp)` must land the same
/// TCP-interleaved transport as the `?transport=tcp` URL query used by the
/// test above — the builder setter is a typed alternative to the query
/// string, not a separate code path, so it should negotiate identically.
#[test]
fn setup_h264_auto_builder_transport_preference_forces_tcp() {
    let sps = sps_nalu();
    let pps = pps_nalu();
    let sps_b64 = base64::engine::general_purpose::STANDARD.encode(&sps);
    let pps_b64 = base64::engine::general_purpose::STANDARD.encode(&pps);
    let sdp_body = h264_sdp(1, &sps_b64, &pps_b64);

    let fixture = FixtureHandle::spawn(FixtureConfig {
        sdp_body,
        ..FixtureConfig::default()
    });

    // No `?transport=` query on the URL — `transport_preference` is the
    // only source of the preference, and it must still win TCP.
    let url = format!("rtsp://127.0.0.1:{}/", fixture.port);
    let mut client = RtspClientBuilder::new(&url)
        .unwrap()
        .transport_preference(RtspTransportPref::ForceTcp)
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let (session, _config) = client.setup_h264_auto(&sdp).unwrap();
    assert_eq!(session.transport_kind(), RtspTransportKind::TcpInterleaved);
}

/// Mode-2 rejection: `setup_h264_auto` must return
/// `UnsupportedPacketizationMode(2)` BEFORE sending any SETUP request.
///
/// We verify that the fixture's `setup_count` remains 0 throughout the call.
#[test]
fn setup_h264_auto_mode2_rejected_before_setup() {
    let sps = sps_nalu();
    let pps = pps_nalu();
    let sps_b64 = base64::engine::general_purpose::STANDARD.encode(&sps);
    let pps_b64 = base64::engine::general_purpose::STANDARD.encode(&pps);
    // packetization-mode=2 → must be rejected.
    let sdp_body = h264_sdp(2, &sps_b64, &pps_b64);

    let fixture = FixtureHandle::spawn(FixtureConfig {
        sdp_body,
        track_setup: true,
        ..FixtureConfig::default()
    });

    let url = format!("rtsp://127.0.0.1:{}/?transport=tcp", fixture.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();

    let result = client.setup_h264_auto(&sdp);
    let err = match result {
        Ok(_) => panic!("setup_h264_auto with mode=2 must fail"),
        Err(e) => e,
    };
    assert!(
        matches!(err, RtspError::UnsupportedPacketizationMode(2)),
        "expected UnsupportedPacketizationMode(2), got {err:?}"
    );

    // Give the fixture a moment for any in-flight requests to arrive,
    // then verify no SETUP was sent.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let count = fixture
        .setup_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        count, 0,
        "SETUP must NOT be sent before UnsupportedPacketizationMode is returned"
    );
}

/// Regression: interleaved RTCP frames must NOT kill the session.
///
/// Before the fix, `into_h264_receiver` dropped `rtcp_rx`, so the pump's
/// `rtcp_tx.try_send()` returned `TrySendError::Disconnected` at the first
/// RTCP frame. The pump exited, dropping `data_tx`, which caused
/// `H264Receiver::recv_au` to surface the MPSC_PUMP_DISCONNECTED sentinel —
/// a false clean-EOS before any AU was received.
///
/// This test interleaves RTCP SR frames (channel 1) between RTP data
/// frames. The session must deliver all expected AUs despite the RTCP
/// frames; with `rtcp_rx` dropped it fails, because the pump's
/// `Err(mpsc::TrySendError::Disconnected(_)) => return` on the RTCP sender
/// ends it.
#[test]
fn interleaved_rtcp_frames_do_not_kill_session() {
    let sps = sps_nalu();
    let pps = pps_nalu();
    let sps_b64 = base64::engine::general_purpose::STANDARD.encode(&sps);
    let pps_b64 = base64::engine::general_purpose::STANDARD.encode(&pps);
    let sdp_body = h264_sdp(1, &sps_b64, &pps_b64);

    const PT: u8 = 96;
    const SSRC: u32 = 0xDEAD_BEEF;

    // Three single-NALU AUs: two non-IDR + one IDR.
    let au1 = vec![0x41u8, 0x01, 0x02]; // non-IDR slice, type 1
    let au2 = vec![0x41u8, 0x03, 0x04]; // non-IDR slice, type 1
    let au3 = vec![0x65u8, 0xAA, 0xBB]; // IDR slice, type 5
    let aus = vec![
        (90_000u32, vec![au1.clone()]),
        (93_003u32, vec![au2.clone()]),
        (96_006u32, vec![au3.clone()]),
    ];
    let rtp_pkts = packetize(&aus, 1400, 1, SSRC, PT);
    assert_eq!(rtp_pkts.len(), 3, "one packet per AU");

    // Interleave: RTCP SR before each RTP packet, and one after the last.
    let mut play_data = Vec::new();
    for pkt in &rtp_pkts {
        play_data.extend(rtcp_sr_frame()); // RTCP before every RTP
        play_data.extend(interleaved_frame(pkt));
    }
    play_data.extend(rtcp_sr_frame()); // trailing RTCP after all data

    let fixture = FixtureHandle::spawn(FixtureConfig {
        sdp_body,
        play_data,
        ..FixtureConfig::default()
    });

    let url = format!("rtsp://127.0.0.1:{}/?transport=tcp", fixture.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let (session, config) = client.setup_h264_auto(&sdp).unwrap();
    client.play().unwrap();
    let mut rx = session.into_h264_receiver(config);

    // Must receive all three AUs despite the interleaved RTCP frames.
    let got1 = rx
        .recv_au()
        .expect("recv_au error on AU1")
        .expect("expected AU1");
    assert_eq!(got1.annexb, [0, 0, 0, 1, 0x41, 0x01, 0x02], "AU1 mismatch");

    let got2 = rx
        .recv_au()
        .expect("recv_au error on AU2")
        .expect("expected AU2");
    assert_eq!(got2.annexb, [0, 0, 0, 1, 0x41, 0x03, 0x04], "AU2 mismatch");

    let got3 = rx
        .recv_au()
        .expect("recv_au error on AU3")
        .expect("expected AU3");
    // AU3 is an IDR: BeforeIdr injection prepends SPS + PPS.
    let mut expected3 = Vec::new();
    expected3.extend_from_slice(&[0, 0, 0, 1]);
    expected3.extend_from_slice(&sps);
    expected3.extend_from_slice(&[0, 0, 0, 1]);
    expected3.extend_from_slice(&pps);
    expected3.extend_from_slice(&[0, 0, 0, 1]);
    expected3.extend_from_slice(&au3);
    assert_eq!(got3.annexb, expected3, "AU3 (IDR) mismatch");
    assert!(got3.key_frame);

    rx.close();
}

/// The `?recv_timeout=<ms>` URL knob on `RtspUrl` must reach the
/// `H264Receiver` returned by `into_h264_receiver` — the H.264 sibling of
/// `rtsp_client/recv_timeout.rs`'s `into_recv_transport` test. No
/// `play_data` is configured, so a receiver with no configured deadline
/// would block `recv_au` forever.
#[test]
fn recv_timeout_query_arms_into_h264_receiver() {
    let sps = sps_nalu();
    let pps = pps_nalu();
    let sps_b64 = base64::engine::general_purpose::STANDARD.encode(&sps);
    let pps_b64 = base64::engine::general_purpose::STANDARD.encode(&pps);
    let sdp_body = h264_sdp(1, &sps_b64, &pps_b64);

    let fixture = FixtureHandle::spawn(FixtureConfig {
        sdp_body,
        ..FixtureConfig::default()
    });

    let url = format!(
        "rtsp://127.0.0.1:{}/?transport=tcp&recv_timeout=200",
        fixture.port
    );
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let (session, config) = client.setup_h264_auto(&sdp).unwrap();
    client.play().unwrap();
    let mut rx = session.into_h264_receiver(config);

    let result = rx.recv_au();

    match result {
        Err(TransportError::Backpressure { .. }) => {}
        other => panic!("expected Backpressure on expiry, got {other:?}"),
    }

    rx.close();
}
