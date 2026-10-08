//! Per-shape adapters: RTP in, TS frames out to the mount's two sinks.
//!
//! Each wire shape a publisher may announce (§1 classification table in
//! [`super::PublishShape`]) gets its own [`PublishAdapter`] impl, fed
//! RTP/RTCP packets by the session's track dispatch (interleaved `$`
//! frames and UDP ingest). This module ships the one shape in scope
//! today: MP2T passthrough.

use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};

use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
use crate::rtsp::server::mount::RTP_PAYLOAD_SIZE;

use super::mount::PublishMountState;

/// Adapts incoming RTP/RTCP packets on a published mount into demuxed
/// events.
///
/// Implementations never fail outward — a malformed or unexpected packet
/// is dropped and counted (via the mount's `malformed_packets` stat),
/// mirroring how [`crate::transport::RtpRecvTransport::recv_bytes`]
/// treats a bad payload on the receive side.
///
/// Constructed by `handle_announce`; `on_rtp`/`on_rtcp` are driven from
/// live RTSP traffic by the session's track dispatch (interleaved `$`
/// frames and UDP ingest).
pub(crate) trait PublishAdapter: Send {
    /// One RTP packet from `track` (index into the announce's track table).
    /// Never fails; drops count.
    fn on_rtp(&mut self, track: usize, packet: &[u8]);
    /// One RTCP packet from `track`'s RTCP channel/socket.
    fn on_rtcp(&mut self, track: usize, packet: &[u8]);
    /// Publisher ended: push out whatever is pending. Called by
    /// `PublishSession::end`.
    fn flush(&mut self);
}

/// RFC 2250 passthrough: the single announced track carries whole TS
/// packets as the RTP payload.
///
/// A publisher may bundle up to 348 TS packets per RTP packet, but the
/// mount's two sinks are bounded in frames, and a UDP PLAY reader's
/// datagram should stay unfragmented. So the TS payload is re-chunked
/// into bundles of at most [`RTP_PAYLOAD_SIZE`] bytes (7 TS packets), the
/// framing a muxer-backed mount emits:
///
/// - PLAY readers get each bundle as a zero-copy slice of the packet.
/// - The application transport gets each bundle behind a synthesized
///   12-byte RTP header: V=2, no padding, extension or CSRCs, marker 0,
///   PT 33 (whatever PT the publisher's SDP declared, so
///   [`crate::transport::RtpRecvTransport`]'s PT pin holds), the source
///   packet's timestamp and SSRC, and a sequence number from this
///   adapter's own counter (one source packet may become several
///   bundles, so the source sequence number cannot be reused).
///
/// The only state across calls is that counter: there is nothing to
/// reassemble.
pub(crate) struct Mp2tAdapter {
    mount: Arc<PublishMountState>,
    expected_pt: u8,
    next_seq: u16,
}

impl Mp2tAdapter {
    pub(crate) fn new(mount: Arc<PublishMountState>, expected_pt: u8) -> Self {
        Self {
            mount,
            expected_pt,
            next_seq: 0,
        }
    }
}

