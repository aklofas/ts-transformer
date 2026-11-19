//! RFC 6597 packetization of one KLV unit. The payload format has no
//! header of its own: the unit is cut into consecutive chunks and the
//! marker bit is set on the chunk that carries its end.

use crate::packet::Chunk;

/// The smallest `max_payload` [`packetize`] accepts: one byte of the unit.
pub const MIN_MAX_PAYLOAD: usize = 1;

/// Packetize one KLV unit.
///
/// The unit is split into chunks of at most `max_payload` bytes. Only the
/// last chunk carries the marker bit; every chunk of the unit shares one
/// RTP timestamp, which is the caller's to set. An empty unit yields no
/// chunk.
///
/// # Panics
///
/// If `max_payload` is below [`MIN_MAX_PAYLOAD`].
///
/// # Example
///
/// ```
/// use tst_rtp::klv::packetize;
///
/// let chunks = packetize(&[1, 2, 3, 4, 5], 2);
/// assert_eq!(chunks.len(), 3);
/// assert_eq!(chunks[2].payload, [5]);
/// assert!(chunks[2].marker && !chunks[0].marker);
/// ```
pub fn packetize(unit: &[u8], max_payload: usize) -> Vec<Chunk> {
    assert!(
        max_payload >= MIN_MAX_PAYLOAD,
        "max_payload {max_payload} is below {MIN_MAX_PAYLOAD}"
    );
    let mut out: Vec<Chunk> = unit
        .chunks(max_payload)
        .map(|part| Chunk {
            payload: part.to_vec(),
            marker: false,
        })
        .collect();
    if let Some(last) = out.last_mut() {
        last.marker = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 100;

    fn unit(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn a_unit_that_fits_is_one_marked_chunk() {
        let u = unit(MAX);
        let c = packetize(&u, MAX);
        assert_eq!(c.len(), 1);
        assert!(c[0].marker);
        assert_eq!(c[0].payload, u);
    }

    #[test]
    fn a_unit_one_byte_over_is_two_chunks_with_the_marker_on_the_last() {
        let c = packetize(&unit(MAX + 1), MAX);
        assert_eq!(c.len(), 2);
        assert!(!c[0].marker && c[1].marker);
        assert_eq!(c[1].payload.len(), 1);
    }

    #[test]
    fn chunks_concatenate_to_the_unit() {
        for n in [1, MAX, MAX + 1, 3 * MAX, 65_527] {
            let u = unit(n);
            let c = packetize(&u, MAX);
            assert!(c.iter().all(|c| c.payload.len() <= MAX));
            assert_eq!(c.iter().filter(|c| c.marker).count(), 1);
            let joined: Vec<u8> = c.iter().flat_map(|c| c.payload.clone()).collect();
            assert_eq!(joined, u, "size {n}");
        }
    }

    #[test]
    fn an_empty_unit_gives_no_chunk() {
        assert!(packetize(&[], MAX).is_empty());
    }

    #[test]
    #[should_panic(expected = "below")]
    fn a_zero_max_payload_panics() {
        packetize(&[1], 0);
    }

    #[cfg(feature = "rtsp-server")]
    mod round_trip {
        use super::*;
        use crate::packet::RtpHeader;
        use crate::rtsp::server::publish::klv_depacketizer::KlvDepacketizer;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]
            #[test]
            fn random_units_round_trip_through_the_servers_depacketizer(
                size in 1usize..=65_527,
                max in 1usize..=2_000,
                seed in any::<u8>(),
                ts in any::<u32>(),
                first_seq in any::<u16>(),
            ) {
                let u: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_add(seed)).collect();
                let mut d = KlvDepacketizer::new();
                for (i, c) in packetize(&u, max).into_iter().enumerate() {
                    let mut h = RtpHeader::new(first_seq.wrapping_add(i as u16), ts, 0x1234);
                    h.marker = c.marker;
                    d.feed(&h, &c.payload);
                }
                let got = d.next_unit().expect("one unit");
                prop_assert_eq!(got.bytes, u);
                prop_assert!(d.next_unit().is_none());
            }
        }
    }
}
