//! One value of every variant of the errors the binding kind table
//! classifies, in declaration order — for the tests that must cover every
//! variant on more than one projection path (tst-pipeline's coarsening
//! test, the C binding's offline-vs-live code test).
//!
//! The lengths are asserted by the consumers. When a variant is added to
//! one of these errors, tst-core's exhaustive `kind()` stops compiling
//! first; add the variant here in the same change.

use tst_core::codec::misp_time::MispTimeError;
use tst_core::error::{DemuxError, MuxError};
use tst_core::mpegts::mux::{MuxStreamKind, TeletextField};
use tst_core::transport::{BrokenCause, TransportError};

/// All 38 `MuxError` variants, in `crates/tst-core/src/error.rs` order.
pub fn every_mux_error() -> Vec<MuxError> {
    vec![
        MuxError::InvalidConfig("bad"),
        MuxError::ConfigInvalid {
            reason: "bad".into(),
        },
        MuxError::InvalidNal,
        MuxError::InvalidAv1Obu,
        MuxError::MispTime(MispTimeError::NoVclNal),
        MuxError::BufferFull {
            capacity_packets: 8,
        },
        MuxError::KlvTooLarge { size: 2, max: 1 },
        MuxError::AudioTooLarge { size: 2, max: 1 },
        MuxError::InvalidStreamHandle {
            kind: MuxStreamKind::Video,
            index: 3,
        },
        MuxError::AmbiguousTarget {
            kind: MuxStreamKind::Klv,
            count: 2,
        },
        MuxError::NoKlvStreamsConfigured,
        MuxError::NoAudioStreamsConfigured,
        MuxError::NoSubtitleStreamsConfigured,
        MuxError::TooManyVideoStreams { count: 2, cap: 1 },
        MuxError::TooManyKlvStreams { count: 2, cap: 1 },
        MuxError::TooManyAudioStreams { count: 2, cap: 1 },
        MuxError::TooManySubtitleStreams { count: 2, cap: 1 },
        MuxError::SubtitleTooLarge { size: 2, max: 1 },
        MuxError::SubtitlePidUsedAsPcrPid { pid: 0x100 },
        MuxError::KlvPidUsedAsPcrPid { pid: 0x101 },
        MuxError::InvalidLanguageCode { code: *b"x1y" },
        MuxError::InvalidTeletextField {
            field: TeletextField::MagazineNumber,
            value: 9,
            max: 8,
        },
        MuxError::PmtTooLarge {
            used_bytes: 2,
            max_bytes: 1,
        },
        MuxError::MalformedDescriptor {
            stream_index: 0,
            descriptor_index: 1,
            reason: "bad",
        },
        MuxError::TooManyPrograms { count: 2, cap: 1 },
        MuxError::EmptyProgram { program_number: 1 },
        MuxError::DuplicateProgramNumber { program_number: 1 },
        MuxError::DuplicatePmtPid {
            pid: 0x100,
            programs: [1, 2],
        },
        MuxError::DuplicatePidAcrossPrograms {
            pid: 0x100,
            programs: [1, 2],
        },
        MuxError::ProgramNotFound { program_number: 9 },
        MuxError::PmtPidConflictsWithStream {
            pmt_pid: 0x100,
            program_number: 1,
        },
        MuxError::NoPcrEligibleStream { program_number: 1 },
        MuxError::NoDataStreamsConfigured,
        MuxError::TooManyDataStreams { count: 2, cap: 1 },
        MuxError::DataTooLarge { size: 2, max: 1 },
        MuxError::DataPidUsedAsPcrPid { pid: 0x102 },
        MuxError::DescriptorIndexOutOfRange {
            kind: MuxStreamKind::Audio,
            index: 5,
            program_number: 1,
        },
        MuxError::AbsIndexOutOfRange {
            abs_idx: 9,
            len: 3,
            program_number: 1,
        },
    ]
}

/// All 5 `TransportError` variants.
pub fn every_transport_error() -> Vec<TransportError> {
    vec![
        TransportError::Backpressure {
            msg: "full".into(),
            errno_code: Some(6),
        },
        TransportError::Broken {
            msg: "reset".into(),
            errno_code: Some(104),
            cause: BrokenCause::Unspecified,
        },
        TransportError::Closed,
        TransportError::TooLarge {
            len: 2000,
            max: 1316,
        },
        TransportError::ExplicitClose,
    ]
}

