//! Per-peer fanout subscriber task — spawned by the PLAY handler for
//! each connected client. Drains the mount's broadcast channel and
//! writes RTP frames to the peer over UDP or TCP-interleaved.
//!
//! Spawned per PLAY by [`super::handlers`]'s handle_play.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::clock::RtpClock;
use crate::packet::{RTP_HEADER_LEN, RtpHeader};

/// Per-peer transport choice — UDP unicast or TCP-interleaved over the
/// per-session TCP stream.
///
/// Multicast mounts don't spawn per-peer tasks — they have a single
/// shared sender; see the `multicast` module for that path.
pub(crate) enum PeerTransport {
    Udp {
        socket: Arc<tokio::net::UdpSocket>,
        peer_addr: SocketAddr,
    },
    Interleaved {
        /// Async-locked TCP write half. Per-session task owns this; the
        /// fanout task locks per-frame.
        writer: Arc<AsyncMutex<tokio::net::tcp::OwnedWriteHalf>>,
        /// SETUP-allocated channel for RTP (server's response Transport
        /// header carried e.g. `interleaved=0-1`; RTP rides 0, RTCP 1).
        rtp_channel: u8,
    },
}

/// Per-peer dropped-frame counter, observable from outside the task.
///
/// When constructed with [`Self::with_mount_total`], every `add` also
/// bumps a shared mount-level total so `MountStats::frames_dropped_total`
/// sums the drops across all of a mount's peers in real time. Constructed
/// via `Default` (no mount link) the per-peer count is still tracked but
/// not aggregated — used by the fanout unit tests.
#[derive(Default)]
pub(crate) struct PeerDropCounter {
    pub(crate) dropped: AtomicU64,
    /// Shared mount-level dropped-frame total; `None` for unlinked
    /// counters (unit tests). Bumped alongside `dropped` on every `add`.
    mount_total: Option<Arc<AtomicU64>>,
}

impl PeerDropCounter {
    /// Construct a counter linked to a mount's shared dropped-frame total.
    pub(crate) fn with_mount_total(mount_total: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            dropped: AtomicU64::new(0),
            mount_total: Some(mount_total),
        })
    }
    pub(crate) fn add(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
        if let Some(total) = &self.mount_total {
            total.fetch_add(n, Ordering::Relaxed);
        }
    }
}

