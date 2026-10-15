//! RFC 7798 H.265 packetization without DONL (`sprop-max-don-diff` = 0):
//! single NAL unit packets (§4.4.1) and fragmentation units (§4.4.3). No
//! aggregation packets. The inverse is [`super::H265Depacketizer`].

use crate::packet::Chunk;

/// The smallest `max_payload` [`packetize`] accepts: the 2-byte payload
/// header, the FU header and one byte of NAL data.
pub const MIN_MAX_PAYLOAD: usize = 4;

/// Packetize one access unit: as `h264::packetizer::packetize`, with each
/// NAL unit starting with its 2-byte H.265 header and fragments carrying a
/// 3-byte prefix (payload header with Type 49, then `S|E|FuType`).
///
/// # Panics
///
/// If `max_payload` is below [`MIN_MAX_PAYLOAD`].
pub fn packetize<'a, I>(nals: I, max_payload: usize) -> Vec<Chunk>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    assert!(
        max_payload >= MIN_MAX_PAYLOAD,
        "max_payload {max_payload} is below {MIN_MAX_PAYLOAD}"
    );
    let mut out = Vec::new();
    for nal in nals.into_iter().filter(|n| !n.is_empty()) {
        if nal.len() <= max_payload {
            out.push(Chunk {
                payload: nal.to_vec(),
                marker: false,
            });
            continue;
        }
        // nal.len() > max_payload >= 4, so both header bytes exist.
        // PayloadHdr: F and the LayerId high bit kept, Type = 49; byte 1
        // (LayerId low bits, TID) unchanged.
        let payload_hdr = [(nal[0] & 0x81) | (49 << 1), nal[1]];
        let fu_type = (nal[0] >> 1) & 0x3F;
        let body = &nal[2..];
        let fragment = max_payload - 3;
        let count = body.len().div_ceil(fragment);
        for (i, part) in body.chunks(fragment).enumerate() {
            let mut fu_header = fu_type;
            if i == 0 {
                fu_header |= 0x80; // S
            }
            if i + 1 == count {
                fu_header |= 0x40; // E
            }
            let mut payload = Vec::with_capacity(3 + part.len());
            payload.extend_from_slice(&payload_hdr);
            payload.push(fu_header);
            payload.extend_from_slice(part);
            out.push(Chunk {
                payload,
                marker: false,
            });
        }
    }
    if let Some(last) = out.last_mut() {
        last.marker = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7798 §4.4.3: PayloadHdr = the NAL header with Type 49; FU header
    /// = S|E|FuType; data starts after the 2-byte NAL header.
    #[test]
    fn a_large_nal_unit_becomes_fu_fragments() {
        let mut nal = vec![0x26u8, 0x01]; // IDR_W_RADL (19), layer 0, tid 1
        nal.extend((0..2500u32).map(|i| (i % 251) as u8));
        let out = packetize([nal.as_slice()], 1200);
        assert_eq!(out.len(), 3); // 1197, 1197, 106
        assert_eq!(&out[0].payload[..3], &[0x62, 0x01, 0x80 | 19]);
        assert_eq!(&out[1].payload[..3], &[0x62, 0x01, 19]);
        assert_eq!(&out[2].payload[..3], &[0x62, 0x01, 0x40 | 19]);
        assert_eq!((out[0].payload.len(), out[2].payload.len()), (1200, 109));
        assert!(out[2].marker && !out[0].marker);
        let body: Vec<u8> = out
            .iter()
            .flat_map(|c| c.payload[3..].iter().copied())
            .collect();
        assert_eq!(body, nal[2..]);
    }

    /// The PayloadHdr keeps the NAL header's F bit and LayerId high bit and
    /// the second byte (LayerId low bits, TID) unchanged: `0x27 0x03` (type
    /// 19, LayerId high bit set, TID 3) gives `0x63 0x03`.
    #[test]
    fn the_payload_header_keeps_layer_id_and_tid() {
        let mut nal = vec![0x27u8, 0x03];
        nal.extend(std::iter::repeat_n(0xCD, 2500));
        let out = packetize([nal.as_slice()], 1200);
        assert_eq!(&out[0].payload[..3], &[0x63, 0x03, 0x80 | 19]);
        assert_eq!(&out[2].payload[..3], &[0x63, 0x03, 0x40 | 19]);
    }

    #[test]
    fn small_nal_units_travel_whole() {
        let out = packetize([&[0x40, 0x01, 0xAA][..], &[0x26, 0x01, 0xBB]], 1200);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].payload, vec![0x40, 0x01, 0xAA]);
        assert!(out[1].marker);
    }
}
