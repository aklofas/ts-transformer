//! Integration tests for `pipeline::RawSender` using a mock `Transport`.

use tst_pipeline::{
    RawSender, RawSenderConfig, RawSenderErrorSource, ShellErrorKind, TransportError,
};
use tst_test_helpers::mock_transport::MockTransport;

#[test]
fn raw_sender_passes_bytes_through() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let mut sender = RawSender::new(transport, RawSenderConfig::default());

    sender.send(b"hello").unwrap();
    sender.send(b"world").unwrap();

    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(&captured[0], b"hello");
    assert_eq!(&captured[1], b"world");
}

#[test]
fn raw_sender_rejects_oversize_message() {
    let transport = MockTransport::new(1316);
    let mut sender = RawSender::new(transport, RawSenderConfig::default());
    let big = vec![0u8; 1317];
    let err = sender.send(&big).unwrap_err();
    match err.source {
        RawSenderErrorSource::Transport(TransportError::TooLarge { len, max }) => {
            assert_eq!(len, 1317);
            assert_eq!(max, 1316);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn raw_sender_close_marks_dead() {
    let transport = MockTransport::new(1316);
    let mut sender = RawSender::new(transport, RawSenderConfig::default());
    sender.close();
    let err = sender.send(b"after close").unwrap_err();
    assert_eq!(err.kind, ShellErrorKind::Closed);
}
