//! UDP-transport ingest path for a published mount.
//!
//! One [`spawn_udp_ingest`] task runs per `TrackTransport::Udp` track
//! (`handle_record` spawns it once RECORD starts the session, and again
//! for any track that didn't have one yet on a later RECORD — see the
//! `udp_spawned` flag on `PublishTrack`). It owns that track's
//! SETUP-bound RTP+RTCP socket pair for as long as the publisher
//! records: both sockets are polled in one `tokio::select!`, RTP
//! datagrams pass the source check in [`admit`], and RTCP datagrams pass
//! the IP half of it ([`same_ip`]) before reaching the adapter.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::packet::RtpHeader;

use super::adapter::PublishAdapter;
use super::mount::PublishMountState;
use super::session::PublishSession;

/// What [`admit`] decided for one RTP datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// From the publisher's IP and a valid RTP packet of the track's
    /// payload type: deliver it (the latch now names its source).
    Accept,
    /// From an IP other than the publisher's control connection: drop
    /// it and count it in `source_rejected`.
    RejectForeignIp,
    /// From the publisher's IP but not an RTP packet of the track's
    /// payload type: the latch is untouched; the adapter drops it and
    /// counts it in `malformed_packets`.
    RejectGarbage,
}

/// Source check for one RTP datagram from `from`. `peer_ip` is the IP of
/// the publisher's RTSP control connection; `packet_ok` says the
/// datagram decodes as RTP with the track's announced payload type.
///
/// Only datagrams from `peer_ip` are admitted, so a third host cannot
/// feed or blind the mount. The port is learned rather than taken from
/// the announced `client_port` (NAT often rewrites it): the first valid
/// packet latches its full source address, and a later valid packet from
/// the same IP but another port re-latches (a NAT rebinding mid-stream).
/// Garbage never latches. IPv4-mapped IPv6 addresses compare equal to
/// their IPv4 form, so a dual-stack listener does not reject its own
/// publisher.
pub(crate) fn admit(
    latched: &mut Option<SocketAddr>,
    peer_ip: IpAddr,
    from: SocketAddr,
    packet_ok: bool,
) -> Admit {
    if !same_ip(from, peer_ip) {
        return Admit::RejectForeignIp;
    }
    if !packet_ok {
        return Admit::RejectGarbage;
    }
    if *latched != Some(from) {
        if let Some(previous) = *latched {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                %previous,
                %from,
                "publisher RTP source port changed; re-latched"
            );
        }
        *latched = Some(from);
    }
    Admit::Accept
}

/// `from` is the publisher's IP. IPv4-mapped IPv6 addresses compare
/// equal to their IPv4 form.
pub(crate) fn same_ip(from: SocketAddr, peer_ip: IpAddr) -> bool {
    from.ip().to_canonical() == peer_ip.to_canonical()
}

