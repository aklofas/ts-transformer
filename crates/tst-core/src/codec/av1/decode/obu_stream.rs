//! AV1 OBU stream collector. Per AV1 spec §5.2.

use crate::codec::CodecParseError;
use crate::codec::Obu;
use crate::codec::av1::model::{Av1FrameHeaderLight, Av1ObuStream, Av1SequenceHeader};
use alloc::vec::Vec;

use super::frame_header::parse_frame_header_light;
use super::sequence_header::parse_sequence_header;

/// Walk a `Vec<Obu>` and collect typed structs. Partial-success-tolerant.
/// State limited to "current sequence header" for frame-header parsing.
/// Other OBU types (TemporalDelimiter, TileGroup, Metadata, RedundantFrameHeader,
/// TileList, Padding) pass through unparsed and are not recorded — they're
/// payload-only or not load-bearing for metadata extraction.
pub fn parse_obu_stream(obus: &[Obu]) -> Av1ObuStream {
    let mut sequence_headers: Vec<Av1SequenceHeader> = Vec::new();
    let mut frame_headers: Vec<Av1FrameHeaderLight> = Vec::new();
    let mut unparseable: Vec<(u8, CodecParseError)> = Vec::new();
    let mut current_seq: Option<Av1SequenceHeader> = None;

    for obu in obus {
        match obu.obu_type {
            1 => match parse_sequence_header(&obu.payload) {
                Ok(sh) => {
                    current_seq = Some(sh.clone());
                    sequence_headers.push(sh);
                }
                Err(e) => unparseable.push((1, e)),
            },
            // OBU_FRAME_HEADER (3) and OBU_FRAME (6) both carry an
            // uncompressed_header. We route both through the light
            // frame_header parser; OBU_FRAME's tile group bytes beyond
            // the header are not consumed (light-scope by design).
            3 | 6 => {
                if let Some(seq) = &current_seq {
                    match parse_frame_header_light(&obu.payload, seq) {
                        Ok(fh) => frame_headers.push(fh),
                        Err(e) => unparseable.push((obu.obu_type, e)),
                    }
                } else {
                    unparseable.push((
                        obu.obu_type,
                        CodecParseError::EngineError("frame header before sequence header".into()),
                    ));
                }
            }
            // OBU_TEMPORAL_DELIMITER (2), OBU_TILE_GROUP (4), OBU_METADATA (5),
            // OBU_REDUNDANT_FRAME_HEADER (7), OBU_TILE_LIST (8), OBU_PADDING (15):
            // pass-through.
            _ => {}
        }
    }

    Av1ObuStream {
        sequence_headers,
        frame_headers,
        unparseable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Obu;

    /// Minimal Sequence Header OBU body — captured byte-for-byte from
    /// `codec::av1::sequence_header::tests::minimal_sequence_header`
    /// (Main profile, level 2.0, 320x240, 8-bit 4:2:0, no color desc,
    /// no timing info).
    fn minimal_seq_header_body() -> Vec<u8> {
        vec![0, 0, 0, 4, 60, 255, 188, 0, 0, 0]
    }

    /// show_existing_frame=0, frame_type=KEY_FRAME(0), show_frame=1 →
    /// 4 bits 0001 in the high nibble = 0x10.
    fn keyframe_body() -> Vec<u8> {
        vec![0x10]
    }

    #[test]
    fn parse_obu_stream_collects_seq_header_then_frame_header() {
        let obus = vec![
            Obu {
                obu_type: 2,
                extension: None,
                payload: vec![].into(),
            }, // TD
            Obu {
                obu_type: 1,
                extension: None,
                payload: minimal_seq_header_body().into(),
            },
            Obu {
                obu_type: 3,
                extension: None,
                payload: keyframe_body().into(),
            },
        ];
        let stream = parse_obu_stream(&obus);
        assert_eq!(stream.sequence_headers.len(), 1);
        assert_eq!(stream.frame_headers.len(), 1);
        assert!(stream.unparseable.is_empty());
    }

    #[test]
    fn parse_obu_stream_records_failures_in_unparseable() {
        let obus = vec![
            Obu {
                obu_type: 1,
                extension: None,
                payload: vec![].into(),
            }, // truncated SH
        ];
        let stream = parse_obu_stream(&obus);
        assert_eq!(stream.sequence_headers.len(), 0);
        assert_eq!(stream.unparseable.len(), 1);
        assert_eq!(stream.unparseable[0].0, 1);
    }

    #[test]
    fn parse_obu_stream_frame_header_without_seq_header_in_unparseable() {
        let obus = vec![Obu {
            obu_type: 3,
            extension: None,
            payload: keyframe_body().into(),
        }];
        let stream = parse_obu_stream(&obus);
        assert_eq!(stream.sequence_headers.len(), 0);
        assert_eq!(stream.frame_headers.len(), 0);
        assert_eq!(stream.unparseable.len(), 1);
        assert_eq!(stream.unparseable[0].0, 3);
    }

    #[test]
    fn parse_obu_stream_skips_unknown_obu_types() {
        let obus = vec![
            Obu {
                obu_type: 5,
                extension: None,
                payload: vec![0x00].into(),
            }, // Metadata — pass through
            Obu {
                obu_type: 4,
                extension: None,
                payload: vec![0x00].into(),
            }, // TileGroup — pass through
        ];
        let stream = parse_obu_stream(&obus);
        assert_eq!(stream.sequence_headers.len(), 0);
        assert_eq!(stream.frame_headers.len(), 0);
        assert!(stream.unparseable.is_empty());
    }
}
