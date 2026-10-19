//! Descriptor parsers for `mpegts::demux` consumers and PSI cascade.
//!
//! Stateless. Each parser takes the descriptor payload (length-field
//! stripped) and returns typed entries or a [`DescriptorParseError`].

use alloc::vec::Vec;

/// Raw, unparsed descriptor. The walker (`walk_descriptors`) and the
/// typed extractors (`has_klva_registration`, `extract_metadata_link`)
/// interpret these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDescriptor {
    pub tag: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitlingDescriptorEntry {
    pub language: [u8; 3],
    pub subtitling_type: u8,
    pub composition_page_id: u16,
    pub ancillary_page_id: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeletextDescriptorEntry {
    pub language: [u8; 3],
    pub teletext_type: u8,
    pub magazine_number: u8,
    pub page_number: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DescriptorParseError {
    Truncated,
    EmptyInput,
}

const SUBTITLING_ENTRY_BYTES: usize = 8;
const TELETEXT_ENTRY_BYTES: usize = 5;

pub fn parse_subtitling_descriptor(
    payload: &[u8],
) -> Result<Vec<SubtitlingDescriptorEntry>, DescriptorParseError> {
    if payload.is_empty() {
        return Err(DescriptorParseError::EmptyInput);
    }
    if !payload.len().is_multiple_of(SUBTITLING_ENTRY_BYTES) {
        return Err(DescriptorParseError::Truncated);
    }
    let mut out = Vec::with_capacity(payload.len() / SUBTITLING_ENTRY_BYTES);
    for chunk in payload.chunks_exact(SUBTITLING_ENTRY_BYTES) {
        out.push(SubtitlingDescriptorEntry {
            language: [chunk[0], chunk[1], chunk[2]],
            subtitling_type: chunk[3],
            composition_page_id: u16::from_be_bytes([chunk[4], chunk[5]]),
            ancillary_page_id: u16::from_be_bytes([chunk[6], chunk[7]]),
        });
    }
    Ok(out)
}

pub fn parse_teletext_descriptor(
    payload: &[u8],
) -> Result<Vec<TeletextDescriptorEntry>, DescriptorParseError> {
    if payload.is_empty() {
        return Err(DescriptorParseError::EmptyInput);
    }
    if !payload.len().is_multiple_of(TELETEXT_ENTRY_BYTES) {
        return Err(DescriptorParseError::Truncated);
    }
    let mut out = Vec::with_capacity(payload.len() / TELETEXT_ENTRY_BYTES);
    for chunk in payload.chunks_exact(TELETEXT_ENTRY_BYTES) {
        let packed = chunk[3];
        out.push(TeletextDescriptorEntry {
            language: [chunk[0], chunk[1], chunk[2]],
            teletext_type: (packed >> 3) & 0x1F,
            magazine_number: packed & 0x07,
            page_number: chunk[4],
        });
    }
    Ok(out)
}

/// Returns true if any `RawDescriptor` in `descriptors` is a
/// registration_descriptor (tag 0x05) whose 4-byte format_identifier
/// matches `target`. Used by the demux PSI cascade and by callers
/// decoding `StreamInfo::raw_descriptors`.
pub fn find_format_identifier(descriptors: &[RawDescriptor], target: &[u8; 4]) -> bool {
    for d in descriptors {
        if d.tag == 0x05 && d.data.len() >= 4 && &d.data[..4] == target {
            return true;
        }
    }
    false
}

/// Returns true if any `RawDescriptor` in `descriptors` has the given
/// tag byte. Used by the demux PSI cascade for subtitling (0x59) /
/// teletext (0x56 + 0x46) classification.
pub fn find_descriptor_tag(descriptors: &[RawDescriptor], tag: u8) -> bool {
    descriptors.iter().any(|d| d.tag == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subtitling_descriptor_single_entry() {
        let payload = [b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x02];
        let entries = parse_subtitling_descriptor(&payload).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].language, *b"eng");
        assert_eq!(entries[0].subtitling_type, 0x10);
        assert_eq!(entries[0].composition_page_id, 1);
        assert_eq!(entries[0].ancillary_page_id, 2);
    }

    #[test]
    fn parse_subtitling_descriptor_truncated_returns_error() {
        let payload = [b'e', b'n', b'g', 0x10, 0x00, 0x01];
        assert!(matches!(
            parse_subtitling_descriptor(&payload),
            Err(DescriptorParseError::Truncated)
        ));
    }

    #[test]
    fn parse_teletext_descriptor_single_entry() {
        let payload = [b'e', b'n', b'g', (0x02 << 3) | 1, 0x88];
        let entries = parse_teletext_descriptor(&payload).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].language, *b"eng");
        assert_eq!(entries[0].teletext_type, 0x02);
        assert_eq!(entries[0].magazine_number, 1);
        assert_eq!(entries[0].page_number, 0x88);
    }

    #[test]
    fn parse_subtitling_descriptor_multi_entry() {
        let mut payload = vec![];
        payload.extend_from_slice(&[b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x02]);
        payload.extend_from_slice(&[b's', b'p', b'a', 0x10, 0x00, 0x03, 0x00, 0x04]);
        let entries = parse_subtitling_descriptor(&payload).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].language, *b"spa");
        assert_eq!(entries[1].composition_page_id, 3);
    }
}
