//! Test-only RFC 6184 H.264 RTP payloader, shared by the `h264` domain
//! binary (via `h264/common.rs`) and the RTSP publisher-role tests.
//!
//! `packetize` produces standards-correct RTP packets so integration tests
//! can exercise H.264 receive paths without a real encoder.

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
pub fn build_rtp_packet(
    seq: u16,
    ts: u32,
    ssrc: u32,
    pt: u8,
    marker: bool,
    payload: &[u8],
) -> Vec<u8> {
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
