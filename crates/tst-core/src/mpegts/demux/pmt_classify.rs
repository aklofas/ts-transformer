//! PMT stream classification: descriptor-driven derivation of `StreamKind`
//! from PMT entries, plus utility functions for stream-type-byte mapping
//! and descriptor recognition.
//!
//! Hosts one helper method on `Demuxer` (`get_stream_kind`) and
//! module-level free functions consumed across the demux module tree.
//! All items are `pub(super)` — invisible outside `mpegts::demux` —
//! except `classify_pmt_stream`, which is `pub(crate)` so the mux-side
//! `StreamSpec::Data` acceptance rule can call it.
//!
//! These stay free functions (not wrapped in a struct): they hold no state
//! of their own, and the classifier's scope is PMT-specific.

use crate::mpegts::demux::event::{AudioCodec, StreamKind, SubtitleCodec, VideoCodec};
use crate::mpegts::demux::psi::{
    classify_audio_stream_type, extract_metadata_link, has_klva_registration,
};
use crate::mpegts::descriptors::RawDescriptor;
use alloc::vec::Vec;

impl super::demuxer::Demuxer {
    pub(super) fn get_stream_kind(
        &self,
        pid: u16,
        s: &crate::mpegts::demux::psi::PmtStream,
    ) -> StreamKind {
        // Caller override wins over PMT classification.
        if let Some(&kind) = self.options.stream_kind_overrides.get(&pid) {
            kind
        } else {
            classify_pmt_stream(s.stream_type, &s.descriptors)
        }
    }
}

// ─── Free functions ────────────────────────────────────────────────────────

/// Classify a PMT entry's `(stream_type, descriptors)` pair under the
/// standard cascade — the shared core of `get_stream_kind` (demux) and
/// the mux-side `StreamSpec::Data` acceptance rule, which requires the
/// result to be `StreamKind::Unknown` so a Data stream re-demuxes as
/// Unknown (rejects typed stream_type bytes and 0x06 descriptor
/// masquerades). `treat_as` overrides are a demux-session concern and do
/// not participate.
pub(crate) fn classify_pmt_stream(stream_type: u8, descriptors: &[RawDescriptor]) -> StreamKind {
    match stream_type {
        0x1B => StreamKind::Video(VideoCodec::H264),
        0x24 => StreamKind::Video(VideoCodec::H265),
        0x33 => StreamKind::Video(VideoCodec::H266),
        0x06 => classify_0x06(descriptors),
        0x15 => StreamKind::KlvSync {
            declared_link: extract_metadata_link(descriptors),
        },
        other => {
            if let Some(codec) = classify_audio_stream_type(other) {
                StreamKind::Audio(codec)
            } else {
                StreamKind::Unknown(other)
            }
        }
    }
}

/// True if `stream_type` denotes a video elementary stream per ISO/IEC
/// 13818-1 Table 2-34 — including video codecs `tst-core` does not parse
/// (MPEG-1/MPEG-2/MPEG-4 video), which [`classify_pmt_stream`] therefore
/// returns as [`StreamKind::Unknown`].
///
/// Used by the zero-`PES_packet_length` rule (H.222.0 §2.4.3.7):
/// an unbounded PES is legal whenever the payload is a *video* elementary
/// stream — not only the codecs `tst-core` recognizes as
/// [`StreamKind::Video`]. Keying the zero-length rule on
/// [`StreamKind::Video`] alone would mis-flag a conformant MPEG-2 video
/// stream (classified `Unknown(0x02)`) as a non-video violation.
pub(crate) fn is_video_stream_type(stream_type: u8) -> bool {
    matches!(
        stream_type,
        0x01      // ISO/IEC 11172-2 Video (MPEG-1)
            | 0x02 // ITU-T H.262 | ISO/IEC 13818-2 Video (MPEG-2)
            | 0x10 // ISO/IEC 14496-2 Visual (MPEG-4 Part 2)
            | 0x1B // ITU-T H.264 | ISO/IEC 14496-10
            | 0x24 // ITU-T H.265 | ISO/IEC 23008-2
            | 0x33 // ITU-T H.266 | ISO/IEC 23090-3
    )
}

