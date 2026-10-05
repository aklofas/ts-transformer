//! PSI section generation — PAT and PMT.
//!
//! Both are single-section, fit in one TS packet for the current scope
//! (one program, one or two elementary streams). The CRC-32/MPEG-2 over
//! the section is computed via `crate::mpegts::common::crc32::crc32_mpeg2`.
//!
//! Padding convention: PSI packets fill unused payload bytes with `0xFF`
//! *within* the TS packet payload (after the section), not via the
//! adaptation field. The pointer field (always 0 for our single-section
//! packets) tells receivers where the section starts.

use super::ts::{AdaptationField, ContinuityCounters, write_packet};
use crate::mpegts::common::{crc32::crc32_mpeg2, descriptor, pid};

/// Build the full 188-byte PAT packet for a single- or multi-program TS.
///
/// Iterates over all programs in `config` and encodes one `(program_number,
/// pmt_pid)` entry per program. Single-program configs produce byte-identical
/// output to the prior hardcoded single-entry implementation.
///
/// Caller passes the shared `ContinuityCounters` so the PAT CC field
/// increments correctly across successive PSI ticks.
pub(crate) fn write_pat_packet(
    out: &mut [u8; 188],
    config: &crate::mpegts::mux::MuxerConfig,
    counters: &mut ContinuityCounters,
) {
    let n_programs = config.programs.len();
    // section_length = bytes after the section_length field itself through the
    // final CRC byte:
    //   tsid(2) + ver/cni(1) + sect_no(1) + last_sect(1) = 5 fixed bytes
    //   + 4 bytes per program entry (program_number(2) + reserved+pmt_pid(2))
    //   + CRC(4)
    //   = 9 + 4 * n_programs
    let section_length: usize = 9 + 4 * n_programs;

    // Full payload: pointer(1) + table_id(1) + length(2) + fixed header(5)
    //               + program loop + CRC(4) + 0xFF padding.
    let mut payload = [0xFFu8; 184];
    let mut idx = 0usize;

    payload[idx] = 0x00; // pointer field — section starts immediately after
    idx += 1;
    // table_id = 0x00 (PAT)
    payload[idx] = 0x00;
    idx += 1;
    // section_syntax_indicator=1 | '0' | reserved=11 | section_length(12 bits)
    payload[idx] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
    payload[idx + 1] = (section_length & 0xFF) as u8;
    idx += 2;
    // transport_stream_id = 0x0001
    payload[idx] = 0x00;
    payload[idx + 1] = 0x01;
    idx += 2;
    // version_number(5)=0 | current_next_indicator=1 → 0xC1 (reserved bits 11)
    payload[idx] = 0xC1;
    idx += 1;
    payload[idx] = 0x00; // section_number
    payload[idx + 1] = 0x00; // last_section_number
    idx += 2;

    // Program loop: one 4-byte entry per program.
    let body_start = 1; // pointer byte excluded from CRC body; table_id is at payload[1]
    for prog in &config.programs {
        payload[idx] = (prog.program_number >> 8) as u8;
        payload[idx + 1] = (prog.program_number & 0xFF) as u8;
        idx += 2;
        // reserved(3 bits) = 0b111 | pmt_pid(13 bits)
        payload[idx] = 0xE0 | ((prog.pmt_pid >> 8) as u8 & 0x1F);
        payload[idx + 1] = (prog.pmt_pid & 0xFF) as u8;
        idx += 2;
    }

    // CRC over section body: from table_id through last program entry byte.
    let crc = crc32_mpeg2(&payload[body_start..idx]);
    payload[idx..idx + 4].copy_from_slice(&crc.to_be_bytes());
    // Bytes after CRC remain 0xFF from the initialiser.

    write_packet(
        out,
        pid::PAT,
        true,
        AdaptationField::default(),
        &payload,
        counters,
    );
}

