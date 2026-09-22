//! WP-C1 — the tst-core transport conformance kit over tst-rtp's three
//! receive/send shapes: plain `rtp://` sender, plain `rtp://` receiver, and
//! the receiver an `RtspClient` session hands out (`into_recv_transport`)
//! against the in-crate `RtspServer`.
//!
//! Port discovery is `free_rtp_port_base`, the same discover-then-release
//! helper `tests/rtp/loopback_unicast.rs` uses (an OS-assigned pair
//! `base`/`base+1`, released before the transport binds it — the `network`
//! nextest group serialises these tests so nobody races for the freed pair).

use std::net::UdpSocket;
use std::sync::Mutex;

use tst_core::transport::Transport;
use tst_core::transport::conformance::{self as kit, BrokenSource, SendPark};
use tst_rtp::{RtpRecvTransport, RtpTransport};

/// Deliverable ceiling of an RTP MP2T receiver: the 16-bit datagram /
/// interleaved-frame limit minus the fixed RTP header
/// (`RECV_SCRATCH_LEN - RTP_HEADER_LEN` in `src/transport.rs`).
const RTP_RECV_CEILING: usize = 65535 - tst_rtp::RTP_HEADER_LEN;

fn free_rtp_port_base() -> u16 {
    // Bounded so a host under unusual port pressure fails deterministically
    // with a clear message instead of spinning until nextest's timeout.
    for _ in 0..1000 {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let base = s.local_addr().unwrap().port();
        if base < u16::MAX {
            if let Ok(companion) = UdpSocket::bind(("127.0.0.1", base + 1)) {
                drop(companion);
                drop(s);
                return base;
            }
        }
        drop(s); // base + 1 was taken (or base == u16::MAX); retry.
        std::thread::yield_now();
    }
    panic!(
        "free_rtp_port_base: no free base/base+1 UDP port pair on 127.0.0.1 after 1000 attempts"
    );
}

/// Send side: every factory call targets a fresh port pair that a plain
/// `UdpSocket` holds open (never read). Without a bound receiver the kernel
/// would answer our datagrams with ICMP port-unreachable and a later `send`
/// would fail `ECONNREFUSED` — a `Broken` the kit's cancel row must never see.
struct SendSink {
    sinks: Mutex<Vec<UdpSocket>>,
}

impl SendSink {
    fn new() -> Self {
        Self {
            sinks: Mutex::new(Vec::new()),
        }
    }
    fn connect(&self) -> RtpTransport {
        let base = free_rtp_port_base();
        let rtp = UdpSocket::bind(("127.0.0.1", base)).expect("hold rtp port");
        let rtcp = UdpSocket::bind(("127.0.0.1", base + 1)).expect("hold rtcp port");
        self.sinks.lock().unwrap().extend([rtp, rtcp]);
        RtpTransport::connect(&format!("rtp://127.0.0.1:{base}")).expect("connect")
    }
}

/// Receive side: each factory call binds a receiver on a fresh pair and keeps
/// a sender pointed at it for `feed`. The kit takes one transport per row, so
/// the sender must be replaced in lockstep — `feed` always reaches the
/// receiver the current row is holding.
struct RecvPair {
    sender: Mutex<Option<RtpTransport>>,
}

impl RecvPair {
    fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }
    fn listen(&self) -> RtpRecvTransport {
        let base = free_rtp_port_base();
        let url = format!("rtp://127.0.0.1:{base}");
        let recv = RtpRecvTransport::listen(&url).expect("listen");
        let send = RtpTransport::connect(&url).expect("feed sender");
        *self.sender.lock().unwrap() = Some(send);
        recv
    }
    fn feed(&self, bytes: &[u8]) {
        self.sender
            .lock()
            .unwrap()
            .as_mut()
            .expect("a live feed sender")
            .send_bytes(bytes)
            .expect("feed send");
    }
}

#[test]
fn rtp_send_contract() {
    let sink = SendSink::new();
    // NotProducible: a datagram sender whose port is held open has no peer to
    // lose (the row prints its skip line).
    kit::assert_send_contract(|| sink.connect(), BrokenSource::NotProducible);
    // Both park modes hold on a datagram sender: `Loop` is covered by the
    // aggregate above (SendOptions::default()), `NextCall` here.
    kit::send_cancel_during_park_is_explicit_close(sink.connect(), SendPark::NextCall);
}

#[test]
fn rtp_recv_contract() {
    let pair = RecvPair::new();
    // NotProducible: nothing on a UDP wire can break a bound receiver (the
    // table's "false after Broken" is vacuous here, and so is
    // `peer_eof_is_not_a_cancel` — a datagram receiver has no peer EOF).
    kit::assert_recv_contract(
        || pair.listen(),
        |b| pair.feed(b),
        BrokenSource::NotProducible,
    );
    kit::recv_max_payload_ge_ceiling(&pair.listen(), RTP_RECV_CEILING);
}

/// The receiver an RTSP session hands out is an `RtpRecvTransport` whose
/// source is the session's UDP socket; the kit's feed pushes keyframes into
/// the server mount, which the server muxes and fans out to the session — the
/// probe bytes are NOT what arrives, which the kit's feed contract allows
/// (delivery, not fidelity). Server→own-client delivery is proven end-to-end
/// by `crates/tst-interop`'s `serve rtsp_serve_round_trip_via_own_client`.
#[cfg(feature = "rtsp-server")]
#[test]
fn rtsp_client_recv_contract() {
    use std::sync::atomic::{AtomicI64, Ordering};
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
    use tst_rtp::{RtspClient, RtspServer};

    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let cfg = b.build().unwrap();

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", cfg).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    // Every session's control connection stays open for the row's lifetime
    // (dropping the client TEARDOWNs the session).
    let clients: Mutex<Vec<RtspClient>> = Mutex::new(Vec::new());
    let pts = AtomicI64::new(0);

    let factory = || {
        let mut client = RtspClient::connect(&url).unwrap();
        client.options().unwrap();
        let sdp = client.describe().unwrap();
        let session = client.setup_mp2t_auto(&sdp).unwrap();
        client.play().unwrap();
        clients.lock().unwrap().push(client);
        session.into_recv_transport()
    };
    let feed = |_probe: &[u8]| {
        // 4-byte start code + IDR NAL header + one RBSP byte — the shape
        // `mount_stats_tick_after_push` (tests/rtsp_server/loopback_udp.rs)
        // pushes; the muxer emits PAT/PMT/PES for it at once.
        //
        // Pushed THREE times: the first access unit carries PSI the fan-out
        // may emit before the fresh session's PLAY has fully wired its peer,
        // so a single push can be delivered to nobody. The kit's feed
        // contract is "something arrives", not "exactly these bytes", so
        // over-feeding is free.
        let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        for _ in 0..3 {
            let t = pts.fetch_add(3_000, Ordering::SeqCst);
            mount.push_video(&nal, Pts90khz::new(t), true).unwrap();
        }
    };
    // NotProducible: the session's UDP source cannot be broken by a peer (a
    // server TEARDOWN ends the RTSP session, not the datagram socket).
    kit::assert_recv_contract(factory, feed, BrokenSource::NotProducible);
    kit::recv_max_payload_ge_ceiling(&factory(), RTP_RECV_CEILING);
    server.stop().ok();
}