/// Extract the `metadata_descriptor` declared link for a specific PID from
/// a parsed PMT. Used by `build_program_map` to rebuild klv_links after
/// collision filtering has already reduced the stream list.
pub(super) fn extract_metadata_link_for_pid(
    pmt: &crate::mpegts::demux::psi::Pmt,
    pid: u16,
) -> Option<u16> {
    pmt.streams
        .iter()
        .find(|s| s.elementary_pid == pid)
        .and_then(|s| extract_metadata_link(&s.descriptors))
}

/// Map a `StreamKind` to its MPEG-TS `stream_type` byte (PMT value).
///
/// Used for `StreamStats.stream_type` labelling on the receiver side; not
/// emitted on the wire (the demuxer reads stream_type from the PMT). See
/// `mpegts::common::StreamType` for the canonical mux-side encoding.
pub(super) fn stream_type_from_kind(k: &StreamKind) -> u8 {
    match k {
        StreamKind::Video(VideoCodec::H264) => 0x1B,
        StreamKind::Video(VideoCodec::H265) => 0x24,
        StreamKind::Video(VideoCodec::H266) => 0x33,
        // AV1 rides stream_type 0x06 (PES private data) plus an AV01
        // registration_descriptor in the PMT.
        StreamKind::Video(VideoCodec::Av1) => 0x06,
        StreamKind::Audio(AudioCodec::Mp2) => 0x03,
        StreamKind::Audio(AudioCodec::Aac) => 0x0F,
        StreamKind::Audio(AudioCodec::AacLatm) => 0x11,
        StreamKind::Audio(AudioCodec::Ac3) => 0x81,
        StreamKind::Subtitle(_) => 0x06,
        StreamKind::KlvSync { .. } => 0x15,
        StreamKind::KlvAsync => 0x06,
        StreamKind::Unknown(t) => *t,
    }
}

/// Classify a stream_type 0x06 ("PES private data") by inspecting its
/// PMT-stream descriptors. Subtitle-disambiguating tags take priority
/// over the existing KLV registration check; if no subtitle descriptor
/// is present the result is identical to the prior behavior.
///
/// Priority (most-specific first):
///   1. `subtitling_descriptor` (tag 0x59, ETSI EN 300 468) → DVB subtitling.
///   2. `teletext_descriptor` (tag 0x56) or `VBI_teletext_descriptor`
///      (tag 0x46) → DVB teletext.
///   3. `registration_descriptor` (tag 0x05) format_identifier `"VTTC"` →
///      WebVTT-in-MPEG-TS.
///   4. `registration_descriptor` format_identifier `"GA94"` → CEA-708
///      standalone.
///   5. `registration_descriptor` format_identifier `"KLVA"` → asynchronous
///      MISB KLV (existing behavior).
///   6. Otherwise → `StreamKind::Unknown(0x06)`.
pub(super) fn classify_0x06(descriptors: &[RawDescriptor]) -> StreamKind {
    use crate::mpegts::descriptors::{find_descriptor_tag, find_format_identifier};
    // AV1 in MPEG-2 TS binding §2.1: format_identifier = "AV01".
    // AV01 registration is exclusive — wins over any other descriptor.
    if find_format_identifier(descriptors, b"AV01") {
        return StreamKind::Video(VideoCodec::Av1);
    }
    if find_descriptor_tag(descriptors, 0x59) {
        StreamKind::Subtitle(SubtitleCodec::DvbSubtitling)
    } else if find_descriptor_tag(descriptors, 0x56) || find_descriptor_tag(descriptors, 0x46) {
        StreamKind::Subtitle(SubtitleCodec::DvbTeletext)
    } else if find_format_identifier(descriptors, b"VTTC") {
        StreamKind::Subtitle(SubtitleCodec::WebVttInTs)
    } else if find_format_identifier(descriptors, b"GA94") {
        StreamKind::Subtitle(SubtitleCodec::Cea708Standalone)
    } else if has_klva_registration(descriptors) {
        StreamKind::KlvAsync
    } else {
        StreamKind::Unknown(0x06)
    }
}

