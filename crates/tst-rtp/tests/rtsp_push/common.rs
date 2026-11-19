//! What the members of this binary share: a started server with one
//! publish mount, pushes to it, the elementary fixtures, and the mount's
//! stream read back.

use std::time::{Duration, Instant};

use base64::Engine as _;
use tst_core::codec::annexb::nals;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::transport::{EndReason, RecvTransport, TransportError};
use tst_pipeline::DemuxReceiver;
use tst_rtp::{
    PublishMountConfig, PublishMountHandle, PublishMountStats, PushDescription, RtpRecvTransport,
    RtspClientBuilder, RtspPush, RtspServer, RtspServerBuilder, VideoTrack,
};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::{SPROP_PPS_B64, SPROP_SPS_B64, demux_until_quiet};
use crate::fixtures::tcp_relay::TcpRelay;

/// The path of the publish mount every test pushes to.
pub const PATH: &str = "/pub";

/// The PTS step between access units (29.97 frames a second).
pub const AU_STEP: i64 = 3003;

/// A started server with a publish mount on [`PATH`]. Dropping it stops the
/// server, so a test that fails leaves none running.
pub struct Served {
    pub server: RtspServer,
    pub mount: PublishMountHandle,
    pub port: u16,
}

impl Served {
    /// A server with the defaults.
    pub fn new() -> Self {
        Self::with(|_| {}, PublishMountConfig::new())
    }

    /// A server `configure` set up, with a publish mount made with `mount`.
    pub fn with(configure: impl FnOnce(&mut RtspServerBuilder), mount: PublishMountConfig) -> Self {
        Self::at("rtsp://127.0.0.1:0", configure, mount)
    }

    /// [`Self::with`] for a server that listens at `url`.
    pub fn at(
        url: &str,
        configure: impl FnOnce(&mut RtspServerBuilder),
        mount: PublishMountConfig,
    ) -> Self {
        let mut b = RtspServerBuilder::new(url).unwrap();
        configure(&mut b);
        let server = b.build().unwrap();
        let mount = server.add_publish_mount_with(PATH, mount).unwrap();
        server.start().unwrap();
        let port = server.local_addr().unwrap().port();
        Self {
            server,
            mount,
            port,
        }
    }

    /// A server whose mount's receiver ends when its publisher leaves.
    pub fn ending_on_exit() -> Self {
        Self::with(
            |_| {},
            PublishMountConfig::new().end_on_publisher_exit(true),
        )
    }

    /// The mount's receiver, each receive bounded so a test that waits for
    /// bytes that never come fails instead of hanging.
    pub fn app(&self) -> RtpRecvTransport {
        let mut app = self.mount.clone().into_recv_transport().unwrap();
        app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
        app
    }

    pub fn url(&self) -> String {
        format!("rtsp://127.0.0.1:{}{PATH}", self.port)
    }

    pub fn builder(&self) -> RtspClientBuilder {
        RtspClientBuilder::new(&self.url()).unwrap()
    }

    pub fn push(&self, desc: &PushDescription) -> RtspPush {
        RtspPush::connect(self.builder(), desc).unwrap()
    }

