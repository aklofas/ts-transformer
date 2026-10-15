//! Every inner-error variant routes through the shell's `From<inner>` impl
//! (and so through the matching `kind_from_*` helper) to the expected
//! `ShellErrorKind`. The CI ratchet
//! `scripts/check/rust/pipeline-kind-classification.sh` enforces that every
//! upstream inner variant has a match arm before the wildcard; the tables
//! below pin the value of each arm. A failure names the variant (`{e:?}`)
//! and, for transport rows, the shell it went through.
//!
//! 49 rows: 32 `MuxError`; 5 `TransportError` through each of the three
//! sender shells and the same 5 through each of the three receiver shells;
//! 5 `DemuxError`; 2 `TsFramingError`.

use tst_core::error::{DemuxError, MuxError};
use tst_core::mpegts::mux::{StreamKind, TeletextField};
use tst_core::transport::{BrokenCause, TransportError};
use tst_pipeline::ShellErrorKind::{
    Backpressure, Closed, ConfigInvalid, EndOfStream, InputMalformed, TransportBroken,
};
use tst_pipeline::sender::TsFramingError;
use tst_pipeline::{
    DemuxReceiverError, MuxSenderError, RawReceiverError, RawSenderError, ReceiverError,
    SenderError, ShellError, ShellErrorKind,
};

/// `(variant, kind through MuxSenderError)`.
fn mux_rows() -> Vec<(MuxError, ShellErrorKind)> {
    vec![
        (MuxError::InvalidConfig("test"), ConfigInvalid),
        (
            MuxError::ConfigInvalid {
                reason: "test".into(),
            },
            ConfigInvalid,
        ),
        (MuxError::InvalidNal, InputMalformed),
        (
            MuxError::BufferFull {
                capacity_packets: 100,
            },
            Backpressure,
        ),
        (MuxError::KlvTooLarge { size: 100, max: 50 }, InputMalformed),
        (
            MuxError::AudioTooLarge { size: 100, max: 50 },
            InputMalformed,
        ),
        (
            MuxError::SubtitleTooLarge { size: 100, max: 50 },
            InputMalformed,
        ),
        (
            MuxError::InvalidStreamHandle {
                kind: StreamKind::Video,
                index: 0,
            },
            ConfigInvalid,
        ),
        (
            MuxError::AmbiguousTarget {
                kind: StreamKind::Video,
                count: 2,
            },
            ConfigInvalid,
        ),
        (MuxError::NoKlvStreamsConfigured, ConfigInvalid),
        (MuxError::NoAudioStreamsConfigured, ConfigInvalid),
        (MuxError::NoSubtitleStreamsConfigured, ConfigInvalid),
        (
            MuxError::TooManyVideoStreams { count: 17, cap: 16 },
            ConfigInvalid,
        ),
        (
            MuxError::TooManyKlvStreams { count: 17, cap: 16 },
            ConfigInvalid,
        ),
        (
            MuxError::TooManyAudioStreams { count: 17, cap: 16 },
            ConfigInvalid,
        ),
        (
            MuxError::TooManySubtitleStreams { count: 17, cap: 16 },
            ConfigInvalid,
        ),
        (
            MuxError::SubtitlePidUsedAsPcrPid { pid: 0x100 },
            ConfigInvalid,
        ),
        (MuxError::KlvPidUsedAsPcrPid { pid: 0x100 }, ConfigInvalid),
        (
            MuxError::InvalidLanguageCode {
                code: [b'X', b'X', b'X'],
            },
            ConfigInvalid,
        ),
        (
            MuxError::InvalidTeletextField {
                field: TeletextField::MagazineNumber,
                value: 99,
                max: 7,
            },
            ConfigInvalid,
        ),
        (
            MuxError::PmtTooLarge {
                used_bytes: 200,
                max_bytes: 183,
            },
            ConfigInvalid,
        ),
        (
            MuxError::MalformedDescriptor {
                stream_index: 0,
                descriptor_index: 0,
                reason: "test",
            },
            ConfigInvalid,
        ),
        (
            MuxError::TooManyPrograms { count: 17, cap: 16 },
            ConfigInvalid,
        ),
        (MuxError::EmptyProgram { program_number: 1 }, ConfigInvalid),
        (
            MuxError::DuplicateProgramNumber { program_number: 1 },
            ConfigInvalid,
        ),
        (
            MuxError::DuplicatePmtPid {
                pid: 0x100,
                programs: [1, 2],
            },
            ConfigInvalid,
        ),
        (
            MuxError::DuplicatePidAcrossPrograms {
                pid: 0x100,
                programs: [1, 2],
            },
            ConfigInvalid,
        ),
        (
            MuxError::ProgramNotFound { program_number: 1 },
            ConfigInvalid,
        ),
        (
            MuxError::PmtPidConflictsWithStream {
                pmt_pid: 0x100,
                program_number: 1,
            },
            ConfigInvalid,
        ),
        (
            MuxError::NoPcrEligibleStream { program_number: 1 },
            ConfigInvalid,
        ),
        (
            MuxError::DescriptorIndexOutOfRange {
                kind: StreamKind::Video,
                index: 5,
                program_number: 1,
            },
            ConfigInvalid,
        ),
        (
            MuxError::AbsIndexOutOfRange {
                abs_idx: 99,
                len: 3,
                program_number: 1,
            },
            ConfigInvalid,
        ),
    ]
}

