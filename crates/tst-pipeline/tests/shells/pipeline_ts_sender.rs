//! Integration tests for `pipeline::Sender` over a mock transport.

use tst_pipeline::{Sender, SenderConfig, TsFramingMode};
use tst_test_helpers::mock_transport::MockTransport;

fn synthetic_ts_packets(n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * 188);
    for i in 0..n {
        buf.push(0x47);
        for j in 1..188 {
            buf.push(((i & 0xFF) as u8).wrapping_add(j as u8));
        }
    }
    buf
}

#[test]
fn ts_sender_passes_aligned_input() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let mut sender = Sender::new(transport, SenderConfig::default());

    sender.send_ts(&synthetic_ts_packets(7)).unwrap();

    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1, "expected one 7-packet bundle");
    assert_eq!(captured[0].len(), 1316);
}

#[test]
fn ts_sender_skips_misaligned_prefix() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let mut sender = Sender::new(transport, SenderConfig::default());

    let prefix = vec![0x80, 0x81, 0x82, 0x83];
    let mut input = prefix.clone();
    input.extend_from_slice(&synthetic_ts_packets(7));
    sender.send_ts(&input).unwrap();

    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(sender.stats().bytes_skipped_for_sync, prefix.len() as u64);
}

#[test]
fn ts_sender_flush_emits_partial_bundle() {
    let transport = MockTransport::new(1316);
    let log = transport.log();
    let mut sender = Sender::new(transport, SenderConfig::default());

    sender.send_ts(&synthetic_ts_packets(3)).unwrap();
    {
        let captured = log.lock().unwrap();
        assert!(captured.is_empty(), "3 < 7 → no bundle yet");
    }

    sender.flush().unwrap();
    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].len(), 188 * 3);
}

#[test]
fn ts_sender_strict_rejects_misaligned() {
    let transport = MockTransport::new(1316);
    let mut sender = Sender::new(transport, {
        let mut cfg = SenderConfig::default();
        cfg.framing_mode = TsFramingMode::Strict;
        cfg
    });

    let prefix = vec![0xAB, 0xCD];
    let mut input = prefix;
    input.extend_from_slice(&synthetic_ts_packets(3));
    let result = sender.send_ts(&input);
    assert!(result.is_err());
}

#[test]
fn ts_sender_recover_mode_errors_after_max_unsynced_bytes() {
    use tst_pipeline::sender::TsFramingError;
    use tst_pipeline::{SenderErrorSource, ShellErrorKind};

    let transport = MockTransport::new(1316);
    let mut sender = Sender::new(transport, {
        let mut cfg = SenderConfig::default();
        cfg.max_unsynced_bytes = 500;
        cfg
    });

    // 501 bytes, never 0x47 — one past the configured limit, RECOVER mode
    // (the default) still applies.
    let garbage = vec![0x00u8; 501];
    let err = sender.send_ts(&garbage).unwrap_err();

    assert_eq!(err.kind, ShellErrorKind::InputMalformed);
    // The garbage was already extended into the framing buffer before the
    // limit check fires — do not resend the same bytes.
    assert_eq!(err.input_consumed, Some(true));
    match err.source {
        SenderErrorSource::Framing(TsFramingError::NoSyncAfterLimit { max }) => {
            assert_eq!(max, 500);
        }
        other => panic!("expected Framing(NoSyncAfterLimit), got {other:?}"),
    }
}