impl PublishAdapter for Mp2tAdapter {
    fn on_rtp(&mut self, _track: usize, packet: &[u8]) {
        self.mount.tick(|s| {
            s.rtp_packets_received += 1;
            s.bytes_received += packet.len() as u64;
        });
        let parsed = match RtpHeader::decode(packet) {
            Ok(p) if p.header.payload_type == self.expected_pt => p,
            Ok(p) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    pt = p.header.payload_type,
                    expected = self.expected_pt,
                    "MP2T publisher packet with unexpected PT; dropped"
                );
                self.mount.tick(|s| s.malformed_packets += 1);
                return;
            }
            Err(e) => {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    error = ?e,
                    "unparseable RTP from publisher; dropped"
                );
                self.mount.tick(|s| s.malformed_packets += 1);
                return;
            }
        };
        let (start, end) = (parsed.payload_offset, parsed.payload_end);
        if !crate::transport::is_valid_mp2t_payload(&packet[start..end]) {
            self.mount.tick(|s| s.malformed_packets += 1);
            return;
        }
        // One copy of the packet; readers get slices of it.
        let source = Bytes::copy_from_slice(packet);
        let mut header = RtpHeader::new(0, parsed.header.timestamp, parsed.header.ssrc);
        header.payload_type = RTP_PT_MP2T;
        let mut at = start;
        while at < end {
            let chunk_end = (at + RTP_PAYLOAD_SIZE).min(end);
            let ts = source.slice(at..chunk_end);
            header.seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let mut app = BytesMut::with_capacity(RTP_HEADER_LEN + ts.len());
            app.put_bytes(0, RTP_HEADER_LEN);
            header.encode_into(&mut app[..RTP_HEADER_LEN]);
            app.put_slice(&ts);
            self.mount.emit(ts, app.freeze());
            at = chunk_end;
        }
    }

    fn on_rtcp(&mut self, _track: usize, _packet: &[u8]) {
        // RFC 2250 carries no KLV/AU timing that needs an RTCP anchor;
        // the MP2T shape has no clock alignment to feed.
    }

    fn flush(&mut self) {
        // Nothing buffered to push out.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::server::publish::mount::PublishMountState;

    fn mount() -> Arc<PublishMountState> {
        PublishMountState::new("/p", 8)
    }
    fn ts(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(0x47);
            v.push(i as u8);
            v.extend(std::iter::repeat_n(0u8, 186));
        }
        v
    }
    fn rtp(pt: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x80, pt, 0, 7, 0, 0, 0x10, 0, 0xDE, 0xAD, 0xBE, 0xEF];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn valid_packet_feeds_readers_with_payload_and_app_with_a_pt33_packet() {
        let m = mount();
        let mut reader = m.fanout.subscribe();
        let mut t = crate::rtsp::server::publish::mount::PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        let mut a = Mp2tAdapter::new(m.clone(), 33);
        let pkt = rtp(33, &ts(3));
        a.on_rtp(0, &pkt);
        assert_eq!(reader.try_recv().unwrap().as_ref(), &pkt[12..]);
        let mut buf = vec![0u8; 2048];
        let n = tst_core::transport::RecvTransport::recv_bytes(&mut t, &mut buf).unwrap();
        assert_eq!(&buf[..n], &pkt[12..]);
        let s = m.stats_snapshot();
        assert_eq!(
            (s.rtp_packets_received, s.bytes_received, s.frames_emitted),
            (1, pkt.len() as u64, 1)
        );
    }

    #[test]
    fn a_large_packet_is_rechunked_into_payload_size_bundles() {
        let m = mount();
        let mut reader = m.fanout.subscribe();
        // Read the app side raw (headers included) through the mount's
        // own receiver, bypassing the transport's header strip.
        let app_rx = m.take_app_rx().unwrap();
        let mut a = Mp2tAdapter::new(m.clone(), 96);
        let payload = ts(20);
        let mut pkt = rtp(96, &payload);
        pkt[1] |= 0x80; // marker set on the source; never copied to a bundle
        a.on_rtp(0, &pkt);
        a.on_rtp(0, &rtp(96, &ts(1)));

        let readers: Vec<Bytes> = std::iter::from_fn(|| reader.try_recv().ok()).collect();
        let sizes: Vec<usize> = readers.iter().map(|b| b.len()).collect();
        assert_eq!(sizes, [1316, 1316, 1128, 188]);
        let joined: Vec<u8> = readers[..3]
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect();
        assert_eq!(joined, payload);

        let app: Vec<Bytes> = app_rx.try_iter().collect();
        assert_eq!(app.len(), 4);
        for (i, (packet, ts)) in app.iter().zip(&readers).enumerate() {
            let h = RtpHeader::decode(packet).unwrap();
            assert_eq!(h.payload_offset, RTP_HEADER_LEN, "bundle {i}: plain header");
            assert_eq!(&packet[RTP_HEADER_LEN..], &ts[..], "bundle {i}");
            assert_eq!(h.header.payload_type, RTP_PT_MP2T, "bundle {i}");
            assert!(!h.header.marker, "bundle {i}");
            assert_eq!(h.header.seq, i as u16, "bundle {i}: adapter's own counter");
            assert_eq!(h.header.timestamp, 0x1000, "bundle {i}: source timestamp");
            assert_eq!(h.header.ssrc, 0xDEAD_BEEF, "bundle {i}: source ssrc");
        }
        let s = m.stats_snapshot();
        assert_eq!((s.rtp_packets_received, s.frames_emitted), (2, 4));
    }

    #[test]
    fn dynamic_pt_is_rewritten_to_33_for_the_app_side() {
        let m = mount();
        let mut t = crate::rtsp::server::publish::mount::PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        let mut a = Mp2tAdapter::new(m.clone(), 96);
        a.on_rtp(0, &rtp(96, &ts(1)));
        let mut buf = vec![0u8; 2048];
        // RtpRecvTransport pins PT 33 — a forwarded PT 96 would have been dropped as malformed.
        let n = tst_core::transport::RecvTransport::recv_bytes(&mut t, &mut buf).unwrap();
        assert_eq!(n, 188);
    }

    #[test]
    fn csrc_extension_and_padding_are_stripped_for_readers() {
        let m = mount();
        let mut reader = m.fanout.subscribe();
        let mut a = Mp2tAdapter::new(m.clone(), 33);
        // V=2 P=1 X=1 CC=1 ; PT 33 ; 1 CSRC ; ext header (profile 0xBEDE, len 1 word) ; 1 ext word ; payload ; 3 pad bytes (last = 3)
        let mut pkt = vec![
            0xB1, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0xCC, 0xCC, 0xCC, 0xCC, 0xBE, 0xDE, 0, 1, 1, 2,
            3, 4,
        ];
        pkt.extend_from_slice(&ts(1));
        pkt.extend_from_slice(&[0, 0, 3]);
        a.on_rtp(0, &pkt);
        assert_eq!(reader.try_recv().unwrap().len(), 188);
    }

    #[test]
    fn malformed_payloads_are_dropped_and_counted() {
        let m = mount();
        let mut reader = m.fanout.subscribe();
        let mut a = Mp2tAdapter::new(m.clone(), 33);
        a.on_rtp(0, &rtp(33, &ts(1)[..100])); // not a 188 multiple
        a.on_rtp(0, &rtp(33, &[0u8; 188])); // no sync byte
        a.on_rtp(0, &rtp(96, &ts(1))); // wrong PT
        a.on_rtp(0, &[0x80, 33, 0]); // truncated header
        a.on_rtp(0, &rtp(33, &[])); // empty payload
        assert!(reader.try_recv().is_err());
        assert_eq!(m.stats_snapshot().malformed_packets, 5);
        assert_eq!(
            m.stats_snapshot().rtp_packets_received,
            5,
            "counted at wire level before validation"
        );
    }

    #[test]
    fn rtcp_is_ignored_by_the_mp2t_adapter() {
        let m = mount();
        let mut a = Mp2tAdapter::new(m.clone(), 33);
        a.on_rtcp(0, &[0x80, 200, 0, 6]);
        assert_eq!(m.stats_snapshot().malformed_packets, 0);
    }
}
