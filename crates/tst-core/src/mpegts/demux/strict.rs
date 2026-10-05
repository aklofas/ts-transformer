//! `StrictMode` — turns `NonConformant` events into hard errors per category.

use crate::mpegts::demux::event::NonConformantIssue;

/// Strictness category for the demuxer. Default `Off`.
///
/// Real-world ISR streams routinely omit `metadata_descriptor` (so a
/// "strict means strict-everything" mode would be unusable on most live
/// data). The categories let consumers fail on timing while tolerating
/// descriptor omissions, or vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrictMode {
    /// Lenient — `NonConformant` events surface as data; receive loop
    /// continues. This is the default.
    #[default]
    Off,
    /// Hard-fail on timing anomalies (`PcrAnomaly`, `PtsAnomaly`,
    /// `MissingRequiredPts`, `PcrMalformed`, `PsiChecksumMismatch`).
    /// Tolerate descriptor and stream-type issues.
    ///
    /// Note: `PusiMidPes` appears in this mode's reject set for
    /// non-exhaustive-enum parity but is never emitted by the demuxer —
    /// see that variant's documentation.
    TimingOnly,
    /// Hard-fail on missing `metadata_descriptor` and on stream-type
    /// mismatches. Tolerate timing anomalies.
    DescriptorsOnly,
    /// Hard-fail on every `NonConformantIssue` variant including
    /// future-added ones. Exception: a multi-section PAT
    /// (`PsiMultiSectionUnsupported { table_id: 0x00, .. }`) degrades to a
    /// surface-only event (don't blame the sender); a
    /// multi-section PMT (table_id 0x02) and every other variant are still
    /// rejected.
    Full,
}

