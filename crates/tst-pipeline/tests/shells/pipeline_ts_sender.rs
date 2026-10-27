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

fn wire_sizes(t: &crate::common::ScriptedSend) -> Vec<usize> {
    t.wire.lock().unwrap().iter().map(Vec::len).collect()
}

#[test]
fn sender_splits_bundles_to_a_smaller_ceiling() {
    for (max, sizes) in [
        (188, vec![188; 7]),
        (1128, vec![1128, 188]),
        (1316, vec![1316]),
    ] {
        let t = crate::common::ScriptedSend::ok().with_max_payload(max);
        let mut s = Sender::new(t.clone(), SenderConfig::default());
        let ts = synthetic_ts_packets(7);
        s.send_ts(&ts).unwrap();
        assert_eq!(wire_sizes(&t), sizes, "max {max}");
        assert_eq!(t.wire_bytes(), ts);
    }
}

#[test]
fn sender_refuses_a_ceiling_below_one_packet_without_consuming() {
    let t = crate::common::ScriptedSend::ok().with_max_payload(100);
    let mut s = Sender::new(t, SenderConfig::default());
    let e = s.send_ts(&synthetic_ts_packets(7)).unwrap_err();
    assert_eq!(e.input_consumed, Some(false));
    assert!(matches!(
        e.source,
        tst_pipeline::SenderErrorSource::Transport(tst_pipeline::TransportError::TooLarge {
            len: 188,
            max: 100
        })
    ));
    assert_eq!(s.stats().bytes_pushed, 0);
}

#[test]
fn a_failure_between_split_slices_resends_only_the_rest() {
    use crate::common::SendStep;
    let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        SendStep::Ok,
        SendStep::Backpressure,
    ])));
    let wire = std::sync::Arc::default();
    let t = crate::common::ScriptedSend::scripted(script, wire).with_max_payload(1128);
    let mut s = Sender::new(t.clone(), SenderConfig::default());
    let ts = synthetic_ts_packets(7);
    let e = s.send_ts(&ts).unwrap_err();
    assert_eq!(e.input_consumed, Some(true));
    s.flush().unwrap();
    assert_eq!(t.wire_bytes(), ts);
    assert_eq!(wire_sizes(&t), vec![1128, 188]);
}

#[test]
fn default_ceiling_messages_are_unchanged() {
    let t = crate::common::ScriptedSend::ok();
    let mut s = Sender::new(t.clone(), SenderConfig::default());
    s.send_ts(&synthetic_ts_packets(10)).unwrap();
    assert_eq!(wire_sizes(&t), vec![1316]);
    s.flush().unwrap();
    assert_eq!(wire_sizes(&t), vec![1316, 564]);
}