/// Spawn the per-peer fanout subscriber task. Returns a `JoinHandle` +
/// the per-peer drop counter so the caller (the PLAY handler / session
/// state) can observe progress.
///
/// The task exits when:
/// - `cancel` is triggered (graceful or hard).
/// - `server_cancel` (the server's graceful cancel) is triggered.
/// - The broadcast channel is closed (mount removed / server shutting down).
/// - The peer transport returns a fatal I/O error.
///
/// Every exit lands at a FRAME BOUNDARY: a frame write runs to completion
/// inside its select arm, and the cancel arms are polled first (`biased;`)
/// so a cancelled task never sends one more stale frame. PLAY-replacement,
/// PAUSE and TEARDOWN rely on this — an interleaved frame is written under
/// the session's shared TCP writer, and only `JoinHandle::abort()` (the
/// session's Drop, where FIN follows) may ever cut one in half.
///
/// `ssrc` is the RTP SSRC for this peer (random per-peer to keep peer-side
/// jitter buffers isolated even though they all carry identical TS bytes).
/// `initial_seq` is the starting RTP sequence number (random per
/// RFC 3550 §5.1 to avoid known-plaintext attacks on encrypted RTP — not
/// strictly needed since we don't ship SRTP, but matches the convention).
/// `clock` is the session-local 90 kHz timestamp source. The caller
/// snapshots `clock.now_ticks()` before calling this function and reports
/// that value in the PLAY `RTP-Info` `rtptime` field (RFC 7826 §18.45),
/// so the RTP timestamps in the first packets correspond to what the
/// client was told to expect.
// Eight positional arguments: the two cancel tokens, the three RTP seeds
// and the two shared handles are each spawn-time constants with one
// caller (`handle_play`); a struct would only move the same list.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_peer_fanout(
    mut rx: broadcast::Receiver<Bytes>,
    transport: PeerTransport,
    cancel: CancellationToken,
    server_cancel: CancellationToken,
    ssrc: u32,
    initial_seq: u16,
    clock: RtpClock,
    drop_counter: Arc<PeerDropCounter>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = initial_seq;
        // Task-owned scratch buffers: hoisted above the recv loop so the
        // heap allocation is reused across frames rather than re-allocated
        // per payload. Sized for one default TS burst; capacity grows to
        // the high-water mark after the first few frames and stays there.
        let mut datagram: Vec<u8> =
            Vec::with_capacity(RTP_HEADER_LEN + crate::url::DEFAULT_PKT_SIZE);
        let mut framed: Vec<u8> =
            Vec::with_capacity(4 + RTP_HEADER_LEN + crate::url::DEFAULT_PKT_SIZE);
        loop {
            tokio::select! {
                // Cancel arms first, biased: when a payload and a cancel are
                // both ready the task exits instead of writing the frame.
                biased;
                _ = cancel.cancelled() => {
                    return;
                }
                // The server's graceful cancel: belt-and-braces beside the
                // session Drop so a fanout can never outlive
                // `RtspServer::stop`.
                _ = server_cancel.cancelled() => {
                    return;
                }
                payload_res = rx.recv() => match payload_res {
                    Ok(payload) => {
                        // Build RTP datagram: 12-byte header + payload.
                        datagram.clear();
                        datagram.resize(RTP_HEADER_LEN, 0);
                        RtpHeader::new(seq, clock.now_ticks(), ssrc)
                            .encode_into(&mut datagram);
                        datagram.extend_from_slice(&payload);
                        seq = seq.wrapping_add(1);

                        match &transport {
                            PeerTransport::Udp { socket, peer_addr } => {
                                if let Err(e) = socket.send_to(&datagram, *peer_addr).await {
                                    tracing::warn!(
                                        target: "tst_rtp::server",
                                        peer = %peer_addr, error = %e,
                                        "UDP send failed; exiting fanout task"
                                    );
                                    return;
                                }
                            }
                            PeerTransport::Interleaved { writer, rtp_channel } => {
                                // RFC 7826 §14: `$<channel:u8><length:u16-BE><payload>`.
                                framed.clear();
                                framed.push(b'$');
                                framed.push(*rtp_channel);
                                framed.extend_from_slice(&(datagram.len() as u16).to_be_bytes());
                                framed.extend_from_slice(&datagram);
                                let mut g = writer.lock().await;
                                use tokio::io::AsyncWriteExt;
                                if let Err(e) = g.write_all(&framed).await {
                                    tracing::warn!(
                                        target: "tst_rtp::server",
                                        error = %e,
                                        "interleaved write failed; exiting fanout task"
                                    );
                                    return;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        drop_counter.add(n);
                        tracing::warn!(
                            target: "tst_rtp::server",
                            lagged = n,
                            "peer lagged broadcast; frames dropped"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Mount removed or server shut down.
                        return;
                    }
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    /// Spawn a UDP-peer fanout task, push 3 payloads, verify the peer
    /// receives 3 RTP datagrams (12-byte header + payload bytes).
    #[tokio::test]
    async fn udp_fanout_delivers_three_frames() {
        // Peer socket — receives what the fanout task sends.
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();

        // Fanout side: bind a sender socket + create the broadcast channel.
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (tx, rx) = broadcast::channel::<Bytes>(8);
        let cancel = CancellationToken::new();
        let drop_counter = Arc::new(PeerDropCounter::default());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };
        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel.clone(),
            CancellationToken::new(),
            0x12345678,
            1000,
            RtpClock::new(0),
            drop_counter.clone(),
        );

        // Push 3 payloads of 16 bytes each.
        for i in 0..3u8 {
            let mut buf = vec![0u8; 16];
            buf[0] = i;
            tx.send(Bytes::from(buf)).unwrap();
        }

        // Receive 3 datagrams, each 12-byte header + 16-byte payload = 28 bytes.
        for i in 0..3u8 {
            let mut recv = [0u8; 64];
            let (n, _) =
                tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv_from(&mut recv))
                    .await
                    .expect("timeout")
                    .unwrap();
            assert_eq!(n, RTP_HEADER_LEN + 16);
            // First byte of payload is the per-frame index we set above.
            assert_eq!(recv[RTP_HEADER_LEN], i);
        }

        // Cancel — task exits cleanly.
        cancel.cancel();
        let _ = handle.await;
    }

    /// Drop-counter ticks when broadcast lags. Capacity 2; push 5
    /// before the task drains any → at least 3 frames lag.
    #[tokio::test]
    async fn drop_counter_ticks_on_lag() {
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<Bytes>(2); // very small
        let cancel = CancellationToken::new();
        let drop_counter = Arc::new(PeerDropCounter::default());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };

        // Push 5 BEFORE spawning the task — guarantees broadcast::Sender's
        // backlog has lagged the (yet-to-be-created) subscriber by 3 frames.
        for i in 0..5u8 {
            // .send may return Err once the channel rolls over, but we ignore.
            let _ = tx.send(Bytes::from(vec![i; 8]));
        }
        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel.clone(),
            CancellationToken::new(),
            0xCAFEBABE,
            1,
            RtpClock::new(0),
            drop_counter.clone(),
        );

        // Give the task a moment to wake + observe lag.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let dropped = drop_counter.dropped.load(Ordering::Relaxed);
        assert!(dropped > 0, "expected drop counter to tick; got {dropped}");

        cancel.cancel();
        let _ = handle.await;
    }

    /// A peer counter built with `with_mount_total` aggregates its drops
    /// into the shared mount-level total in real time — the wiring behind
    /// `MountStats::frames_dropped_total`. Same deterministic push-before-
    /// drain setup as `drop_counter_ticks_on_lag`.
    #[tokio::test]
    async fn mount_total_aggregates_peer_drops() {
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (tx, rx) = broadcast::channel::<Bytes>(2);
        let cancel = CancellationToken::new();
        let mount_total = Arc::new(AtomicU64::new(0));
        let drop_counter = PeerDropCounter::with_mount_total(mount_total.clone());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };

        for i in 0..5u8 {
            let _ = tx.send(Bytes::from(vec![i; 8]));
        }
        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel.clone(),
            CancellationToken::new(),
            0xCAFEBABE,
            1,
            RtpClock::new(0),
            drop_counter.clone(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let peer_dropped = drop_counter.dropped.load(Ordering::Relaxed);
        let mount_dropped = mount_total.load(Ordering::Relaxed);
        assert!(
            peer_dropped > 0,
            "per-peer drop counter should tick; got {peer_dropped}"
        );
        assert_eq!(
            mount_dropped, peer_dropped,
            "mount total should equal the single peer's drops"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn cancel_exits_task() {
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<Bytes>(8);
        let cancel = CancellationToken::new();
        let drop_counter = Arc::new(PeerDropCounter::default());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };
        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel.clone(),
            CancellationToken::new(),
            0,
            0,
            RtpClock::new(0),
            drop_counter,
        );
        cancel.cancel();
        // Should exit within a reasonable bound.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("fanout task did not exit on cancel")
            .unwrap();
    }

    /// The server's graceful cancel alone ends the fanout; the per-peer
    /// `cancel` is never fired here. Without the `server_cancel` arm the
    /// task would keep sending until its broadcast closed.
    #[tokio::test]
    async fn server_cancel_exits_task() {
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<Bytes>(8);
        let cancel = CancellationToken::new();
        let server_cancel = CancellationToken::new();
        let drop_counter = Arc::new(PeerDropCounter::default());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };
        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel,
            server_cancel.clone(),
            0,
            0,
            RtpClock::new(0),
            drop_counter,
        );
        server_cancel.cancel();
        // Should exit within a reasonable bound.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("fanout task did not exit on server cancel")
            .unwrap();
    }

    /// The first RTP packet's sequence number must equal `initial_seq` and
    /// the caller's clock snapshot (`initial_rtptime`) must correspond to
    /// the timestamp embedded in that packet.
    ///
    /// This is the property required by RFC 7826 §18.45 / RFC 2326 §12.33:
    /// the server's PLAY `RTP-Info` `seq=` and `rtptime=` fields describe
    /// the first packet the client will receive, not hardcoded zeros.
    #[tokio::test]
    async fn first_rtp_packet_seq_matches_initial_seq() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let send_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (tx, rx) = broadcast::channel::<Bytes>(8);
        let cancel = CancellationToken::new();
        let drop_counter = Arc::new(PeerDropCounter::default());
        let transport = PeerTransport::Udp {
            socket: send_sock,
            peer_addr,
        };

        // Use a distinctive initial sequence number and clock snapshot
        // so a zero-regression would be obvious.
        let initial_seq: u16 = 0x1A2B;
        let clock = RtpClock::new(0);
        let initial_rtptime = clock.now_ticks();

        let handle = spawn_peer_fanout(
            rx,
            transport,
            cancel.clone(),
            CancellationToken::new(),
            0xDEAD_BEEF,
            initial_seq,
            clock,
            drop_counter,
        );

        tx.send(Bytes::from(vec![0xAAu8; 8])).unwrap();

        let mut buf = [0u8; 64];
        let (n, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv_from(&mut buf))
                .await
                .expect("timeout receiving first RTP packet")
                .unwrap();
        assert!(n >= RTP_HEADER_LEN, "datagram too short: {n}");

        // RFC 3550 §5.1: sequence number is bytes 2-3, big-endian.
        let pkt_seq = u16::from_be_bytes([buf[2], buf[3]]);
        assert_eq!(
            pkt_seq, initial_seq,
            "first packet seq {pkt_seq} must equal initial_seq {initial_seq} \
             (the value reported in the PLAY RTP-Info header)"
        );

        // RFC 3550 §5.1: timestamp is bytes 4-7, big-endian.
        // The packet timestamp is clock.now_ticks() at send time, which must
        // be >= initial_rtptime (a few ticks may have elapsed between the
        // snapshot and the send). Allow up to 90_000 ticks (1 second).
        let pkt_rtptime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let delta = pkt_rtptime.wrapping_sub(initial_rtptime);
        assert!(
            delta < 90_000,
            "packet rtptime {pkt_rtptime} is more than 1 s after \
             initial_rtptime {initial_rtptime} (delta={delta} ticks)"
        );

        cancel.cancel();
        let _ = handle.await;
    }
}