impl StrictMode {
    /// Should this issue convert to a fatal `DemuxError::StrictRejection`?
    pub(crate) fn rejects(self, issue: &NonConformantIssue) -> bool {
        match self {
            StrictMode::Off => false,
            StrictMode::TimingOnly => matches!(
                issue,
                NonConformantIssue::PcrAnomaly { .. }
                    | NonConformantIssue::PtsAnomaly { .. }
                    | NonConformantIssue::MissingRequiredPts { .. }
                    | NonConformantIssue::PcrMalformed { .. }
                    | NonConformantIssue::PusiMidPes
                    | NonConformantIssue::PsiChecksumMismatch { .. }
            ),
            StrictMode::DescriptorsOnly => matches!(
                issue,
                NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid
                    | NonConformantIssue::StreamTypeMismatchAsyncOnSyncPid
                    | NonConformantIssue::MissingMetadataDescriptor
                    | NonConformantIssue::SubtitleMissingDescriptor { .. }
                    | NonConformantIssue::SubtitleDescriptorAmbiguous { .. }
            ),
            StrictMode::Full => !matches!(
                issue,
                // A multi-section PAT (table_id 0x00) degrades to a
                // surface-only event — never a hard fail ("don't blame the
                // sender"). PMT multi-section (table_id 0x02) is still rejected.
                NonConformantIssue::PsiMultiSectionUnsupported { table_id: 0x00, .. }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_rejects_nothing() {
        assert!(!StrictMode::Off.rejects(&NonConformantIssue::PusiMidPes));
        assert!(!StrictMode::Off.rejects(&NonConformantIssue::MissingMetadataDescriptor));
    }

    #[test]
    fn timing_only_rejects_timing() {
        let m = StrictMode::TimingOnly;
        assert!(m.rejects(&NonConformantIssue::PcrAnomaly { delta: 100_000 }));
        assert!(m.rejects(&NonConformantIssue::PusiMidPes));
        assert!(!m.rejects(&NonConformantIssue::MissingMetadataDescriptor));
    }

    #[test]
    fn descriptors_only_rejects_descriptors() {
        let m = StrictMode::DescriptorsOnly;
        assert!(m.rejects(&NonConformantIssue::MissingMetadataDescriptor));
        assert!(m.rejects(&NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid));
        assert!(!m.rejects(&NonConformantIssue::PcrAnomaly { delta: 100_000 }));
    }

    #[test]
    fn descriptors_only_rejects_subtitle_missing_descriptor() {
        // SubtitleMissingDescriptor is the subtitle-side parallel to
        // MissingMetadataDescriptor for KLV — both are descriptor-shape
        // concerns and DescriptorsOnly should escalate both.
        let m = StrictMode::DescriptorsOnly;
        assert!(m.rejects(&NonConformantIssue::SubtitleMissingDescriptor { pid: 0x100 }));
    }

    #[test]
    fn descriptors_only_rejects_subtitle_descriptor_ambiguous() {
        // Multi-marker ambiguity on a 0x06 PID is also a descriptor-shape
        // concern — DescriptorsOnly should escalate.
        let m = StrictMode::DescriptorsOnly;
        assert!(m.rejects(&NonConformantIssue::SubtitleDescriptorAmbiguous {
            pid: 0x100,
            tags: vec![0x59, 0xF0],
        }));
    }

    #[test]
    fn full_rejects_all() {
        let m = StrictMode::Full;
        assert!(m.rejects(&NonConformantIssue::Other("anything".into())));
        assert!(m.rejects(&NonConformantIssue::MissingMetadataDescriptor));
        assert!(m.rejects(&NonConformantIssue::PcrAnomaly { delta: 0 }));
    }

    #[test]
    fn timing_only_rejects_pts_anomaly_and_missing_required_pts() {
        // PTS timing concerns join the timing-only cascade.
        let m = StrictMode::TimingOnly;
        assert!(m.rejects(&NonConformantIssue::PtsAnomaly { delta: -100_000 }));
        assert!(m.rejects(&NonConformantIssue::MissingRequiredPts { pid: 0x100 }));
    }

    #[test]
    fn ac3_sync_missing_only_rejected_by_full() {
        // AC-3 syncframe alignment is a content-layer
        // concern, neither timing nor descriptor; only StrictMode::Full
        // escalates it.
        let issue = NonConformantIssue::Ac3SyncMissing { pid: 0x300 };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    #[test]
    fn adaptation_field_malformed_rejected_only_by_full() {
        use crate::mpegts::demux::ts::AdaptationFieldKind;
        let issue = NonConformantIssue::AdaptationFieldMalformed {
            pid: 0x100,
            kind: AdaptationFieldKind::ReservedControl,
        };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    #[test]
    fn unsupported_scrambling_rejected_only_by_full() {
        let issue = NonConformantIssue::UnsupportedScrambling {
            pid: 0x100,
            control: 1,
        };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    #[test]
    fn zero_length_pes_non_video_rejected_only_by_full() {
        let issue = NonConformantIssue::ZeroLengthPesNonVideo {
            pid: 0x101,
            stream_id: 0xC0,
        };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    #[test]
    fn psi_syntax_rejected_only_by_full() {
        use crate::mpegts::demux::event::PsiSyntaxKind;
        let issue = NonConformantIssue::PsiSyntax {
            pid: 0x0000,
            table_id: 0x00,
            kind: PsiSyntaxKind::SectionSyntaxIndicatorUnset,
        };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    /// LATM framing is data-conformance (the bitstream itself, not
    /// timing or descriptors). Only `Full` strict mode rejects it; the
    /// narrower `TimingOnly` / `DescriptorsOnly` modes leave it as a
    /// surface-only NonConformant event.
    #[test]
    fn latm_framing_rejected_only_by_full() {
        use crate::codec::aac::latm::LatmFramingKind;
        let issue = NonConformantIssue::LatmFraming {
            pid: 0x101,
            kind: LatmFramingKind::MissingSyncword,
        };
        assert!(!StrictMode::Off.rejects(&issue));
        assert!(!StrictMode::TimingOnly.rejects(&issue));
        assert!(!StrictMode::DescriptorsOnly.rejects(&issue));
        assert!(StrictMode::Full.rejects(&issue));
    }

    #[test]
    fn full_does_not_reject_multi_section_pat() {
        // A PAT multi-section diagnostic (table_id 0x00) is a
        // "don't blame the sender" degradation, never a hard fail — even in Full.
        let issue = NonConformantIssue::PsiMultiSectionUnsupported {
            pid: 0x0000,
            table_id: 0x00,
            last_section_number: 3,
        };
        assert!(!StrictMode::Full.rejects(&issue));
        assert!(!StrictMode::Off.rejects(&issue));
    }

    #[test]
    fn full_still_rejects_multi_section_pmt() {
        // PMT multi-section stays unsupported AND a hard fail under Full.
        let issue = NonConformantIssue::PsiMultiSectionUnsupported {
            pid: 0x0100,
            table_id: 0x02,
            last_section_number: 1,
        };
        assert!(StrictMode::Full.rejects(&issue));
    }
}