/// PMT entry for one elementary stream.
pub(crate) struct PmtStreamEntry<'a> {
    /// Raw PMT stream_type byte. Typed streams reduce their
    /// `StreamType` enum via `.as_u8()` at entry-build time
    /// (`scheduling.rs`); `StreamSpec::Data` carries the caller-chosen
    /// byte through verbatim.
    pub stream_type: u8,
    pub elementary_pid: u16,
    /// Pre-composed descriptor-loop bytes for this ES (already concatenated
    /// across auto-emitted + caller-supplied descriptors). Empty slice
    /// means no descriptors. Owned by the [`crate::mpegts::mux::Muxer`]'s
    /// per-stream cache, borrowed at PMT emission time.
    pub descriptors: &'a [u8],
}

/// Pre-built KLVA registration descriptor body.
/// descriptor_tag(1) + descriptor_length(1) + format_identifier(4)
pub(crate) const KLVA_REGISTRATION_DESCRIPTOR: &[u8] = &[
    descriptor::REGISTRATION,
    4,
    descriptor::KLVA[0],
    descriptor::KLVA[1],
    descriptor::KLVA[2],
    descriptor::KLVA[3],
];

/// Maximum PMT section body size that fits in one 188-byte TS packet.
/// 188 - 4 (TS header) - 1 (pointer field) = 183 bytes.
pub(crate) const MAX_PMT_SECTION_BYTES: usize = 183;

/// Estimate the PMT section body size (bytes) for a `MuxerProgramConfig`.
///
/// Used by `MuxerConfig::validate()` to reject configurations that would
/// produce a PMT section too large for one TS packet.
///
/// Derived directly from `build_pmt_descriptor_cache` so the estimator and
/// the emitter are structurally identical — any suppression rule added or
/// changed in the cache builder is automatically reflected here.
/// The only overhead vs. real construction is an extra `Vec` allocation at
/// validation time; this is construction-time cost, not per-push.
pub(crate) fn estimate_pmt_section_size(prog: &crate::mpegts::mux::MuxerProgramConfig) -> usize {
    let cache = super::state::build_pmt_descriptor_cache(prog);
    // Each cache entry contains the full descriptor-loop bytes for one ES
    // (auto-emitted + caller-supplied). The PMT ES entry overhead is 5 bytes:
    // stream_type(1) + reserved+ES_PID(2) + reserved+ES_info_length(2).
    let es_loop_size: usize = cache.iter().map(|d| 5 + d.len()).sum();
    let program_info_len: usize = prog.program_descriptors.iter().map(|d| d.len()).sum();
    // table_id(1) + section_syntax+length(2) + program_number(2) +
    // ver+curr(1) + section_number(1) + last_section_number(1) +
    // reserved+PCR_PID(2) + reserved+program_info_length(2) + program_descs + es_loop + CRC(4)
    3 + 9 + program_info_len + es_loop_size + 4
}