/// `(variant, kind through every sender shell, kind through every receiver shell)`.
fn transport_rows() -> Vec<(TransportError, ShellErrorKind, ShellErrorKind)> {
    vec![
        (
            TransportError::Backpressure {
                msg: "test".into(),
                errno_code: None,
            },
            Backpressure,
            Backpressure,
        ),
        (
            TransportError::Broken {
                msg: "test".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            },
            TransportBroken,
            TransportBroken,
        ),
        (TransportError::Closed, Closed, EndOfStream),
        (
            TransportError::TooLarge {
                len: 2000,
                max: 1316,
            },
            InputMalformed,
            InputMalformed,
        ),
        (TransportError::ExplicitClose, Closed, Closed),
    ]
}

/// `(variant, kind through DemuxReceiverError)`.
fn demux_rows() -> Vec<(DemuxError, ShellErrorKind)> {
    vec![
        (
            DemuxError::Unrecoverable { after_bytes: 100 },
            InputMalformed,
        ),
        (DemuxError::StrictRejection("test".into()), InputMalformed),
        (
            DemuxError::MalformedPes {
                pid: 0,
                reason: "test",
            },
            InputMalformed,
        ),
        (
            DemuxError::SyncBufExhausted {
                observed: 4096,
                max: 4096,
            },
            InputMalformed,
        ),
    ]
}

/// `(variant, kind through SenderError)`.
fn framing_rows() -> Vec<(TsFramingError, ShellErrorKind)> {
    vec![
        (TsFramingError::SyncLost { offset: 0 }, InputMalformed),
        (
            TsFramingError::NoSyncAfterLimit { max: 4096 },
            InputMalformed,
        ),
    ]
}

#[test]
fn every_inner_variant_routes_to_its_kind() {
    for (e, expected) in mux_rows() {
        let row = format!("{e:?}");
        let got = MuxSenderError::from(e).kind();
        assert_eq!(got, expected, "MuxError::{row} via MuxSenderError");
    }
    for (e, via_sender, via_receiver) in transport_rows() {
        let row = format!("{e:?}");
        let senders = [
            ("MuxSenderError", MuxSenderError::from(e.clone()).kind()),
            ("SenderError", SenderError::from(e.clone()).kind()),
            ("RawSenderError", RawSenderError::from(e.clone()).kind()),
        ];
        for (via, got) in senders {
            assert_eq!(got, via_sender, "TransportError::{row} via {via}");
        }
        let receivers = [
            (
                "DemuxReceiverError",
                DemuxReceiverError::from(e.clone()).kind(),
            ),
            ("ReceiverError", ReceiverError::from(e.clone()).kind()),
            ("RawReceiverError", RawReceiverError::from(e).kind()),
        ];
        for (via, got) in receivers {
            assert_eq!(got, via_receiver, "TransportError::{row} via {via}");
        }
    }
    for (e, expected) in demux_rows() {
        let row = format!("{e:?}");
        let got = DemuxReceiverError::from(e).kind();
        assert_eq!(got, expected, "DemuxError::{row} via DemuxReceiverError");
    }
    for (e, expected) in framing_rows() {
        let row = format!("{e:?}");
        let got = SenderError::from(e).kind();
        assert_eq!(got, expected, "TsFramingError::{row} via SenderError");
    }
}
