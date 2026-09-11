//! Verify every inner-error variant routes through the correct
//! `kind_from_*` helper AND through the shell's `From<inner>` impl
//! to the expected `ShellErrorKind`. CI ratchet
//! `scripts/check/rust/pipeline-kind-classification.sh` enforces every
//! upstream inner variant is matched in the corresponding kind_from_*
//! helper before the wildcard; this test asserts the actual kind value
//! for each pairing.
//!
//! ~43 (variant, expected kind, via-shell) rows. Maintained as a
//! flat table; if a new inner variant is added upstream the CI ratchet
//! catches the missing match arm before this test runs.

use tst_core::error::{DemuxError, MuxError};
use tst_core::transport::{BrokenCause, TransportError};
use tst_pipeline::sender::TsFramingError;
use tst_pipeline::{
    DemuxReceiverError, MuxSenderError, RawReceiverError, RawSenderError, ReceiverError,
    SenderError, ShellError, ShellErrorKind,
};

/// Assert a `MuxError` variant produces the expected kind when wrapped
/// in a `MuxSenderError`.
fn assert_mux(e: MuxError, expected: ShellErrorKind) {
    let wrapped: MuxSenderError = e.into();
    assert_eq!(
        wrapped.kind(),
        expected,
        "MuxError variant did not route to expected kind via MuxSenderError: got {:?}",
        wrapped.kind()
    );
}

/// Assert a `TransportError` variant produces the expected kind when
/// wrapped in a sender shell.
fn assert_transport_send(e: TransportError, expected: ShellErrorKind) {
    let mux: MuxSenderError = e.clone().into();
    assert_eq!(mux.kind(), expected, "via MuxSenderError");
    let sender: SenderError = e.clone().into();
    assert_eq!(sender.kind(), expected, "via SenderError");
    let raw: RawSenderError = e.into();
    assert_eq!(raw.kind(), expected, "via RawSenderError");
}

/// Assert a `TransportError` variant produces the expected kind when
/// wrapped in a receiver shell.
fn assert_transport_recv(e: TransportError, expected: ShellErrorKind) {
    let demux: DemuxReceiverError = e.clone().into();
    assert_eq!(demux.kind(), expected, "via DemuxReceiverError");
    let recv: ReceiverError = e.clone().into();
    assert_eq!(recv.kind(), expected, "via ReceiverError");
    let raw: RawReceiverError = e.into();
    assert_eq!(raw.kind(), expected, "via RawReceiverError");
}

fn assert_demux(e: DemuxError, expected: ShellErrorKind) {
    let wrapped: DemuxReceiverError = e.into();
    assert_eq!(wrapped.kind(), expected);
}

fn assert_framing(e: TsFramingError, expected: ShellErrorKind) {
    let wrapped: SenderError = e.into();
    assert_eq!(wrapped.kind(), expected);
}

