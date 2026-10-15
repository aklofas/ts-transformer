//! AV1 Frame Header parser (light scope). Per AV1 spec §5.9.

use super::bitreader::Av1BitReader;
use crate::codec::CodecParseError;
use crate::codec::av1::model::{Av1FrameHeaderLight, Av1SequenceHeader};

/// Parse an AV1 Frame Header OBU body (light scope). Per AV1 spec §5.9.1.
///
/// Light scope: extracts `frame_type` + `show_frame` + `show_existing_frame`
/// only. Per-frame size override is always None — full decode would
/// require reference-frame management beyond this parser's scope. See
/// `docs/project/deferred-features.md`.
pub fn parse_frame_header_light(
    payload: &[u8],
    seq: &Av1SequenceHeader,
) -> Result<Av1FrameHeaderLight, CodecParseError> {
    if seq.reduced_still_picture_header {
        // Per AV1 §5.9.1: with reduced_still_picture_header set, the
        // frame is implicitly a KEY_FRAME with show_frame=1; no fields
        // are read from the bitstream.
        return Ok(Av1FrameHeaderLight {
            frame_type: 0,
            show_frame: true,
            show_existing_frame: false,
            frame_size: None,
            raw: payload.to_vec(),
        });
    }

    let mut br = Av1BitReader::new(payload);
    let show_existing_frame = br.f(1)? != 0;
    if show_existing_frame {
        // Spec §5.9.2: frame_to_show_map_idx u(3) follows; we don't
        // surface it. Return early with show_frame=true (implicit).
        return Ok(Av1FrameHeaderLight {
            frame_type: 0, // not encoded in this path; sentinel value
            show_frame: true,
            show_existing_frame: true,
            frame_size: None,
            raw: payload.to_vec(),
        });
    }
    let frame_type = br.f(2)? as u8;
    let show_frame = br.f(1)? != 0;
    // (Further fields exist beyond this point; current scope doesn't surface them.)

    Ok(Av1FrameHeaderLight {
        frame_type,
        show_frame,
        show_existing_frame: false,
        frame_size: None,
        raw: payload.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ChromaFormat;
    use crate::test_prelude::*;

    fn dummy_seq() -> Av1SequenceHeader {
        Av1SequenceHeader {
            profile: 0,
            level: 0,
            tier: 0,
            max_frame_width: 320,
            max_frame_height: 240,
            bit_depth: 8,
            monochrome: false,
            chroma_format: ChromaFormat::Yuv420,
            still_picture: false,
            reduced_still_picture_header: false,
            color_info: None,
            frame_rate: None,
            raw: vec![],
        }
    }

    /// Build a minimal Frame Header OBU body for a keyframe (KEY_FRAME=0).
    /// Bitstream: show_existing_frame(1)=0, frame_type(2)=0, show_frame(1)=1.
    /// Four bits: 0,0,0,1 → high nibble of byte = 0b0001_0000 = 0x10.
    fn keyframe_header_body() -> Vec<u8> {
        // show_existing_frame=0, frame_type=KEY_FRAME(0), show_frame=1
        // 0 | 00 | 1 → 4 bits: 0001 → high nibble 0x10
        vec![0b0001_0000]
    }

    #[test]
    fn parse_frame_header_keyframe() {
        let payload = keyframe_header_body();
        let fh = parse_frame_header_light(&payload, &dummy_seq()).expect("should parse");
        assert_eq!(fh.frame_type, 0);
        assert!(fh.show_frame);
        assert!(!fh.show_existing_frame);
        assert_eq!(fh.frame_size, None);
    }

    #[test]
    fn parse_frame_header_show_existing_frame() {
        // show_existing_frame=1, frame_to_show_map_idx u(3) = 0
        // 1 | 000 → 4 bits: 1000 → 0x80
        let payload = vec![0x80];
        let fh = parse_frame_header_light(&payload, &dummy_seq()).expect("should parse");
        assert!(fh.show_existing_frame);
        assert!(fh.show_frame); // implied true for show_existing_frame
        assert_eq!(fh.frame_size, None);
    }

    #[test]
    fn parse_frame_header_reduced_still_picture_header_implies_keyframe() {
        let mut seq = dummy_seq();
        seq.reduced_still_picture_header = true;
        // For reduced still picture header, no bits are read from the
        // payload; the parser returns KEY_FRAME + show_frame=true
        // implicit per spec §5.9.1.
        let fh = parse_frame_header_light(&[], &seq).expect("should parse");
        assert_eq!(fh.frame_type, 0);
        assert!(fh.show_frame);
        assert!(!fh.show_existing_frame);
    }
}