    /// The mount's counters once `cond` holds; fails after five seconds.
    pub fn stats_when(
        &self,
        what: &str,
        cond: impl Fn(&PublishMountStats) -> bool,
    ) -> PublishMountStats {
        let give_up = Instant::now() + scaled(Duration::from_secs(5));
        loop {
            let stats = self.mount.stats();
            if cond(&stats) {
                return stats;
            }
            assert!(
                Instant::now() < give_up,
                "never happened: {what}: {stats:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait until the mount has no publisher; fails after five seconds.
    pub fn until_no_publisher(&self) {
        let give_up = Instant::now() + scaled(Duration::from_secs(5));
        while self.mount.publisher().is_some() {
            assert!(Instant::now() < give_up, "the mount kept its publisher");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.server.stop();
    }
}

/// The first `len` bytes the mount's receiver yields.
pub fn read_ts(app: &mut RtpRecvTransport, len: usize) -> Vec<u8> {
    let mut got = Vec::with_capacity(len);
    let mut buf = vec![0u8; 65536];
    while got.len() < len {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("after {} of {len} bytes: {e:?}", got.len()));
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// Everything the mount's receiver yields until it ends, as it does on a
/// mount that ends with its publisher.
pub fn read_ts_to_end(app: &mut RtpRecvTransport) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        match app.recv_bytes(&mut buf) {
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(TransportError::Closed) => return got,
            Err(e) => panic!("after {} bytes: {e:?}", got.len()),
        }
    }
}

/// Every event the mount's receiver yields until it ends, read through a
/// demuxer. The receiver's own bound is unscaled two seconds: it only ends
/// a read that the end of the mount should have ended.
pub fn demux_to_end(app: RtpRecvTransport) -> Vec<DemuxEvent> {
    let mut app = app;
    app.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut rx = DemuxReceiver::new(app);
    demux_until_quiet(
        &mut rx,
        Instant::now() + scaled(Duration::from_secs(20)),
        "the mount",
    )
}

/// `n` whole TS packets with a sync byte each, filled with `fill`.
pub fn ts_packets(n: usize, fill: u8) -> Vec<u8> {
    let mut packet = [fill; 188];
    packet[0] = 0x47;
    packet.repeat(n)
}

pub fn sps() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(SPROP_SPS_B64)
        .unwrap()
}

pub fn pps() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(SPROP_PPS_B64)
        .unwrap()
}

/// The H.264 track the tests announce: a real SPS and PPS.
pub fn h264() -> VideoTrack {
    VideoTrack::H264 {
        parameter_sets: vec![sps(), pps()],
    }
}

pub fn elementary(klv: bool) -> PushDescription {
    PushDescription::Elementary { video: h264(), klv }
}

/// A NAL unit of `len` bytes behind `header`, no zero byte in it (so no
/// start code), told apart from others by `salt`.
pub fn nal(header: u8, len: usize, salt: usize) -> Vec<u8> {
    std::iter::once(header)
        .chain((1..len).map(|k| ((k + salt) % 251) as u8 + 1))
        .collect()
}

/// Access unit `i` in Annex B form, without parameter sets: an IDR slice
/// for the first, P slices of different sizes after it, each over one RTP
/// packet's worth for some.
pub fn access_unit(i: usize) -> Vec<u8> {
    let slice = if i == 0 {
        nal(0x65, 3000, i)
    } else {
        nal(0x41, 300 + (i * 397) % 2500, i)
    };
    [&[0, 0, 0, 1][..], &slice].concat()
}

pub fn pts(ticks: i64) -> Pts90khz {
    Pts90khz::new(ticks)
}

/// The NAL units of `annexb`, without access unit delimiters and parameter
/// sets (which the server adds).
pub fn slices(annexb: &[u8]) -> Vec<Vec<u8>> {
    nals(annexb)
        .filter(|n| !matches!(n.first().map(|h| h & 0x1F), Some(7..=9)))
        .map(<[u8]>::to_vec)
        .collect()
}

/// `got` is `want`, byte for byte: same length and same SHA-256.
pub fn assert_same_bytes(got: &[u8], want: &[u8], what: &str) {
    use sha2::{Digest as _, Sha256};
    let sha256 = |b: &[u8]| -> String {
        Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect()
    };
    assert!(!want.is_empty(), "{what}: nothing to compare");
    let (got_hash, want_hash) = (sha256(got), sha256(want));
    assert!(
        got.len() == want.len() && got_hash == want_hash,
        "{what}: {} bytes, SHA-256 {got_hash}; expected {} bytes, SHA-256 {want_hash}",
        got.len(),
        want.len()
    );
}

/// A client builder for the mount, through `relay`.
pub fn builder_via(relay: &TcpRelay) -> RtspClientBuilder {
    RtspClientBuilder::new(&format!("rtsp://127.0.0.1:{}{PATH}", relay.port())).unwrap()
}

/// What `send_ts` returns once the push has seen its end: the client's
/// thread records the end before it closes the media queue, so a send in
/// between is still taken. Fails after five seconds.
pub fn refused_send(push: &mut RtspPush) -> TransportError {
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    loop {
        if let Err(e) = push.send_ts(&ts_packets(7, 0x33)) {
            return e;
        }
        assert!(Instant::now() < give_up, "sends were still taken");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Wait until the push has recorded why it ended; fails after five seconds.
pub fn end_reason_of(push: &RtspPush) -> EndReason {
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    loop {
        if let Some(reason) = push.end_reason() {
            return reason;
        }
        assert!(Instant::now() < give_up, "the push never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
}
