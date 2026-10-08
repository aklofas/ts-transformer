//! Per-shape adapters: RTP in, TS frames out to the mount's two sinks.
//!
//! Each wire shape a publisher may announce (§1 classification table in
//! [`super::PublishShape`]) gets its own [`PublishAdapter`] impl, fed
//! RTP/RTCP packets by the session's track dispatch (a later task). This
//! module ships the one shape in scope today: MP2T passthrough.

use std::sync::Arc;

use bytes::Bytes;

use crate::packet::{RTP_PT_MP2T, RtpHeader};

use super::mount::PublishMountState;

/// Adapts incoming RTP/RTCP packets on a published mount into demuxed
/// events.
///
/// Implementations never fail outward — a malformed or unexpected packet
/// is dropped and counted (via the mount's `malformed_packets` stat),
/// mirroring how [`crate::transport::RtpRecvTransport::recv_bytes`]
/// treats a bad payload on the receive side.
///
/// Constructed by `handle_announce` (Task 6); `on_rtp`/`on_rtcp` are
/// driven from live RTSP traffic by the session's track dispatch
/// (Tasks 7/8 of this arc — interleaved `$` frames and UDP ingest).
pub(crate) trait PublishAdapter: Send {
    /// One RTP packet from `track` (index into the announce's track table).
    /// Never fails; drops count. Called by Task 7/8's track dispatch —
    /// unreached (and so unreachable from any impl) until that lands.
    #[allow(dead_code)]
    fn on_rtp(&mut self, track: usize, packet: &[u8]);
    /// One RTCP packet from `track`'s RTCP channel/socket. Called from
    /// the interleaved `$`-frame arm (Task 7) and from Task 8's UDP
    /// ingest tasks.
    fn on_rtcp(&mut self, track: usize, packet: &[u8]);
    /// Publisher ended: push out whatever is pending. Called by
    /// `PublishSession::end` — reached today.
    fn flush(&mut self);
}

/// RFC 2250 passthrough: the single announced track carries whole TS
/// packets as the RTP payload. The TS payload goes to PLAY readers
/// as-is; the whole RTP packet goes to the application transport with
/// its payload-type bits forced to 33 so
/// [`crate::transport::RtpRecvTransport`]'s PT pin holds regardless of
/// which PT the publisher's SDP declared (33 directly, or a dynamic PT
/// mapped to `MP2T/90000` — see the classification table).
///
/// Stateless across calls other than the mount handle and the PT this
/// instance was built to expect — there is nothing to reassemble: one
/// RTP packet is one TS bundle.
pub(crate) struct Mp2tAdapter {
    // Both fields are read only from `on_rtp`'s body, which is itself
    // unreached until Tasks 7/8 drive live RTP through it (see the
    // `#[allow(dead_code)]` on `PublishAdapter::on_rtp`).
    #[allow(dead_code)]
    mount: Arc<PublishMountState>,
    #[allow(dead_code)]
    expected_pt: u8,
}

impl Mp2tAdapter {
    pub(crate) fn new(mount: Arc<PublishMountState>, expected_pt: u8) -> Self {
        Self { mount, expected_pt }
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
        let payload = &packet[parsed.payload_offset..parsed.payload_end];
        if !crate::transport::is_valid_mp2t_payload(payload) {
            self.mount.tick(|s| s.malformed_packets += 1);
            return;
        }
        // App side: whole packet, PT bits forced to 33 so
        // RtpRecvTransport's pin holds even for a dynamic-PT publisher.
        let mut app = packet.to_vec();
        app[1] = (app[1] & 0x80) | RTP_PT_MP2T;
        self.mount
            .emit(Bytes::copy_from_slice(payload), Bytes::from(app));
    }

    fn on_rtcp(&mut self, _track: usize, _packet: &[u8]) {
        // RFC 2250 carries no KLV/AU timing that needs an RTCP anchor;
        // nothing to do until a later task adds clock alignment.
    }

    fn flush(&mut self) {
        // Stateless: nothing buffered to push out.
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
    fn valid_packet_feeds_readers_with_payload_and_app_with_whole_packet() {
        let m = mount();
        let mut reader = m.fanout.subscribe();
        let mut a = Mp2tAdapter::new(m.clone(), 33);
        let pkt = rtp(33, &ts(3));
        a.on_rtp(0, &pkt);
        assert_eq!(reader.try_recv().unwrap().as_ref(), &pkt[12..]);
        let s = m.stats_snapshot();
        assert_eq!(
            (s.rtp_packets_received, s.bytes_received, s.frames_emitted),
            (1, pkt.len() as u64, 1)
        );
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
