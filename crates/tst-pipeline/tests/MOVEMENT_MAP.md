# tst-pipeline integration-test movement map

The 29 former top-level `tests/*.rs` integration binaries are now **6**:
five domain harnesses `tests/<domain>.rs`, each `#[path]`-including its
member files from `tests/<domain>/`, plus `tests/conformance.rs`, the
transport conformance kit over mock transports, which stays a binary of its
own like the kit binary in every transport crate.

## What changed (and what did not)

- **Test bodies are unchanged by the move.** Each member file lost only its
  `mod common;` line (the root declares `common` once), and
  `shells/mux_sender_golden.rs`'s `include_bytes!` path gained one `../`.
- **Test paths gained a `<file>::` prefix** because each former file is now a
  module of its domain binary: `reconnect_background`'s
  `background_reconnect_drains_fifo_then_resumes_direct` is now
  `reconnect_background::background_reconnect_drains_fifo_then_resumes_direct`
  in binary `reconnect`. Filter with
  `cargo nextest run -p tst-pipeline --test reconnect reconnect_background::`.
- **Shared scaffolding lives in `tests/common/mod.rs`**: `ScriptedRecv`,
  `ScriptedSend` + `SendStep`, `FlagCancel`, `flag`, `broken`, `fast_policy`,
  `wait_for`, `build_h264_au`, `dummy_klv`, `drain_mux`, `to_ts_packets`.
- **`tests/support/round_trip.rs`** is not a test target; tst-udp, tst-tcp
  and tst-rist include it in their `pipeline_round_trip.rs`.
- **The kind-derivation table**: `errors/shell_error_kind_derivation.rs` holds
  one test, `every_inner_variant_routes_to_its_kind`, over 49 rows (32
  `MuxError`, 5 `TransportError` through each sender and each receiver
  shell, 5 `DemuxError`, 2 `TsFramingError`); a failure names the variant
  and the shell.

## Equivalence check

Before the move and after it, the multiset of test leaf names (the segment
after the last `::`) of `cargo nextest list -p tst-pipeline` is identical.

## Movement table

| old `tests/…` | new `tests/…` |
| --- | --- |
| `demux_receiver_malformed_pes_recovery.rs` | `shells/demux_receiver_malformed_pes_recovery.rs` |
| `demux_receiver_terminal_flush.rs` | `shells/demux_receiver_terminal_flush.rs` |
| `mux_sender_backpressure_tracing.rs` | `shells/mux_sender_backpressure_tracing.rs` |
| `mux_sender_golden.rs` | `shells/mux_sender_golden.rs` |
| `pipeline_raw_sender.rs` | `shells/pipeline_raw_sender.rs` |
| `pipeline_receiver.rs` | `shells/pipeline_receiver.rs` |
| `pipeline_sender_unit.rs` | `shells/pipeline_sender_unit.rs` |
| `pipeline_ts_sender.rs` | `shells/pipeline_ts_sender.rs` |
| `shell_lifetime_spans.rs` | `shells/shell_lifetime_spans.rs` |
| `managed_demux_receiver_reconnect.rs` | `managed/managed_demux_receiver_reconnect.rs` |
| `managed_demux_receiver_terminal_flush.rs` | `managed/managed_demux_receiver_terminal_flush.rs` |
| `managed_receive_attempts.rs` | `managed/managed_receive_attempts.rs` |
| `managed_receive_cancel_backoff.rs` | `managed/managed_receive_cancel_backoff.rs` |
| `managed_receive_cancel_factory.rs` | `managed/managed_receive_cancel_factory.rs` |
| `managed_receive_tracing.rs` | `managed/managed_receive_tracing.rs` |
| `managed_send_cancel_factory.rs` | `managed/managed_send_cancel_factory.rs` |
| `managed_send_inner_cancelled.rs` | `managed/managed_send_inner_cancelled.rs` |
| `poison_policy.rs` | `managed/poison_policy.rs` |
| `reconnect_background.rs` | `reconnect/reconnect_background.rs` |
| `reconnect_blocking_send_ownership.rs` | `reconnect/reconnect_blocking_send_ownership.rs` |
| `reconnect_cancel_parked_send.rs` | `reconnect/reconnect_cancel_parked_send.rs` |
| `reconnect_reject_overflow.rs` | `reconnect/reconnect_reject_overflow.rs` |
| `reconnect_tracing.rs` | `reconnect/reconnect_tracing.rs` |
| `pairing_demuxer_round_trip.rs` | `pairing/pairing_demuxer_round_trip.rs` |
| `pairing_round_trip.rs` | `pairing/pairing_round_trip.rs` |
| `shell_error_display.rs` | `errors/shell_error_display.rs` |
| `shell_error_kind_derivation.rs` | `errors/shell_error_kind_derivation.rs` |
| `transport_error_discrimination.rs` | `errors/transport_error_discrimination.rs` |
| `conformance.rs` | `conformance.rs` (unchanged; own binary) |

## Mocks kept local, and why

| File | Items | Why not in `common` |
| --- | --- | --- |
| `conformance.rs` | `Wire`, `MockSend`, `MockRecv`, `WireCancel`, `InCall`, `Interrupted`, `policy`, `policy_in` | the kit's own wire protocol and budget |
| `reconnect/reconnect_background.rs` | `Gate`, `GateState`, `GateCancel`, `GatedTransport`, `GatedRig`, `bg_policy` | parks inside `send_bytes` until released — the behaviour under test |
| `reconnect/reconnect_cancel_parked_send.rs` | `Gate`, `GateCancel`, `Behavior`, `Mock`, `policy` | same parking protocol, different release rules |
| `reconnect/reconnect_reject_overflow.rs` | `Link`, `Barrier`, `ReleaseOnDrop` | a two-party barrier on the wire |
| `managed/managed_send_cancel_factory.rs` | `RaceInner`, `DrainRaceInner` | fire the cancel from inside a send or drain |
| `managed/managed_receive_cancel_factory.rs` | `RaceInner`, `ParkedAccept`, `NotifyOnCancel` | a factory parked like a listener accept |
| `managed/managed_send_inner_cancelled.rs` | `Inner`, `Rebuild`, `Rig`, `policy` | per-case rebuild outcomes |
| `managed/managed_demux_receiver_terminal_flush.rs` | `CancelThenBroken` | fires the managed handle from inside `recv_bytes` |
| `managed/managed_receive_attempts.rs` | `Scripted` | delivers `b"data"` on every call, forever |
| `managed/poison_policy.rs` | `ParkedRecv`, `ParkedRecvCancel`, `fast_policy` | parks until its own cancel; 1 ms backoff and a 16-slot gap buffer |
| `managed/poison_policy.rs` | `OkTransport`, `OneShotBrokenThenOk` | function-scoped mocks for the lock-poison and one-shot cases; not targeted by the shared mock |
| `shells/pipeline_receiver.rs` | `minimal_h264_au` (6-byte AUD) | a different AU than `common::build_h264_au` |
| `shells/pipeline_sender_unit.rs`, `shells/mux_sender_golden.rs`, `shells/mux_sender_backpressure_tracing.rs`, `reconnect/reconnect_blocking_send_ownership.rs` | `synthetic_h264_au`, `synthetic_h264_idr`, `idr_nal`, `idr` | golden or sized inputs whose exact bytes the tests depend on |
