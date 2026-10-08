//! UDP-transport ingest path for a published mount.
//!
//! One [`spawn_udp_ingest`] task runs per `TrackTransport::Udp` track
//! (`handle_record` spawns it once RECORD starts the session, and again
//! for any track that didn't have one yet on a later RECORD — see the
//! `udp_spawned` flag on `PublishTrack`). It owns that track's
//! SETUP-bound RTP+RTCP socket pair for as long as the publisher
//! records: both sockets are polled in one `tokio::select!`, RTP
//! packets latch the publisher's source address, and RTCP packets are
//! handed to the adapter unconditionally.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::adapter::PublishAdapter;
use super::mount::PublishMountState;
use super::session::PublishSession;

/// Run one track's UDP RTP+RTCP ingest loop until `cancel` fires or a
/// socket read fails.
///
/// Source latching: the first RTP datagram's source address is latched
/// into `peer`; a later RTP datagram from a different address is
/// dropped and ticked as `malformed_packets` ("source rejected"). RTCP
/// datagrams are forwarded to the adapter's `on_rtcp` unconditionally —
/// RFC 3550 §6.4 allows a participant's RTP and RTCP source ports to
/// differ (and NAT can rewrite either independently), so this loop
/// never tries to correlate an RTCP sender against the RTP-latched
/// `peer`; RTCP carries its own SSRC-based identity, and rejecting it
/// here would just lose SR/RR reports for no attribution benefit.
///
/// Counting happens inside the adapter (`on_rtp` ticks
/// `rtp_packets_received`/`bytes_received` itself) — this loop never
/// double-counts those.
pub(crate) fn spawn_udp_ingest(
    track: usize,
    rtp: Arc<UdpSocket>,
    rtcp: Arc<UdpSocket>,
    adapter: Arc<Mutex<Box<dyn PublishAdapter>>>,
    mount: Arc<PublishMountState>,
    last_media_ms: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rtp_buf = vec![0u8; 65_536];
        let mut rtcp_buf = vec![0u8; 65_536];
        let mut peer: Option<SocketAddr> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                r = rtp.recv_from(&mut rtp_buf) => match r {
                    Ok((n, from)) => {
                        match peer {
                            None => peer = Some(from),
                            Some(p) if p != from => {
                                mount.tick(|s| s.malformed_packets += 1);
                                continue;
                            }
                            _ => {}
                        }
                        last_media_ms.store(PublishSession::now_ms(), Ordering::Relaxed);
                        adapter
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .on_rtp(track, &rtp_buf[..n]);
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
                r = rtcp.recv_from(&mut rtcp_buf) => if let Ok((n, _)) = r {
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

    #[tokio::test]
    async fn udp_ingest_latches_the_first_source_and_rejects_others() {
        let mount = PublishMountState::new("/p", 8);
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
        a.send_to(&pkt, target).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        b.send_to(&pkt, target).await.unwrap(); // different source: rejected
        a.send_to(&pkt, target).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = mount.stats_snapshot();
        assert_eq!(s.frames_emitted, 2);
        assert_eq!(s.malformed_packets, 1);
        assert!(last.load(Ordering::Relaxed) > 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), j)
            .await
            .unwrap()
            .unwrap();
    }
}
