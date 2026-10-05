//! Test-only RFC 6184 H.264 RTP payloader.
//!
//! Used as the generative partner to the hand-built spec-byte unit tests.
//! `packetize` produces standards-correct RTP packets so the integration tests
//! can exercise the full `H264Receiver` path without a real encoder.

/// Build the Annex B framing we expect for a single AU given its raw NALUs.
///
/// Each NALU is preceded by a `[0,0,0,1]` start code, in order.
pub fn expected_annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nalu in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Packetize a sequence of H.264 Access Units into RTP packets.
///
/// # Parameters
/// - `aus` — each element is `(rtp_timestamp, vec_of_nalus)`.
/// - `mtu` — maximum payload budget **excluding** the 12-byte RTP header.
///   NALUs that fit within this budget are carried as single-NALU packets
///   (RFC 6184 §5.6). NALUs that exceed it are fragmented into FU-A packets
///   (RFC 6184 §5.8).
/// - `seq0` — initial RTP sequence number; incremented per packet.
/// - `ssrc` — SSRC for all emitted packets.
/// - `pt` — RTP payload type (7 bits).
///
/// # Packet format
///
/// Header (12 bytes, RFC 3550 §5.1):
/// - V=2, P=0, X=0, CC=0
/// - M=1 on the **last packet of each AU** (RFC 6184 §5.1 end-of-AU marker)
/// - PT, seq (big-endian u16), timestamp (big-endian u32), SSRC (big-endian u32)
///
/// FU-A indicator byte: `(nri << 5) | 28`
/// FU-A header byte:   `(S << 7) | (E << 6) | (nalu_type & 0x1F)`
///
/// where `nri = (nalu[0] >> 5) & 0x03`.
pub fn packetize(
    aus: &[(u32, Vec<Vec<u8>>)],
    mtu: usize,
    seq0: u16,
    ssrc: u32,
    pt: u8,
) -> Vec<Vec<u8>> {
    debug_assert!(
        mtu >= 3,
        "payloader needs ≥3 bytes for FU header + 1 fragment byte"
    );
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut seq = seq0;

    for (ts, nalus) in aus {
        // Collect this AU's packets in a temp buffer so we can set M=1 on
        // the last one.
        let au_start = packets.len();

        for nalu in nalus {
            if nalu.is_empty() {
                continue;
            }
            if nalu.len() <= mtu {
                // ── Single-NALU packet (RFC 6184 §5.6) ──────────────────────
                let pkt = build_rtp_packet(seq, *ts, ssrc, pt, false, nalu);
                packets.push(pkt);
                seq = seq.wrapping_add(1);
            } else {
                // ── FU-A fragmentation (RFC 6184 §5.8) ───────────────────────
                // Payload budget: mtu minus the 2 FU-A header bytes.
                let frag_size = mtu.saturating_sub(2);
                if frag_size == 0 {
                    // MTU too small to carry even a single fragment byte — skip.
                    continue;
                }

                let nalu_hdr = nalu[0];
                let nri = (nalu_hdr >> 5) & 0x03;
                let nalu_type = nalu_hdr & 0x1F;
                // FU indicator: NRI + type 28
                let fu_ind = (nri << 5) | 28;
                let body = &nalu[1..]; // everything after the NALU header byte

                let chunks: Vec<&[u8]> = body.chunks(frag_size).collect();
                let n_chunks = chunks.len();
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let s = i == 0;
                    let e = i == n_chunks - 1;
                    let fu_hdr = (u8::from(s) << 7) | (u8::from(e) << 6) | (nalu_type & 0x1F);
                    let mut payload = Vec::with_capacity(2 + chunk.len());
                    payload.push(fu_ind);
                    payload.push(fu_hdr);
                    payload.extend_from_slice(chunk);
                    let pkt = build_rtp_packet(seq, *ts, ssrc, pt, false, &payload);
                    packets.push(pkt);
                    seq = seq.wrapping_add(1);
                }
            }
        }

        // Set M=1 on the last packet of this AU.
        if let Some(last) = packets[au_start..].last_mut() {
            // Byte 1 of RTP: M(1) | PT(7). Set M=1.
            last[1] |= 0x80;
        }
    }

    packets
}