/// Run one track's UDP RTP+RTCP ingest loop until `cancel` fires or a
/// socket read fails.
///
/// RTP datagrams go through [`admit`] against `peer_ip` (the publisher's
/// control-connection IP) and `expected_pt` (the track's announced
/// payload type). RTCP datagrams are forwarded to the adapter's `on_rtcp`
/// only when they come from `peer_ip` too ([`same_ip`]; others count in
/// `source_rejected`): sender reports steer the elementary adapter's
/// KLV alignment, so a third host must not be able to feed them. The
/// RTCP port is not checked — RFC 3550 §6.4 allows a participant's RTP
/// and RTCP source ports to differ, and NAT can rewrite either
/// independently.
///
/// Counting of accepted packets happens inside the adapter (`on_rtp`
/// ticks `rtp_packets_received`/`bytes_received` itself); this loop only
/// counts `source_rejected`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_udp_ingest(
    track: usize,
    rtp: Arc<UdpSocket>,
    rtcp: Arc<UdpSocket>,
    peer_ip: IpAddr,
    expected_pt: u8,
    adapter: Arc<Mutex<Box<dyn PublishAdapter>>>,
    mount: Arc<PublishMountState>,
    last_media_ms: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rtp_buf = vec![0u8; 65_536];
        let mut rtcp_buf = vec![0u8; 65_536];
        let mut latched: Option<SocketAddr> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                r = rtp.recv_from(&mut rtp_buf) => match r {
                    Ok((n, from)) => {
                        let packet = &rtp_buf[..n];
                        let packet_ok = RtpHeader::decode(packet)
                            .is_ok_and(|p| p.header.payload_type == expected_pt);
                        match admit(&mut latched, peer_ip, from, packet_ok) {
                            Admit::RejectForeignIp => {
                                mount.tick(|s| s.source_rejected += 1);
                                continue;
                            }
                            Admit::RejectGarbage => {}
                            Admit::Accept => {
                                last_media_ms.store(PublishSession::now_ms(), Ordering::Relaxed);
                            }
                        }
                        adapter
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .on_rtp(track, packet);
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: "tst_rtp::server::publish",
                            error = %e,
                            "udp rtp recv failed; ending ingest task"
                        );
                        break;
                    }
                },
                r = rtcp.recv_from(&mut rtcp_buf) => if let Ok((n, from)) = r {
                    if !same_ip(from, peer_ip) {
                        tracing::debug!(
                            target: "tst_rtp::server::publish",
                            from = ?from,
                            "publisher RTCP from a foreign IP; dropped"
                        );
                        mount.tick(|s| s.source_rejected += 1);
                        continue;
                    }
                    adapter
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .on_rtcp(track, &rtcp_buf[..n]);
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::server::publish::adapter::Mp2tAdapter;
    use std::time::Duration;

    #[test]
    fn admit_garbage_first_does_not_latch() {
        let peer: IpAddr = "10.0.0.5".parse().unwrap();
        let src: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        let mut latched = None;
        assert_eq!(admit(&mut latched, peer, src, false), Admit::RejectGarbage);
        assert_eq!(latched, None, "garbage never latches");
        assert_eq!(admit(&mut latched, peer, src, true), Admit::Accept);
        assert_eq!(latched, Some(src));
    }

    #[test]
    fn admit_rejects_a_foreign_ip_valid_or_not_before_and_after_the_latch() {
        let peer: IpAddr = "10.0.0.5".parse().unwrap();
        let foreign: SocketAddr = "10.0.0.66:4000".parse().unwrap();
        let mut latched = None;
        assert_eq!(
            admit(&mut latched, peer, foreign, true),
            Admit::RejectForeignIp
        );
        assert_eq!(latched, None, "a foreign first packet does not latch");
        let src: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        assert_eq!(admit(&mut latched, peer, src, true), Admit::Accept);
        assert_eq!(
            admit(&mut latched, peer, foreign, false),
            Admit::RejectForeignIp
        );
        assert_eq!(latched, Some(src));
    }

    #[test]
    fn admit_relatches_a_new_port_on_the_same_ip() {
        let peer: IpAddr = "10.0.0.5".parse().unwrap();
        let first: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        let rebound: SocketAddr = "10.0.0.5:51234".parse().unwrap();
        let mut latched = None;
        assert_eq!(admit(&mut latched, peer, first, true), Admit::Accept);
        assert_eq!(admit(&mut latched, peer, rebound, true), Admit::Accept);
        assert_eq!(latched, Some(rebound));
        // Garbage from the old port does not move the latch back.
        assert_eq!(
            admit(&mut latched, peer, first, false),
            Admit::RejectGarbage
        );
        assert_eq!(latched, Some(rebound));
    }

    #[test]
    fn admit_treats_an_ipv4_mapped_source_as_its_ipv4_peer() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:4000".parse().unwrap();
        let mut latched = None;
        assert_eq!(admit(&mut latched, peer, mapped, true), Admit::Accept);
    }

    /// Counts RTCP packets the ingest loop hands over.
    struct RtcpCounter(Arc<AtomicU64>);
    impl PublishAdapter for RtcpCounter {
        fn on_rtp(&mut self, _: usize, _: &[u8]) {}
        fn on_rtcp(&mut self, _: usize, _: &[u8]) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn flush(&mut self) {}
    }

    /// Spawn an ingest loop for `peer_ip`, send one RTCP datagram from
    /// 127.0.0.1, and return `(rtcp delivered, source_rejected)` once the
    /// datagram has been handled either way.
    async fn rtcp_from_loopback(peer_ip: &str) -> (u64, u64) {
        let mount = PublishMountState::new("/p", 8, Default::default());
        let delivered = Arc::new(AtomicU64::new(0));
        let adapter: Arc<Mutex<Box<dyn PublishAdapter>>> =
            Arc::new(Mutex::new(Box::new(RtcpCounter(delivered.clone()))));
        let rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let rtcp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = rtcp.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let j = spawn_udp_ingest(
            1,
            rtp,
            rtcp,
            peer_ip.parse().unwrap(),
            33,
            adapter,
            mount.clone(),
            Arc::new(AtomicU64::new(0)),
            cancel.clone(),
        );
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(&[0x80, 200, 0, 6], target).await.unwrap();
        let handled =
            || delivered.load(Ordering::Relaxed) + mount.stats_snapshot().source_rejected > 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !handled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("RTCP datagram handled");
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), j)
            .await
            .unwrap()
            .unwrap();
        (
            delivered.load(Ordering::Relaxed),
            mount.stats_snapshot().source_rejected,
        )
    }

    #[tokio::test]
    async fn udp_rtcp_is_delivered_only_from_the_publishers_ip() {
        assert_eq!(rtcp_from_loopback("127.0.0.1").await, (1, 0));
        // 127.0.0.1 is not the publisher: a third host cannot steer
        // alignment with forged sender reports.
        assert_eq!(rtcp_from_loopback("10.0.0.5").await, (0, 1));
    }

    #[tokio::test]
    async fn udp_ingest_relatches_a_same_ip_port_change_and_counts_garbage_as_malformed() {
        let mount = PublishMountState::new("/p", 8, Default::default());
        let adapter: Arc<Mutex<Box<dyn PublishAdapter>>> =
            Arc::new(Mutex::new(Box::new(Mp2tAdapter::new(mount.clone(), 33))));
        let rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let rtcp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = rtp.local_addr().unwrap();
        let last = Arc::new(AtomicU64::new(0));
        let cancel = CancellationToken::new();
        let j = spawn_udp_ingest(
            0,
            rtp,
            rtcp,
            "127.0.0.1".parse().unwrap(),
            33,
            adapter,
            mount.clone(),
            last.clone(),
            cancel.clone(),
        );
        let a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let pkt = {
            let mut v = vec![0x80u8, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
            v.push(0x47);
            v.extend(std::iter::repeat_n(0u8, 187));
            v
        };
        b.send_to(&[0xFFu8; 40], target).await.unwrap(); // garbage first: no latch
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.send_to(&pkt, target).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        b.send_to(&pkt, target).await.unwrap(); // same IP, new port: re-latched
        a.send_to(&pkt, target).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = mount.stats_snapshot();
        assert_eq!(s.frames_emitted, 3);
        assert_eq!(s.source_rejected, 0);
        assert_eq!(s.malformed_packets, 1);
        assert!(last.load(Ordering::Relaxed) > 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), j)
            .await
            .unwrap()
            .unwrap();
    }
}
