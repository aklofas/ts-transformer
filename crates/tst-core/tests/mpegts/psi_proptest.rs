//! Property tests for MPEG-TS PSI build→parse round-trip + descriptors
//! build→parse round-trip + arbitrary-chunking invariance through
//! `Demuxer::feed`.
//!
//! All tests go through the public mux→demux path to avoid exposing
//! internal PSI build helpers.

use proptest::prelude::*;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::low_level::walk_descriptors;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::descriptors;
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

/// Drain all TS packets from a muxer into a single `Vec<u8>`.
fn drain(mux: &mut Muxer) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    bytes
}

/// Drive a fixed-config Muxer once, return the muxed TS bytes used by
/// the chunking-invariance test. Fixed inputs keep the chunking test
/// focused on the boundary-sequence property rather than on muxer
/// behavior — that's covered by `psi_roundtrip` above.
fn mux_fixed_one_program_stream() -> Vec<u8> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let cfg = b.build().expect("fixed config valid");

    let mut mux = Muxer::new(cfg).expect("muxer construct");
    // Minimal Annex-B NAL: 4-byte start code + AUD (NAL type 9, primary_pic_type=0)
    let nal = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    for i in 0..16 {
        mux.push_video(&nal, Pts90khz::new(i * 3000), true)
            .expect("push_video");
    }
    drain(&mut mux)
}