/// Build one 12-byte-header RTP packet.
fn build_rtp_packet(seq: u16, ts: u32, ssrc: u32, pt: u8, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0u8; 12 + payload.len()];
    // Byte 0: V=2, P=0, X=0, CC=0
    pkt[0] = 0x80;
    // Byte 1: M | PT
    pkt[1] = (u8::from(marker) << 7) | (pt & 0x7F);
    // Bytes 2..4: seq
    pkt[2..4].copy_from_slice(&seq.to_be_bytes());
    // Bytes 4..8: timestamp
    pkt[4..8].copy_from_slice(&ts.to_be_bytes());
    // Bytes 8..12: ssrc
    pkt[8..12].copy_from_slice(&ssrc.to_be_bytes());
    // Payload
    pkt[12..].copy_from_slice(payload);
    pkt
}

/// A minimal deterministic LCG PRNG for the loss-soak test.
///
/// Uses the parameters of Numerical Recipes (m=2^32, a=1664525, c=1013904223)
/// which produce a good distribution for 32-bit uniform output.
pub struct Lcg(u32);

impl Lcg {
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }

    /// Advance and return the next pseudo-random u32.
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    /// Return `true` with probability `p_drop` (0.0..=1.0).
    pub fn should_drop(&mut self, p_drop: f64) -> bool {
        // Scale: if next_u32() / 2^32 < p_drop → drop.
        let threshold = (p_drop * (u32::MAX as f64)) as u32;
        self.next_u32() < threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-NALU AU: verify that the emitted packet is a valid 12-byte header
    /// followed by the raw NALU and that M=1.
    #[test]
    fn single_nalu_au_fits_mtu() {
        let nalu = vec![0x65u8, 0xAA, 0xBB]; // IDR slice
        let aus = vec![(90_000u32, vec![nalu.clone()])];
        let pkts = packetize(&aus, 1400, 1, 0xDEAD, 96);
        assert_eq!(pkts.len(), 1);
        // M=1: byte 1 has bit 7 set.
        assert_eq!(pkts[0][1], 0x80 | 96);
        // Seq=1
        assert_eq!(u16::from_be_bytes([pkts[0][2], pkts[0][3]]), 1);
        // TS=90000
        assert_eq!(
            u32::from_be_bytes([pkts[0][4], pkts[0][5], pkts[0][6], pkts[0][7]]),
            90_000
        );
        // Payload is exactly the NALU.
        assert_eq!(&pkts[0][12..], &nalu[..]);
    }

    /// FU-A split: NALU larger than MTU becomes multiple packets.
    #[test]
    fn nalu_over_mtu_splits_into_fu_a() {
        // 10-byte NALU, MTU=5 (payload budget 5, frag budget 5-2=3 bytes per chunk).
        let nalu = vec![0x41u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let aus = vec![(1000u32, vec![nalu.clone()])];
        let pkts = packetize(&aus, 5, 1, 0xBEEF, 96);
        // body = nalu[1..] = 9 bytes, frag_size = 3 → ceil(9/3) = 3 packets.
        assert_eq!(pkts.len(), 3);
        // Only the last packet has M=1.
        assert_eq!(pkts[0][1] & 0x80, 0, "first FU packet M must be 0");
        assert_eq!(pkts[1][1] & 0x80, 0, "middle FU packet M must be 0");
        assert_eq!(pkts[2][1] & 0x80, 0x80, "last FU packet M must be 1");
        // S=1 on first, E=1 on last.
        let fu_ind = pkts[0][12];
        let fh0 = pkts[0][13];
        let fh2 = pkts[2][13];
        assert_eq!(fu_ind & 0x1F, 28); // type 28 = FU-A
        assert_eq!(fh0 & 0x80, 0x80, "S bit first packet");
        assert_eq!(fh0 & 0x40, 0, "E bit first packet must be 0");
        assert_eq!(fh2 & 0x80, 0, "S bit last packet must be 0");
        assert_eq!(fh2 & 0x40, 0x40, "E bit last packet");
        // NRI preserved: nalu[0] = 0x41 → NRI = (0x41 >> 5) & 0x03 = 2.
        assert_eq!((fu_ind >> 5) & 0x03, (nalu[0] >> 5) & 0x03);
    }

    /// LCG produces distinct values and should_drop proportion is approximately correct.
    #[test]
    fn lcg_drop_distribution() {
        let mut rng = Lcg::new(42);
        let n = 100_000;
        let p = 0.2;
        let dropped: u32 = (0..n).map(|_| u32::from(rng.should_drop(p))).sum();
        // Allow ±3% relative tolerance.
        let expected = (n as f64 * p) as u32;
        let delta = (dropped as i64 - expected as i64).unsigned_abs() as u32;
        assert!(delta < n / 20, "LCG drop rate {dropped}/{n} far from {p}");
    }
}
