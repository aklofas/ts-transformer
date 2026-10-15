//! RFC 6184 H.264 packetization, packetization mode 1 (non-interleaved):
//! single NAL unit packets (§5.6) and FU-A fragmentation units (§5.8). No
//! STAP-A: parameter sets travel as single NAL units, which every
//! receiver accepts. The inverse is [`super::H264Depacketizer`].

use crate::packet::Chunk;

/// The smallest `max_payload` [`packetize`] accepts: the 2-byte FU-A header
/// plus one byte of NAL data.
pub const MIN_MAX_PAYLOAD: usize = 3;

/// Packetize one access unit.
///
/// `nals` are its NAL units in decoding order, each starting with its
/// 1-byte header and without a start code — what
/// `tst_core::codec::annexb::nals` yields. A NAL unit of at most
/// `max_payload` bytes travels whole; a larger one becomes FU-A fragments
/// of at most `max_payload` bytes each. Empty NAL units are skipped. The
/// last chunk carries the marker bit; an access unit with no non-empty NAL
/// unit yields nothing.
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
        // FU indicator: F and NRI of the NAL header, type 28.
        let indicator = (nal[0] & 0xE0) | 28;
        let nal_type = nal[0] & 0x1F;
        let body = &nal[1..];
        let fragment = max_payload - 2;
        let count = body.len().div_ceil(fragment);
        for (i, part) in body.chunks(fragment).enumerate() {
            let mut fu_header = nal_type;
            if i == 0 {
                fu_header |= 0x80; // S
            }
            if i + 1 == count {
                fu_header |= 0x40; // E
            }
            let mut payload = Vec::with_capacity(2 + part.len());
            payload.push(indicator);
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

    #[test]
    fn nal_units_that_fit_travel_whole_with_the_marker_on_the_last() {
        let out = packetize([&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5, 6]], 1200);
        assert_eq!(out.len(), 3);
        assert_eq!(
            (out[0].payload.as_slice(), out[0].marker),
            (&[0x67, 1, 2][..], false)
        );
        assert_eq!(
            (out[2].payload.as_slice(), out[2].marker),
            (&[0x65, 4, 5, 6][..], true)
        );
    }

    /// RFC 6184 §5.8: FU indicator = F|NRI of the NAL header + type 28;
    /// FU header = S|E|R|type; the NAL header byte itself is not sent.
    #[test]
    fn a_large_nal_unit_becomes_fu_a_fragments() {
        let mut nal = vec![0x65u8];
        nal.extend((0..2500u32).map(|i| (i % 251) as u8));
        let out = packetize([nal.as_slice()], 1200);
        assert_eq!(out.len(), 3); // 2500 body bytes in 1198-byte fragments: 1198, 1198, 104
        assert_eq!(&out[0].payload[..2], &[0x7C, 0x85]); // NRI 3, type 28 | S + type 5
        assert_eq!(&out[1].payload[..2], &[0x7C, 0x05]);
        assert_eq!(&out[2].payload[..2], &[0x7C, 0x45]); // E + type 5
        assert_eq!((out[0].payload.len(), out[2].payload.len()), (1200, 106));
        assert_eq!(
            out.iter().map(|c| c.marker).collect::<Vec<_>>(),
            vec![false, false, true]
        );
        let body: Vec<u8> = out
            .iter()
            .flat_map(|c| c.payload[2..].iter().copied())
            .collect();
        assert_eq!(body, nal[1..]);
    }

    #[test]
    fn exactly_max_payload_is_not_fragmented_and_empty_input_yields_nothing() {
        let nal = vec![0x41u8; 1200];
        assert_eq!(packetize([nal.as_slice()], 1200).len(), 1);
        assert!(packetize(std::iter::empty::<&[u8]>(), 1200).is_empty());
        assert!(packetize([&[][..]], 1200).is_empty());
    }

    use crate::h264::{H264Depacketizer, H264DepayConfig, ParameterSetInjection};
    use crate::packet::RtpHeader;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn depacketize_after_packetize_is_the_access_unit(
            nals in proptest::collection::vec((1u8..=23, proptest::collection::vec(any::<u8>(), 0..3000)), 1..8),
            max_payload in MIN_MAX_PAYLOAD..1500usize,
            ssrc in any::<u32>(),
        ) {
            let nals: Vec<Vec<u8>> = nals.into_iter().map(|(t, body)| { let mut n = vec![0x60 | t]; n.extend(body); n }).collect();
            let chunks = packetize(nals.iter().map(Vec::as_slice), max_payload);
            let mut d = H264Depacketizer::new(H264DepayConfig {
                parameter_set_injection: ParameterSetInjection::None,
                ..H264DepayConfig::default()
            });
            for (i, c) in chunks.iter().enumerate() {
                let mut h = RtpHeader::new(i as u16, 90_000, ssrc);
                h.marker = c.marker;
                d.feed(&h, &c.payload);
            }
            let au = d.next_au().expect("one access unit");
            let annexb: Vec<u8> = nals.iter().flat_map(|n| [0u8, 0, 0, 1].into_iter().chain(n.iter().copied())).collect();
            prop_assert_eq!(&au.annexb, &annexb);
            prop_assert_eq!(au.key_frame, nals.iter().any(|n| n[0] & 0x1F == 5));
            prop_assert!(d.next_au().is_none());
        }
    }
}