/// Same as [`classify_0x06`] but also returns the list of recognized
/// subtitle/KLV codec markers found on the PID — empty if there's no
/// ambiguity (zero or one marker), populated if more than one was found.
///
/// Tag list encoding mirrors [`crate::mpegts::demux::event::NonConformantIssue::SubtitleDescriptorAmbiguous`]:
/// descriptor tag bytes for tag-presence matches (0x59 / 0x56 / 0x46),
/// synthetic codepoints for `format_identifier` matches (0xF0=VTTC,
/// 0xF1=GA94, 0xF2=KLVA). The classification result follows the existing
/// first-match priority — only the diagnostic tag list changes.
pub(super) fn classify_0x06_with_ambiguity(descriptors: &[RawDescriptor]) -> (StreamKind, Vec<u8>) {
    use crate::mpegts::descriptors::{find_descriptor_tag, find_format_identifier};
    let mut markers: Vec<u8> = Vec::new();
    if find_descriptor_tag(descriptors, 0x59) {
        markers.push(0x59);
    }
    // 0x56 and 0x46 are sibling teletext tags — count as one marker so
    // a stream carrying both doesn't trip ambiguity on the teletext side.
    if find_descriptor_tag(descriptors, 0x56) {
        markers.push(0x56);
    } else if find_descriptor_tag(descriptors, 0x46) {
        markers.push(0x46);
    }
    if find_format_identifier(descriptors, b"VTTC") {
        markers.push(0xF0);
    }
    if find_format_identifier(descriptors, b"GA94") {
        markers.push(0xF1);
    }
    if find_format_identifier(descriptors, b"KLVA") {
        markers.push(0xF2);
    }
    let kind = classify_0x06(descriptors);
    let ambiguous = if markers.len() <= 1 {
        Vec::new()
    } else {
        markers
    };
    (kind, ambiguous)
}

/// True iff `descriptors` contains any descriptor that lets the demuxer
/// recognize this stream as a subtitle/caption track:
///   * `subtitling_descriptor`  (tag 0x59)
///   * `teletext_descriptor`    (tag 0x56)
///   * `VBI_teletext_descriptor`(tag 0x46)
///   * `registration_descriptor` with format_identifier `"VTTC"` or `"GA94"`.
///
/// Used by the PMT classifier to surface `SubtitleMissingDescriptor`
/// when a `treat_as` override (or any other path) routes a PID to
/// `StreamKind::Subtitle(_)` but the PMT entry has none of the above.
pub(super) fn has_recognized_subtitle_descriptor(descriptors: &[RawDescriptor]) -> bool {
    use crate::mpegts::descriptors::{find_descriptor_tag, find_format_identifier};
    find_descriptor_tag(descriptors, 0x59)
        || find_descriptor_tag(descriptors, 0x56)
        || find_descriptor_tag(descriptors, 0x46)
        || find_format_identifier(descriptors, b"VTTC")
        || find_format_identifier(descriptors, b"GA94")
}

