//! Unit-level integration tests for `pipeline::MuxSender` using a mock
//! `Transport`. End-to-end tests over a real Socket pair are in
//! `tst-srt`'s `tests/pipeline/pipeline_sender.rs`.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_test_helpers::mock_transport::MockTransport;

fn synthetic_h264_au() -> Vec<u8> {
    // 4-byte start code + IDR NAL header + 64 bytes of payload.
    // Just enough to look Annex-B framed; no semantic validity required.
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    buf.extend(std::iter::repeat(0xAA).take(64));
    buf
}

#[test]
fn sender_drives_video_through_transport() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let sender = MuxSender::new(transport, MuxerConfig::default()).unwrap();

    sender
        .send_video(&synthetic_h264_au(), Pts90khz::new(0), true)
        .unwrap();

    let captured = log.lock().unwrap();
    // Each captured entry should be exactly 1316 bytes (one 7-packet bundle)
    // OR the final partial bundle (smaller). At least one capture must
    // exist.
    assert!(
        !captured.is_empty(),
        "expected at least one outbound message"
    );
    for msg in captured.iter() {
        assert!(
            msg.len() <= 1316,
            "every message must fit in payload: got {}",
            msg.len()
        );
        assert!(
            msg.len() % 188 == 0,
            "every message must be a multiple of 188 (TS packet size): got {}",
            msg.len()
        );
    }
}

#[test]
fn sender_drives_klv_through_transport() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let sender = MuxSender::new(transport, MuxerConfig::default()).unwrap();

    let klv = vec![0xAB; 64];
    sender.send_klv(&klv, Pts90khz::new(0), 0x00).unwrap();

    let captured = log.lock().unwrap();
    assert!(!captured.is_empty());
}

#[test]
fn sender_close_marks_dead() {
    let transport = MockTransport::new(1316);
    let sender = MuxSender::new(transport, MuxerConfig::default()).unwrap();
    sender.close();
    assert!(!sender.is_alive());
}