/// All 4 `DemuxError` variants.
pub fn every_demux_error() -> Vec<DemuxError> {
    vec![
        DemuxError::Unrecoverable { after_bytes: 6016 },
        DemuxError::StrictRejection("strict".into()),
        DemuxError::MalformedPes {
            pid: 0x100,
            reason: "short",
        },
        DemuxError::SyncBufExhausted {
            observed: 2,
            max: 1,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::error::{DemuxErrorKind, MuxErrorKind};
    use tst_core::transport::TransportErrorKind;

    // The kind enums are exhaustive and these matches have no wildcard, so a
    // new kind fails to compile here until it is given a covering sample. The
    // tests check each listed kind's sample against `every_*_error()`, and
    // that every kind those samples reach is listed.

    const MUX_KINDS: [MuxErrorKind; 5] = [
        MuxErrorKind::InputMalformed,
        MuxErrorKind::ConfigInvalid,
        MuxErrorKind::InvalidUsage,
        MuxErrorKind::Backpressure,
        MuxErrorKind::Internal,
    ];
    const TRANSPORT_KINDS: [TransportErrorKind; 5] = [
        TransportErrorKind::Backpressure,
        TransportErrorKind::Broken,
        TransportErrorKind::Closed,
        TransportErrorKind::TooLarge,
        TransportErrorKind::ExplicitClose,
    ];
    const DEMUX_KINDS: [DemuxErrorKind; 4] = [
        DemuxErrorKind::Unrecoverable,
        DemuxErrorKind::StrictRejection,
        DemuxErrorKind::MalformedPes,
        DemuxErrorKind::SyncBufExhausted,
    ];

    fn mux_kind_sample(kind: MuxErrorKind) -> Option<MuxError> {
        match kind {
            MuxErrorKind::InputMalformed => Some(MuxError::InvalidNal),
            MuxErrorKind::ConfigInvalid => Some(MuxError::InvalidConfig("bad")),
            MuxErrorKind::InvalidUsage => Some(MuxError::NoKlvStreamsConfigured),
            MuxErrorKind::Backpressure => Some(MuxError::BufferFull {
                capacity_packets: 8,
            }),
            // Reserved: no `MuxError` variant maps here.
            MuxErrorKind::Internal => None,
        }
    }

    fn transport_kind_sample(kind: TransportErrorKind) -> TransportError {
        match kind {
            TransportErrorKind::Backpressure => TransportError::Backpressure {
                msg: "full".into(),
                errno_code: Some(6),
            },
            TransportErrorKind::Broken => TransportError::Broken {
                msg: "reset".into(),
                errno_code: Some(104),
                cause: BrokenCause::Unspecified,
            },
            TransportErrorKind::Closed => TransportError::Closed,
            TransportErrorKind::TooLarge => TransportError::TooLarge {
                len: 2000,
                max: 1316,
            },
            TransportErrorKind::ExplicitClose => TransportError::ExplicitClose,
        }
    }

    fn demux_kind_sample(kind: DemuxErrorKind) -> DemuxError {
        match kind {
            DemuxErrorKind::Unrecoverable => DemuxError::Unrecoverable { after_bytes: 6016 },
            DemuxErrorKind::StrictRejection => DemuxError::StrictRejection("strict".into()),
            DemuxErrorKind::MalformedPes => DemuxError::MalformedPes {
                pid: 0x100,
                reason: "short",
            },
            DemuxErrorKind::SyncBufExhausted => DemuxError::SyncBufExhausted {
                observed: 2,
                max: 1,
            },
        }
    }

    #[test]
    fn every_mux_kind_is_covered_by_every_mux_error() {
        let all = every_mux_error();
        for kind in MUX_KINDS {
            match mux_kind_sample(kind) {
                Some(sample) => {
                    assert_eq!(sample.kind(), kind);
                    assert!(all.contains(&sample), "{sample:?}");
                }
                None => assert!(all.iter().all(|e| e.kind() != kind), "{kind:?}"),
            }
        }
        assert!(all.iter().all(|e| MUX_KINDS.contains(&e.kind())));
    }

    #[test]
    fn every_transport_kind_is_covered_by_every_transport_error() {
        let all = every_transport_error();
        for kind in TRANSPORT_KINDS {
            let sample = transport_kind_sample(kind);
            assert_eq!(sample.kind(), kind);
            assert!(all.contains(&sample), "{sample:?}");
        }
        assert!(all.iter().all(|e| TRANSPORT_KINDS.contains(&e.kind())));
    }

    #[test]
    fn every_demux_kind_is_covered_by_every_demux_error() {
        let all = every_demux_error();
        for kind in DEMUX_KINDS {
            let sample = demux_kind_sample(kind);
            assert_eq!(sample.kind(), kind);
            assert!(all.contains(&sample), "{sample:?}");
        }
        assert!(all.iter().all(|e| DEMUX_KINDS.contains(&e.kind())));
    }

    #[test]
    fn every_mux_error_sample_count_is_pinned() {
        assert_eq!(every_mux_error().len(), 38);
    }

    #[test]
    fn every_transport_error_sample_count_is_pinned() {
        assert_eq!(every_transport_error().len(), 5);
    }

    #[test]
    fn every_demux_error_sample_count_is_pinned() {
        assert_eq!(every_demux_error().len(), 4);
    }
}