#[test]
fn mux_error_invalid_config_routes_to_config_invalid() {
    assert_mux(
        MuxError::InvalidConfig("test"),
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_config_invalid_with_reason_routes_to_config_invalid() {
    assert_mux(
        MuxError::ConfigInvalid {
            reason: "test".into(),
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_invalid_nal_routes_to_input_malformed() {
    assert_mux(MuxError::InvalidNal, ShellErrorKind::InputMalformed);
}

#[test]
fn mux_error_buffer_full_routes_to_backpressure() {
    assert_mux(
        MuxError::BufferFull {
            capacity_packets: 100,
        },
        ShellErrorKind::Backpressure,
    );
}

#[test]
fn mux_error_klv_too_large_routes_to_input_malformed() {
    assert_mux(
        MuxError::KlvTooLarge { size: 100, max: 50 },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn mux_error_audio_too_large_routes_to_input_malformed() {
    assert_mux(
        MuxError::AudioTooLarge { size: 100, max: 50 },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn mux_error_subtitle_too_large_routes_to_input_malformed() {
    assert_mux(
        MuxError::SubtitleTooLarge { size: 100, max: 50 },
        ShellErrorKind::InputMalformed,
    );
}

// Config-invalid MuxError variants (24 of 32 — the rest are tested above).
// Listed individually so a missing kind_from_mux arm produces a specific
// failure name rather than a generic "this test failed" message.
#[test]
fn mux_error_invalid_stream_handle_routes_to_config_invalid() {
    use tst_core::mpegts::mux::StreamKind;
    assert_mux(
        MuxError::InvalidStreamHandle {
            kind: StreamKind::Video,
            index: 0,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_ambiguous_target_routes_to_config_invalid() {
    use tst_core::mpegts::mux::StreamKind;
    assert_mux(
        MuxError::AmbiguousTarget {
            kind: StreamKind::Video,
            count: 2,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_no_klv_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::NoKlvStreamsConfigured,
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_no_audio_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::NoAudioStreamsConfigured,
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_no_subtitle_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::NoSubtitleStreamsConfigured,
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_too_many_video_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::TooManyVideoStreams { count: 17, cap: 16 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_too_many_klv_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::TooManyKlvStreams { count: 17, cap: 16 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_too_many_audio_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::TooManyAudioStreams { count: 17, cap: 16 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_too_many_subtitle_streams_routes_to_config_invalid() {
    assert_mux(
        MuxError::TooManySubtitleStreams { count: 17, cap: 16 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_subtitle_pid_as_pcr_routes_to_config_invalid() {
    assert_mux(
        MuxError::SubtitlePidUsedAsPcrPid { pid: 0x100 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_klv_pid_as_pcr_routes_to_config_invalid() {
    assert_mux(
        MuxError::KlvPidUsedAsPcrPid { pid: 0x100 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_invalid_language_routes_to_config_invalid() {
    assert_mux(
        MuxError::InvalidLanguageCode {
            code: [b'X', b'X', b'X'],
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_invalid_teletext_routes_to_config_invalid() {
    use tst_core::mpegts::mux::TeletextField;
    assert_mux(
        MuxError::InvalidTeletextField {
            field: TeletextField::MagazineNumber,
            value: 99,
            max: 7,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_pmt_too_large_routes_to_config_invalid() {
    assert_mux(
        MuxError::PmtTooLarge {
            used_bytes: 200,
            max_bytes: 183,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_malformed_descriptor_routes_to_config_invalid() {
    assert_mux(
        MuxError::MalformedDescriptor {
            stream_index: 0,
            descriptor_index: 0,
            reason: "test",
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_too_many_programs_routes_to_config_invalid() {
    assert_mux(
        MuxError::TooManyPrograms { count: 17, cap: 16 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_empty_program_routes_to_config_invalid() {
    assert_mux(
        MuxError::EmptyProgram { program_number: 1 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_duplicate_program_number_routes_to_config_invalid() {
    assert_mux(
        MuxError::DuplicateProgramNumber { program_number: 1 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_duplicate_pmt_pid_routes_to_config_invalid() {
    assert_mux(
        MuxError::DuplicatePmtPid {
            pid: 0x100,
            programs: [1, 2],
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_duplicate_pid_across_programs_routes_to_config_invalid() {
    assert_mux(
        MuxError::DuplicatePidAcrossPrograms {
            pid: 0x100,
            programs: [1, 2],
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_program_not_found_routes_to_config_invalid() {
    assert_mux(
        MuxError::ProgramNotFound { program_number: 1 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_pmt_pid_conflicts_routes_to_config_invalid() {
    assert_mux(
        MuxError::PmtPidConflictsWithStream {
            pmt_pid: 0x100,
            program_number: 1,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_no_pcr_eligible_stream_routes_to_config_invalid() {
    assert_mux(
        MuxError::NoPcrEligibleStream { program_number: 1 },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_descriptor_index_out_of_range_routes_to_config_invalid() {
    use tst_core::mpegts::mux::StreamKind;
    assert_mux(
        MuxError::DescriptorIndexOutOfRange {
            kind: StreamKind::Video,
            index: 5,
            program_number: 1,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

#[test]
fn mux_error_abs_index_out_of_range_routes_to_config_invalid() {
    assert_mux(
        MuxError::AbsIndexOutOfRange {
            abs_idx: 99,
            len: 3,
            program_number: 1,
        },
        ShellErrorKind::ConfigInvalid,
    );
}

// TransportError × Direction:
// - Backpressure -> Backpressure (both directions)
// - Broken -> TransportBroken (both directions)
// - Closed -> Closed on senders, EndOfStream on receivers
// - TooLarge -> InputMalformed (both directions)
// - ExplicitClose -> Closed (both directions)

#[test]
fn transport_backpressure_routes_to_backpressure_in_senders() {
    assert_transport_send(
        TransportError::Backpressure {
            msg: "test".into(),
            errno_code: None,
        },
        ShellErrorKind::Backpressure,
    );
}

#[test]
fn transport_broken_routes_to_transport_broken_in_senders() {
    assert_transport_send(
        TransportError::Broken {
            msg: "test".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        },
        ShellErrorKind::TransportBroken,
    );
}

#[test]
fn transport_closed_routes_to_closed_in_senders() {
    assert_transport_send(TransportError::Closed, ShellErrorKind::Closed);
}

#[test]
fn transport_too_large_routes_to_input_malformed_in_senders() {
    assert_transport_send(
        TransportError::TooLarge {
            len: 2000,
            max: 1316,
        },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn transport_explicit_close_routes_to_closed_in_senders() {
    assert_transport_send(TransportError::ExplicitClose, ShellErrorKind::Closed);
}

#[test]
fn transport_backpressure_routes_to_backpressure_in_receivers() {
    assert_transport_recv(
        TransportError::Backpressure {
            msg: "test".into(),
            errno_code: None,
        },
        ShellErrorKind::Backpressure,
    );
}

#[test]
fn transport_broken_routes_to_transport_broken_in_receivers() {
    assert_transport_recv(
        TransportError::Broken {
            msg: "test".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        },
        ShellErrorKind::TransportBroken,
    );
}

#[test]
fn transport_closed_routes_to_end_of_stream_in_receivers() {
    assert_transport_recv(TransportError::Closed, ShellErrorKind::EndOfStream);
}

#[test]
fn transport_too_large_routes_to_input_malformed_in_receivers() {
    assert_transport_recv(
        TransportError::TooLarge {
            len: 2000,
            max: 1316,
        },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn transport_explicit_close_routes_to_closed_in_receivers() {
    assert_transport_recv(TransportError::ExplicitClose, ShellErrorKind::Closed);
}

// DemuxError — all 5 variants route to InputMalformed.

#[test]
fn demux_unrecoverable_routes_to_input_malformed() {
    assert_demux(
        DemuxError::Unrecoverable { after_bytes: 100 },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn demux_strict_rejection_routes_to_input_malformed() {
    assert_demux(
        DemuxError::StrictRejection("test".into()),
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn demux_malformed_psi_routes_to_input_malformed() {
    assert_demux(
        DemuxError::MalformedPsi {
            pid: 0,
            reason: "test",
        },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn demux_malformed_pes_routes_to_input_malformed() {
    assert_demux(
        DemuxError::MalformedPes {
            pid: 0,
            reason: "test",
        },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn demux_sync_buf_exhausted_routes_to_input_malformed() {
    assert_demux(
        DemuxError::SyncBufExhausted {
            observed: 4096,
            max: 4096,
        },
        ShellErrorKind::InputMalformed,
    );
}

// TsFramingError — both variants route to InputMalformed.

#[test]
fn framing_sync_lost_routes_to_input_malformed() {
    assert_framing(
        TsFramingError::SyncLost { offset: 0 },
        ShellErrorKind::InputMalformed,
    );
}

#[test]
fn framing_no_sync_after_limit_routes_to_input_malformed() {
    assert_framing(
        TsFramingError::NoSyncAfterLimit { max: 4096 },
        ShellErrorKind::InputMalformed,
    );
}
