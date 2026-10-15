//! RTP packets for the H.264 receive-path tests: the library's RFC 6184
//! packetizer behind a hand-written RTP header (RFC 3550 §5.1), and the
//! Annex-B buffer a depacketizer must produce from them.

/// The Annex-B framing expected for one access unit: each NAL unit behind
/// a `[0, 0, 0, 1]` start code, in order.
pub fn expected_annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nalu in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Packetize access units into whole RTP packets.
///
/// `aus` are `(rtp_timestamp, nal_units)`; `mtu` is the payload budget
/// excluding the 12-byte RTP header; sequence numbers start at `seq0`.
pub fn packetize(
    aus: &[(u32, Vec<Vec<u8>>)],
    mtu: usize,
    seq0: u16,
    ssrc: u32,
    pt: u8,
) -> Vec<Vec<u8>> {
    let mut seq = seq0;
    let mut out = Vec::new();
    for (ts, nalus) in aus {
        for c in tst_rtp::h264::packetizer::packetize(nalus.iter().map(Vec::as_slice), mtu) {
            out.push(build_rtp_packet(seq, *ts, ssrc, pt, c.marker, &c.payload));
            seq = seq.wrapping_add(1);
        }
    }
    out
}

/// One RTP packet: V=2, P=0, X=0, CC=0, then marker|PT, sequence,
/// timestamp and SSRC (big-endian), then the payload.
pub fn build_rtp_packet(
    seq: u16,
    ts: u32,
    ssrc: u32,
    pt: u8,
    marker: bool,
    payload: &[u8],
) -> Vec<u8> {
    let mut pkt = vec![0u8; 12 + payload.len()];
    pkt[0] = 0x80;
    pkt[1] = (u8::from(marker) << 7) | (pt & 0x7F);
    pkt[2..4].copy_from_slice(&seq.to_be_bytes());
    pkt[4..8].copy_from_slice(&ts.to_be_bytes());
    pkt[8..12].copy_from_slice(&ssrc.to_be_bytes());
    pkt[12..].copy_from_slice(payload);
    pkt
}