/// Build the full 188-byte PMT packet for one program.
///
/// `prog.pmt_pid` is used as the TS packet PID (the PAT points receivers
/// here). `prog.program_number` is encoded in the PMT section header.
/// `pcr_pid` is the resolved PCR PID for this program (may differ from
/// `prog.pcr_pid` if auto-fallback was applied at `Muxer::new` time).
/// `streams` is the list of ES entries — order is preserved in the PMT.
pub(crate) fn write_pmt_packet(
    out: &mut [u8; 188],
    prog: &crate::mpegts::mux::MuxerProgramConfig,
    pcr_pid: u16,
    streams: &[PmtStreamEntry<'_>],
    counters: &mut ContinuityCounters,
) -> Result<(), crate::error::MuxError> {
    // Compute the section body size first so we can fill in section_length.
    // PMT body layout:
    //   table_id(1) + section_syntax+length(2) + program_number(2) +
    //   ver+curr(1) + section_number(1) + last_section_number(1) +
    //   reserved+PCR_PID(2) + reserved+program_info_length(2) +
    //   [program_info_descriptors=0] +
    //   ES loop entries +
    //   CRC(4)
    let mut es_loop_size: usize = 0;
    for s in streams {
        // stream_type(1) + reserved+ES_PID(2) + reserved+ES_info_length(2) + descriptors
        es_loop_size += 5 + s.descriptors.len();
    }
    let program_info_length_usize: usize = prog.program_descriptors.iter().map(|d| d.len()).sum();
    // section_length covers everything after itself: 9 (program/ver/sect/PCR/info_len header)
    // + program_descs + es_loop_size + 4 (CRC).
    let section_length: u16 = 9 + program_info_length_usize as u16 + es_loop_size as u16 + 4;
    let total_body_size: usize = 3 + section_length as usize; // table_id + length_bytes + section_length

    // program_info_length is a 12-bit field (top 4 reserved). Reject configs whose
    // program-level descriptors overflow it.
    if program_info_length_usize >= 0x400 {
        return Err(crate::error::MuxError::PmtTooLarge {
            used_bytes: total_body_size,
            max_bytes: 183,
        });
    }

    // PMT must fit in one TS packet. 188 - 4 (TS header) - 1 (pointer) = 183.
    if total_body_size > 183 {
        return Err(crate::error::MuxError::PmtTooLarge {
            used_bytes: total_body_size,
            max_bytes: 183,
        });
    }

    // Full 184-byte payload: pointer(1) + body(total_body_size) + 0xFF padding.
    let mut payload = [0xFFu8; 184];
    payload[0] = 0x00; // pointer field
    let body_start = 1;
    let mut idx = body_start;

    payload[idx] = 0x02; // table_id = PMT
    idx += 1;
    payload[idx] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
    payload[idx + 1] = (section_length & 0xFF) as u8;
    idx += 2;
    // program_number — use the actual program number from the config (not a constant).
    payload[idx] = (prog.program_number >> 8) as u8;
    payload[idx + 1] = (prog.program_number & 0xFF) as u8;
    idx += 2;
    // version+current
    payload[idx] = 0xC1;
    idx += 1;
    payload[idx] = 0x00; // section_number
    payload[idx + 1] = 0x00; // last_section_number
    idx += 2;
    // reserved(3) + PCR_PID(13)
    payload[idx] = 0xE0 | ((pcr_pid >> 8) as u8 & 0x1F);
    payload[idx + 1] = (pcr_pid & 0xFF) as u8;
    idx += 2;
    // reserved(4) + program_info_length(12) — caller-supplied descriptor bytes count.
    let program_info_length: u16 = program_info_length_usize as u16;
    payload[idx] = 0xF0 | (((program_info_length >> 8) as u8) & 0x0F);
    payload[idx + 1] = (program_info_length & 0xFF) as u8;
    idx += 2;
    // Program-level descriptors. H.222.0 V9 §2.4.4.9 Table 2-33 mandates the
    // descriptor()_loop sits between program_info_length and the per-stream loop.
    for desc in &prog.program_descriptors {
        payload[idx..idx + desc.len()].copy_from_slice(desc);
        idx += desc.len();
    }

    // ES loop
    for s in streams {
        payload[idx] = s.stream_type;
        idx += 1;
        payload[idx] = 0xE0 | ((s.elementary_pid >> 8) as u8 & 0x1F);
        payload[idx + 1] = (s.elementary_pid & 0xFF) as u8;
        idx += 2;
        let es_info_length = s.descriptors.len() as u16;
        payload[idx] = 0xF0 | (((es_info_length >> 8) as u8) & 0x0F);
        payload[idx + 1] = (es_info_length & 0xFF) as u8;
        idx += 2;
        payload[idx..idx + s.descriptors.len()].copy_from_slice(s.descriptors);
        idx += s.descriptors.len();
    }

    // CRC over body (table_id through last ES descriptor byte).
    let crc = crc32_mpeg2(&payload[body_start..idx]);
    payload[idx..idx + 4].copy_from_slice(&crc.to_be_bytes());
    // Bytes after the CRC remain 0xFF from the initialiser.

    // Emit on prog.pmt_pid — the PAT entry points receivers to this PID.
    write_packet(
        out,
        prog.pmt_pid,
        true,
        AdaptationField::default(),
        &payload,
        counters,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpegts::common::{StreamType, pid};
    use crate::mpegts::mux::{
        KlvStreamType, MuxerConfig, MuxerProgramConfig, MuxerProgramConfigBuilder, StreamSpec,
        VideoCodec,
    };

    /// Build a minimal single-program config for unit tests.
    fn single_prog_config() -> MuxerConfig {
        MuxerConfig::default()
    }

    /// Build a minimal MuxerProgramConfig for tests that call write_pmt_packet directly.
    fn prog_config_h264_klv() -> MuxerProgramConfig {
        MuxerProgramConfig {
            program_number: 1,
            pmt_pid: 0x1000,
            streams: vec![
                StreamSpec::Video {
                    pid: 0x1011,
                    codec: VideoCodec::H264,
                },
                StreamSpec::Klv {
                    pid: 0x1031,
                    stream_type: KlvStreamType::PrivateData,
                    carries_pts: false,
                },
            ],
            pcr_pid: None,
            program_descriptors: Vec::new(),
            stream_descriptors: vec![Vec::new(), Vec::new()],
        }
    }

    /// Re-decode a PAT from our generated bytes and assert PMT PID matches.
    #[test]
    fn pat_round_trips() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let cfg = single_prog_config();
        write_pat_packet(&mut buf, &cfg, &mut cc);

        // TS header sanity
        assert_eq!(buf[0], 0x47);
        // PID 0
        let pid_lo = buf[2];
        let pid_hi = buf[1] & 0x1F;
        assert_eq!(((pid_hi as u16) << 8) | pid_lo as u16, pid::PAT);
        // PUSI set
        assert_eq!(buf[1] & 0x40, 0x40);
        // afc = 01 (no AF, just payload — section + 0xFF padding inside payload)
        assert_eq!(buf[3] >> 4, 0b01);

        // Section starts at byte 4 (no AF) + 1 (pointer) = byte 5.
        assert_eq!(buf[4], 0x00); // pointer = 0
        assert_eq!(buf[5], 0x00); // table_id = PAT

        // Single program: body is 12 bytes (table_id through last byte before CRC) at buf[5..17].
        // CRC at buf[17..21].
        let body = &buf[5..17];
        let crc_computed = crc32_mpeg2(body);
        let crc_in_packet = ((buf[17] as u32) << 24)
            | ((buf[18] as u32) << 16)
            | ((buf[19] as u32) << 8)
            | (buf[20] as u32);
        assert_eq!(crc_computed, crc_in_packet);

        // PMT PID embedded in last 2 bytes of program loop = body[10..12] = buf[15..17].
        let pmt_pid = (((buf[15] as u16) & 0x1F) << 8) | (buf[16] as u16);
        assert_eq!(
            pmt_pid, 0x1000,
            "single-program PAT must list pmt_pid=0x1000"
        );

        // Padding after CRC should be 0xFF (sanity check).
        assert_eq!(buf[21], 0xFF);
        assert_eq!(buf[187], 0xFF);
    }

    /// `estimate_pmt_section_size` must count every auto-emitted
    /// descriptor: KLVA, AV01 (6 B), DVB subtitling (10 B), DVB teletext
    /// (7 B), CEA-708 GA94 (6 B), WebVTT VTTC (6 B). Counting KLVA alone
    /// lets configs near the 183-byte budget pass `validate()` and then
    /// fail at PMT emission with `PmtTooLarge`.
    ///
    /// This test builds a config with 15 DVB-sub streams whose auto-emit
    /// (10 bytes each = 150 bytes) blows past the budget. An estimate that
    /// ignores auto-emit comes in at ~96 bytes and returns Ok; `validate()`
    /// must return Err(PmtTooLarge).
    #[test]
    fn estimate_pmt_size_includes_subtitle_auto_emit() {
        use crate::mpegts::mux::SubtitleCodec;

        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        for i in 0..15u16 {
            prog.add_subtitle(
                0x200 + i,
                SubtitleCodec::DvbSubtitling {
                    language: *b"eng",
                    subtitling_type: 0x10,
                    composition_page_id: 1,
                    ancillary_page_id: 1,
                },
            );
        }
        let mut builder = MuxerConfig::builder();
        builder.add_program(prog.build());
        let result = builder.build();
        match result {
            Err(crate::error::MuxError::PmtTooLarge { used_bytes, .. }) => {
                assert!(
                    used_bytes > MAX_PMT_SECTION_BYTES,
                    "PmtTooLarge must report a size > {MAX_PMT_SECTION_BYTES}, got {used_bytes}",
                );
            }
            other => panic!(
                "expected Err(PmtTooLarge) — config has 15 DVB-sub streams whose \
                 auto-emit (10 B each) blows past {MAX_PMT_SECTION_BYTES} byte budget; \
                 got {other:?}",
            ),
        }
    }

    /// Caller-supplied program-level descriptors must reach the PMT, not be
    /// silently dropped by the PMT writer. Per H.222.0 V9 §2.4.4.9 Table 2-33 (PDF
    /// p.79), the descriptor()_loop between program_info_length and the per-stream
    /// loop carries program-level descriptors. Public builder method
    /// `MuxerProgramConfigBuilder::program_descriptors(...)` accepted them, but the
    /// writer hardcoded `program_info_length=0` and never wrote the bytes.
    #[test]
    fn pmt_serializes_program_level_descriptors() {
        use crate::mpegts::descriptors::iso_639_language;

        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let mut prog = prog_config_h264_klv();
        // ISO 639 language descriptor: tag(1) + length=4(1) + lang(3) + audio_type(1) = 6 bytes.
        let lang_desc = iso_639_language(*b"eng", 0);
        assert_eq!(lang_desc.len(), 6, "iso_639_language returns 6 bytes");
        prog.program_descriptors = vec![lang_desc.clone()];

        let streams = [
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x1011,
                descriptors: &[],
            },
            PmtStreamEntry {
                stream_type: StreamType::KlvPrivate.as_u8(),
                elementary_pid: 0x1031,
                descriptors: KLVA_REGISTRATION_DESCRIPTOR,
            },
        ];
        write_pmt_packet(&mut buf, &prog, 0x1011, &streams, &mut cc).expect("PMT fits");

        // Section starts at TS header(4) + pointer(1) = byte 5. Layout:
        //   [5] table_id=0x02
        //   [6..8] section_syntax+length
        //   [8..10] program_number
        //   [10] version+current
        //   [11] section_number
        //   [12] last_section_number
        //   [13..15] reserved+PCR_PID
        //   [15..17] reserved+program_info_length
        //   [17..17+pil] program-level descriptors  ← our 6 bytes land here
        //   then ES loop, then CRC.
        let program_info_length = (((buf[15] as u16) & 0x0F) << 8) | (buf[16] as u16);
        assert_eq!(
            program_info_length as usize,
            lang_desc.len(),
            "program_info_length must reflect the program_descriptors byte count"
        );
        assert_eq!(
            &buf[17..17 + lang_desc.len()],
            &lang_desc[..],
            "program-level descriptor bytes must appear after program_info_length"
        );

        // Sanity: CRC still validates over the body.
        let section_length = (((buf[6] as u16) & 0x0F) << 8) | (buf[7] as u16);
        let body_end = 5 + 3 + section_length as usize - 4;
        let crc_in_packet = ((buf[body_end] as u32) << 24)
            | ((buf[body_end + 1] as u32) << 16)
            | ((buf[body_end + 2] as u32) << 8)
            | (buf[body_end + 3] as u32);
        let crc_computed = crc32_mpeg2(&buf[5..body_end]);
        assert_eq!(
            crc_in_packet, crc_computed,
            "CRC must validate after program_descriptors are serialized"
        );
    }

    #[test]
    fn pmt_round_trips_with_klva() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let prog = prog_config_h264_klv();
        let streams = [
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x1011,
                descriptors: &[],
            },
            PmtStreamEntry {
                stream_type: StreamType::KlvPrivate.as_u8(),
                elementary_pid: 0x1031,
                descriptors: KLVA_REGISTRATION_DESCRIPTOR,
            },
        ];
        write_pmt_packet(&mut buf, &prog, 0x1011, &streams, &mut cc).expect("PMT fits");

        assert_eq!(buf[0], 0x47);
        // PMT PID — must match prog.pmt_pid (0x1000)
        let pid_value = (((buf[1] as u16) & 0x1F) << 8) | (buf[2] as u16);
        assert_eq!(pid_value, 0x1000, "PMT packet PID must equal prog.pmt_pid");
        // afc = 01 (no AF)
        assert_eq!(buf[3] >> 4, 0b01);
        // pointer = 0
        assert_eq!(buf[4], 0x00);
        // table_id = PMT
        assert_eq!(buf[5], 0x02);

        // PCR_PID at body[8..10] = buf[13..15].
        let pcr_pid = (((buf[13] as u16) & 0x1F) << 8) | (buf[14] as u16);
        assert_eq!(pcr_pid, 0x1011);

        // ES loop starts at buf[17] (after program_info_length=0 at body[10..12] = buf[15..17]).
        // Entry 1: stream_type=0x1B, ES_PID=0x1011, ES_info_length=0
        assert_eq!(buf[17], 0x1B);
        let es_pid_1 = (((buf[18] as u16) & 0x1F) << 8) | (buf[19] as u16);
        assert_eq!(es_pid_1, 0x1011);
        let es_info_len_1 = (((buf[20] as u16) & 0x0F) << 8) | (buf[21] as u16);
        assert_eq!(es_info_len_1, 0);

        // Entry 2: stream_type=0x06, ES_PID=0x1031, ES_info_length=6 (KLVA descriptor)
        assert_eq!(buf[22], 0x06);
        let es_pid_2 = (((buf[23] as u16) & 0x1F) << 8) | (buf[24] as u16);
        assert_eq!(es_pid_2, 0x1031);
        let es_info_len_2 = (((buf[25] as u16) & 0x0F) << 8) | (buf[26] as u16);
        assert_eq!(es_info_len_2, 6);
        // KLVA descriptor: 0x05, 0x04, 'K', 'L', 'V', 'A'
        assert_eq!(&buf[27..33], &[0x05, 0x04, 0x4B, 0x4C, 0x56, 0x41]);
    }

    #[test]
    fn pmt_with_sync_klv_stream_type() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let prog = MuxerProgramConfig {
            program_number: 1,
            pmt_pid: 0x1000,
            streams: vec![
                StreamSpec::Video {
                    pid: 0x1011,
                    codec: VideoCodec::H265,
                },
                StreamSpec::Klv {
                    pid: 0x1031,
                    stream_type: KlvStreamType::SynchronousMetadata,
                    carries_pts: true,
                },
            ],
            pcr_pid: None,
            program_descriptors: Vec::new(),
            stream_descriptors: vec![Vec::new(), Vec::new()],
        };
        let streams = [
            PmtStreamEntry {
                stream_type: StreamType::H265.as_u8(),
                elementary_pid: 0x1011,
                descriptors: &[],
            },
            PmtStreamEntry {
                stream_type: StreamType::KlvSyncMetadata.as_u8(),
                elementary_pid: 0x1031,
                descriptors: KLVA_REGISTRATION_DESCRIPTOR,
            },
        ];
        write_pmt_packet(&mut buf, &prog, 0x1011, &streams, &mut cc).expect("PMT fits");
        // H.265 stream_type
        assert_eq!(buf[17], 0x24);
        // Sync KLV stream_type
        assert_eq!(buf[22], 0x15);
    }

    #[test]
    fn pmt_emits_h266_stream_type_0x33() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let prog = MuxerProgramConfig {
            program_number: 1,
            pmt_pid: 0x1000,
            streams: vec![StreamSpec::Video {
                pid: 0x1011,
                codec: VideoCodec::H266,
            }],
            pcr_pid: None,
            program_descriptors: Vec::new(),
            stream_descriptors: vec![Vec::new()],
        };
        let streams = [PmtStreamEntry {
            stream_type: StreamType::H266.as_u8(),
            elementary_pid: 0x1011,
            descriptors: &[],
        }];
        write_pmt_packet(&mut buf, &prog, 0x1011, &streams, &mut cc).expect("PMT fits");
        // ES loop entry 1 stream_type byte at buf[17] — same offset as
        // pmt_round_trips_with_klva (single-program, no program-info loop).
        assert_eq!(
            buf[17], 0x33,
            "expected H.266 stream_type 0x33, got {:#x}",
            buf[17]
        );
    }

    #[test]
    fn pmt_too_large_returns_error() {
        use crate::error::MuxError;
        use crate::mpegts::common::StreamType;

        // Build 4 streams, each carrying a 255-byte descriptor blob.
        // write_pmt_packet doesn't validate TLV well-formedness — it only
        // checks total section size. Sum: 4 * (5 ES-header + 255 desc) =
        // 1040 bytes — far beyond the 183-byte single-packet PMT budget.
        let big_desc = vec![0xFFu8; 255];
        let entries = [
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x100,
                descriptors: &big_desc,
            },
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x101,
                descriptors: &big_desc,
            },
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x102,
                descriptors: &big_desc,
            },
            PmtStreamEntry {
                stream_type: StreamType::H264.as_u8(),
                elementary_pid: 0x103,
                descriptors: &big_desc,
            },
        ];
        let prog = MuxerProgramConfig {
            program_number: 1,
            pmt_pid: 0x1000,
            streams: Vec::new(), // streams field unused by write_pmt_packet directly
            pcr_pid: None,
            program_descriptors: Vec::new(),
            stream_descriptors: Vec::new(),
        };
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let err = write_pmt_packet(&mut buf, &prog, 0x100, &entries, &mut cc).unwrap_err();
        assert!(matches!(err, MuxError::PmtTooLarge { .. }));
    }

    #[test]
    fn pmt_crc_validates() {
        let mut buf = [0u8; 188];
        let mut cc = ContinuityCounters::new();
        let prog = prog_config_h264_klv();
        let streams = [PmtStreamEntry {
            stream_type: StreamType::H264.as_u8(),
            elementary_pid: 0x1011,
            descriptors: &[],
        }];
        write_pmt_packet(&mut buf, &prog, 0x1011, &streams, &mut cc).expect("PMT fits");

        // section_length is at bytes 6-7
        let section_length = (((buf[6] as u16) & 0x0F) << 8) | (buf[7] as u16);
        // Body for CRC starts at buf[5] (table_id). Total section bytes from
        // buf[5] = 3 (table_id + length_bytes) + section_length. CRC is the last
        // 4 of those.
        let body_end = 5 + 3 + section_length as usize - 4;
        let body = &buf[5..body_end];
        let crc_computed = crc32_mpeg2(body);
        let crc_in_packet = ((buf[body_end] as u32) << 24)
            | ((buf[body_end + 1] as u32) << 16)
            | ((buf[body_end + 2] as u32) << 8)
            | (buf[body_end + 3] as u32);
        assert_eq!(crc_computed, crc_in_packet);
    }
}