/// True iff `descriptors` contains a Registration descriptor that
/// LOOKS like an attempted AV1 (`AV01`) registration but is truncated.
/// Specifically: a descriptor with `tag == 0x05`, body length < 4 bytes,
/// and body starts with `b"AV"`. Outer length-vs-buffer overflow would
/// already error via `PsiParseError::DescriptorLoopOverflow` at walk
/// time; this catches the subtler case where the descriptor is
/// well-formed but its body can't be a valid 4-byte format_identifier.
///
/// Used by the demuxer to surface `NonConformantIssue::Av1RegistrationMalformed`
/// from the PMT processing path. Lenient mode silently still falls
/// through to `StreamKind::Unknown(0x06)` from the standard cascade;
/// strict mode (`StrictMode::Full`) converts the issue to a fatal
/// `DemuxError::StrictRejection`.
pub(super) fn is_malformed_av1_registration(descriptors: &[RawDescriptor]) -> bool {
    descriptors
        .iter()
        .any(|d| d.tag == 0x05 && d.data.len() < 4 && d.data.starts_with(b"AV"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn is_video_stream_type_covers_iso_video_codes() {
        // Recognized-by-tst-core video.
        assert!(is_video_stream_type(0x1B)); // H.264
        assert!(is_video_stream_type(0x24)); // H.265
        assert!(is_video_stream_type(0x33)); // H.266
        // Video codecs tst-core does NOT parse (classified Unknown) but which
        // are still video elementary streams (zero-length permission).
        assert!(is_video_stream_type(0x01)); // MPEG-1 video
        assert!(is_video_stream_type(0x02)); // MPEG-2 video (H.262)
        assert!(is_video_stream_type(0x10)); // MPEG-4 Part 2 visual
        // Non-video stream types.
        assert!(!is_video_stream_type(0x03)); // MPEG-1 audio
        assert!(!is_video_stream_type(0x04)); // MPEG-2 audio
        assert!(!is_video_stream_type(0x0F)); // AAC ADTS
        assert!(!is_video_stream_type(0x15)); // metadata in PES (KLV)
        assert!(!is_video_stream_type(0x06)); // PES private data
        assert!(!is_video_stream_type(0x00));
    }

    #[test]
    fn classify_pmt_stream_matches_derive_cascade() {
        use crate::mpegts::descriptors::RawDescriptor;
        // Typed bytes
        assert!(matches!(
            classify_pmt_stream(0x1B, &[]),
            StreamKind::Video(VideoCodec::H264)
        ));
        assert!(matches!(
            classify_pmt_stream(0x24, &[]),
            StreamKind::Video(VideoCodec::H265)
        ));
        assert!(matches!(
            classify_pmt_stream(0x33, &[]),
            StreamKind::Video(VideoCodec::H266)
        ));
        assert!(matches!(
            classify_pmt_stream(0x15, &[]),
            StreamKind::KlvSync { .. }
        ));
        assert!(matches!(
            classify_pmt_stream(0x03, &[]),
            StreamKind::Audio(AudioCodec::Mp2)
        ));
        assert!(matches!(
            classify_pmt_stream(0x04, &[]),
            StreamKind::Audio(AudioCodec::Mp2)
        ));
        assert!(matches!(
            classify_pmt_stream(0x0F, &[]),
            StreamKind::Audio(AudioCodec::Aac)
        ));
        assert!(matches!(
            classify_pmt_stream(0x11, &[]),
            StreamKind::Audio(AudioCodec::AacLatm)
        ));
        assert!(matches!(
            classify_pmt_stream(0x81, &[]),
            StreamKind::Audio(AudioCodec::Ac3)
        ));
        // 0x06 cascade
        let klva = RawDescriptor {
            tag: 0x05,
            data: b"KLVA".to_vec(),
        };
        assert!(matches!(
            classify_pmt_stream(0x06, &[klva]),
            StreamKind::KlvAsync
        ));
        assert!(matches!(
            classify_pmt_stream(0x06, &[]),
            StreamKind::Unknown(0x06)
        ));
        // User-private + unrecognized
        assert!(matches!(
            classify_pmt_stream(0xF0, &[]),
            StreamKind::Unknown(0xF0)
        ));
        assert!(matches!(
            classify_pmt_stream(0x87, &[]),
            StreamKind::Unknown(0x87)
        ));
        // 0x06 + unrecognized registration stays Unknown
        let abcd = RawDescriptor {
            tag: 0x05,
            data: b"ABCD".to_vec(),
        };
        assert!(matches!(
            classify_pmt_stream(0x06, &[abcd]),
            StreamKind::Unknown(0x06)
        ));
        // 0x06 + name-tag-only private descriptor (tag 0xFF) stays Unknown
        let name = RawDescriptor {
            tag: 0xFF,
            data: b"SERIAL_ADF".to_vec(),
        };
        assert!(matches!(
            classify_pmt_stream(0x06, &[name]),
            StreamKind::Unknown(0x06)
        ));
    }

    #[test]
    fn classify_pmt_stream_0x15_declared_link() {
        use crate::mpegts::descriptors::RawDescriptor;
        // metadata_descriptor (tag 0x26): extract_metadata_link's lenient
        // shape accepts a >=5-byte body whose trailing 2 bytes fall in the
        // valid PID range (0x0010..=0x1FFE).
        let md = RawDescriptor {
            tag: 0x26,
            data: vec![0x01, 0x00, 0x00, 0x01, 0x00],
        };
        assert!(matches!(
            classify_pmt_stream(0x15, &[md]),
            StreamKind::KlvSync {
                declared_link: Some(0x0100)
            }
        ));
        assert!(matches!(
            classify_pmt_stream(0x15, &[]),
            StreamKind::KlvSync {
                declared_link: None
            }
        ));
    }
}