proptest! {
    /// PSI mux→demux round-trip: configure a Muxer with a single program +
    /// video stream, pull all TS bytes, feed to a Demuxer, assert a
    /// `ProgramMap` event carries matching `program_number` and a
    /// `StreamInfo` with the configured video PID.
    #[test]
    fn psi_roundtrip(
        program_number in 1u16..=0xFFFE,
        pmt_pid in 0x0010u16..=0x1FFE,
        video_pid in 0x0010u16..=0x1FFE,
    ) {
        // Valid user PID range is 0x0010..=0x1FFE per pid::is_user_pid; both
        // PMT and stream PIDs must be distinct.
        prop_assume!(pmt_pid != video_pid);

        let mut prog = MuxerProgramConfigBuilder::new(program_number, pmt_pid);
        prog.add_video(video_pid, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().expect("config valid");

        let mut mux = Muxer::new(cfg).expect("muxer construct");
        // Minimal Annex-B AUD NAL — just enough to trigger PES + PSI emission.
        let nal = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        mux.push_video(&nal, Pts90khz::new(0), true).expect("push_video");

        let bytes = drain(&mut mux);
        prop_assume!(!bytes.is_empty());

        let mut demux = Demuxer::new();
        demux.feed(&bytes).expect("demux feed");

        let mut found = false;
        while let Some(evt) = demux.next_event() {
            if let DemuxEvent::ProgramMap(pm) = evt {
                if pm.program_number == program_number
                    && pm.streams.iter().any(|s| s.pid == video_pid)
                {
                    found = true;
                    break;
                }
            }
        }
        prop_assert!(found, "PMT round-trip lost program/stream identity");
    }

    /// Descriptors build/parse round-trip:
    /// `descriptors::registration([_;4], &[])` then `walk_descriptors` yields
    /// one `RawDescriptor` with `tag=0x05` and `data == format_id`.
    #[test]
    fn descriptor_roundtrip(format_id in any::<[u8; 4]>()) {
        let bytes = descriptors::registration(format_id, &[])
            .expect("zero-byte additional within length cap");
        let parsed = walk_descriptors(&bytes).expect("walk_descriptors ok");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x05);
        prop_assert_eq!(&parsed[0].data[..], &format_id[..]);
    }

    /// Metadata KLVA descriptor (tag 0x26) builder → walk round-trip.
    /// `service_id` parameterizes the only variable byte.
    #[test]
    fn metadata_klva_roundtrip(service_id in any::<u8>()) {
        let bytes = descriptors::metadata_klva(service_id);
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x26);
        // Body layout: 0x01 0x00 0xFF 'K' 'L' 'V' 'A' service_id 0x0F
        prop_assert_eq!(parsed[0].data.len(), 9);
        prop_assert_eq!(parsed[0].data[7], service_id);
    }

    /// Metadata STD descriptor (tag 0x27) builder → walk round-trip.
    /// Each rate field is 22 bits (top 2 reserved = 11); the strategy
    /// samples the 22-bit value space directly so the round-trip
    /// asserts byte-identical packing per H.222.0 §2.6.62.
    #[test]
    fn metadata_std_roundtrip(
        input_leak_rate in 0u32..(1u32 << 22),
        buffer_size in 0u32..(1u32 << 22),
        output_leak_rate in 0u32..(1u32 << 22),
    ) {
        let bytes = descriptors::metadata_std(input_leak_rate, buffer_size, output_leak_rate);
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x27);
        // 9 body bytes = 3 packed 22-bit rates. Top 2 bits of each
        // first byte must be 11 per spec.
        prop_assert_eq!(parsed[0].data.len(), 9);
        prop_assert_eq!(parsed[0].data[0] & 0xC0, 0xC0);
        prop_assert_eq!(parsed[0].data[3] & 0xC0, 0xC0);
        prop_assert_eq!(parsed[0].data[6] & 0xC0, 0xC0);
        // Unpack each 22-bit value and assert it matches the input —
        // catches swapped byte order, wrong bit-width masks, or
        // rate1/rate2/rate3 slot mix-up that the reserved-bit check
        // alone would miss.
        let unpack_22 = |b: &[u8]| -> u32 {
            ((b[0] as u32 & 0x3F) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
        };
        prop_assert_eq!(unpack_22(&parsed[0].data[0..3]), input_leak_rate);
        prop_assert_eq!(unpack_22(&parsed[0].data[3..6]), buffer_size);
        prop_assert_eq!(unpack_22(&parsed[0].data[6..9]), output_leak_rate);
    }

    /// User-private descriptor (tag in 0x40..=0xFF) builder → walk
    /// round-trip. Tag and payload both vary; `payload.len() ≤ 255`
    /// to fit in the u8 length field.
    #[test]
    fn user_private_with_tag_roundtrip(
        tag in 0x40u8..=0xFFu8,
        payload in proptest::collection::vec(any::<u8>(), 0..=255),
    ) {
        let bytes = descriptors::user_private_with_tag(tag, &payload)
            .expect("payload strategy capped at 255 bytes");
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, tag);
        prop_assert_eq!(&parsed[0].data[..], &payload[..]);
    }

    /// ISO 639 language descriptor (tag 0x0A) round-trip.
    /// 3-byte language + 1-byte audio_type. Walk-level round-trip is
    /// byte-identical on the body.
    #[test]
    fn iso_639_language_roundtrip(
        language in any::<[u8; 3]>(),
        audio_type in any::<u8>(),
    ) {
        let bytes = descriptors::iso_639_language(language, audio_type);
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x0A);
        prop_assert_eq!(parsed[0].data.len(), 4);
        prop_assert_eq!(&parsed[0].data[..3], &language[..]);
        prop_assert_eq!(parsed[0].data[3], audio_type);
    }

    /// Component descriptor (tag 0x50) round-trip on walker.
    /// Asserts body length + back-byte structure; deep field equality
    /// of the text section is exact because the strategy uses ASCII
    /// (printable subset of UTF-8 — UTF-8 round-trip is identity here).
    #[test]
    fn component_descriptor_roundtrip(
        stream_content_ext in 0u8..=0x0F,
        stream_content in 0u8..=0x0F,
        component_type in any::<u8>(),
        component_tag in any::<u8>(),
        language in any::<[u8; 3]>(),
        // Text length 0..=249 — the helper returns
        // DescriptorError::TooLarge above 249.
        text in "[ -~]{0,249}",
    ) {
        // EN 300 468 §6.2.8: ext must be 0xF for legacy content values 0x1..=0x8.
        prop_assume!(!(0x1..=0x8).contains(&stream_content) || stream_content_ext == 0xF);
        let bytes = descriptors::component(stream_content_ext, stream_content, component_type, component_tag, language, &text)
            .expect("text strategy capped at 249 bytes");
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x50);
        prop_assert_eq!(parsed[0].data.len(), 6 + text.len());
        // First byte: high nibble = stream_content_ext, low nibble = stream_content
        prop_assert_eq!(parsed[0].data[0] >> 4, stream_content_ext);
        prop_assert_eq!(parsed[0].data[0] & 0x0F, stream_content);
        prop_assert_eq!(parsed[0].data[1], component_type);
        prop_assert_eq!(parsed[0].data[2], component_tag);
        prop_assert_eq!(&parsed[0].data[3..6], &language[..]);
        prop_assert_eq!(&parsed[0].data[6..], text.as_bytes());
    }

    /// Stream identifier descriptor (tag 0x52) round-trip on walker.
    /// Single-byte body carrying `component_tag` per ETSI EN 300 468
    /// §6.2.39 — pairs with a Component descriptor's `component_tag`
    /// field. Trivial wire format, included for completeness so the
    /// descriptor-proptest set covers every parameterized builder.
    #[test]
    fn stream_identifier_roundtrip(component_tag in any::<u8>()) {
        let bytes = descriptors::stream_identifier(component_tag);
        let parsed = walk_descriptors(&bytes).expect("walk");
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].tag, 0x52);
        prop_assert_eq!(parsed[0].data.len(), 1);
        prop_assert_eq!(parsed[0].data[0], component_tag);
    }

    /// DVB subtitling_descriptor multi-entry (tag 0x59) typed round-trip.
    /// Build via `subtitling_descriptor_multi`, parse via the typed
    /// parser `parse_subtitling_descriptor`, assert structural equality
    /// of each `SubtitlingDescriptorEntry`.
    #[test]
    fn subtitling_descriptor_multi_typed_roundtrip(
        entries in proptest::collection::vec(
            (any::<[u8; 3]>(), any::<u8>(), any::<u16>(), any::<u16>()),
            1..=8, // u8 length field caps body at 255 / 8 bytes per entry = 31 max
        ),
    ) {
        let bytes = descriptors::subtitling_descriptor_multi(&entries)
            .expect("non-empty entries");
        // walk_descriptors returns the [tag, length, body...] envelope;
        // parse_subtitling_descriptor expects the body only. Skip the
        // first 2 envelope bytes.
        let parsed = tst_core::mpegts::descriptors::parse_subtitling_descriptor(&bytes[2..])
            .expect("typed parse");
        prop_assert_eq!(parsed.len(), entries.len());
        for (got, (lang, sub_type, comp_id, anc_id)) in parsed.iter().zip(entries.iter()) {
            prop_assert_eq!(got.language, *lang);
            prop_assert_eq!(got.subtitling_type, *sub_type);
            prop_assert_eq!(got.composition_page_id, *comp_id);
            prop_assert_eq!(got.ancillary_page_id, *anc_id);
        }
    }

    /// DVB teletext_descriptor multi-entry (tag 0x56) typed round-trip.
    /// Mirror of the subtitling case. `teletext_type` is 5 bits;
    /// `magazine_number` is 3 bits — the helper packs them into one
    /// byte, the typed parser unpacks. The strategy samples the full
    /// u8 domain for `teletext_type` and `magazine_number` because the
    /// helper masks them (`& 0x1F`, `& 0x07`) — round-trip asserts
    /// the parsed values match the masked inputs.
    #[test]
    fn teletext_descriptor_multi_typed_roundtrip(
        entries in proptest::collection::vec(
            (any::<[u8; 3]>(), any::<u8>(), any::<u8>(), any::<u8>()),
            1..=8, // each entry = 5 bytes; cap below u8 length boundary
        ),
    ) {
        let bytes = descriptors::teletext_descriptor_multi(&entries)
            .expect("non-empty entries");
        let parsed = tst_core::mpegts::descriptors::parse_teletext_descriptor(&bytes[2..])
            .expect("typed parse");
        prop_assert_eq!(parsed.len(), entries.len());
        for (got, (lang, tt_type, mag, page)) in parsed.iter().zip(entries.iter()) {
            prop_assert_eq!(got.language, *lang);
            // Pack/unpack masks: tt_type keeps low 5 bits; mag keeps low 3.
            prop_assert_eq!(got.teletext_type, tt_type & 0x1F);
            prop_assert_eq!(got.magazine_number, mag & 0x07);
            // page_number is BCD-encoded per spec, but the builder/parser
            // pass it through byte-identical; the proptest verifies the
            // wire round-trip only, not BCD semantic validity.
            prop_assert_eq!(got.page_number, *page);
        }
    }

    /// Demuxer chunking invariance: feeding a fixed muxed TS stream as one
    /// whole feed must produce the same event sequence as feeding it split
    /// at arbitrary boundaries. Catches buffer-boundary bugs in the framing
    /// state machine.
    #[test]
    fn demuxer_chunking_invariant(
        // 8192 chosen to comfortably exceed the fixed stream length
        // (~5.2 KiB with 16 video pushes) so the modulo mapping at line
        // 126 actually distributes boundaries across the full stream
        // rather than clustering in the first half.
        boundaries in proptest::collection::vec(0usize..=8192, 0..=32),
    ) {
        let bytes = mux_fixed_one_program_stream();
        prop_assume!(!bytes.is_empty());

        let mut once = Demuxer::new();
        once.feed(&bytes).expect("feed whole");
        let events_once: Vec<_> = std::iter::from_fn(|| once.next_event()).collect();

        // Map proptest-generated raw indices into the valid `[0, bytes.len()]`
        // range, sort + dedup, and always include the final byte index so the
        // last chunk gets fed.
        let mut splits: Vec<usize> = boundaries
            .into_iter()
            .map(|b| b % (bytes.len() + 1))
            .collect();
        splits.sort_unstable();
        splits.dedup();
        if splits.last().copied() != Some(bytes.len()) {
            splits.push(bytes.len());
        }

        let mut chunked = Demuxer::new();
        let mut last = 0;
        for s in splits {
            chunked.feed(&bytes[last..s]).expect("feed chunk");
            last = s;
        }
        let events_chunked: Vec<_> =
            std::iter::from_fn(|| chunked.next_event()).collect();

        prop_assert_eq!(events_once, events_chunked);
    }
}
