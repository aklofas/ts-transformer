//! Raw-socket RTSP *publisher* (ANNOUNCE / SETUP `mode=record` / RECORD)
//! for the server's publisher-role tests. `RtspClient` only plays, so the
//! publish side is driven by hand: blocking std sockets, every read bounded
//! by a 2 s read timeout, the control exchange through
//! [`super::raw_rtsp::request`].
//!
//! The SETUP helpers return what the server's `Transport:` header says
//! (the interleaved channel pair it granted, its UDP server ports) rather
//! than assuming them, so the tests check what the server answered.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::Muxer;
use tst_core::transport::RecvTransport;
use tst_pipeline::{DemuxReceiver, ShellErrorKind};

use tst_core::klv::st0601::{self, UasDatalinkLs};
use tst_rtp::SenderReport;

use super::h264_payloader::{build_rtp_packet, packetize};
use super::raw_rtsp::{header, make_muxer_cfg, request, session_id};

/// A single PT 33 (MP2T/90000, RFC 2250) track, `a=control:streamid=0`.
pub const SDP_MP2T: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=publish\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\na=control:streamid=0\r\n";

/// The SPS/PPS pair ffmpeg announced in the spec's appendix A capture
/// (`sprop-parameter-sets`), shared by [`SDP_H264`] and [`SDP_H264_KLV`].
pub const SPROP_SPS_B64: &str = "Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq";
pub const SPROP_PPS_B64: &str = "aM4PGSA=";

/// ffmpeg's RTSP-muxer shape (spec appendix A, `-rtsp_transport tcp|udp`):
/// one H.264 track, dynamic PT 96, parameter sets out of band in
/// `sprop-parameter-sets`, `a=control:streamid=0`.
pub const SDP_H264: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=No Name\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\na=tool:libavformat 60.16.100\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq,aM4PGSA=; profile-level-id=F4000C\r\na=control:streamid=0\r\n";

/// GStreamer 1.24.2 `rtspclientsink` capture of an elementary H.264 + KLV
/// push (`tsdemux` → `h264parse` / `meta/x-klv` → `rtspclientsink`). The
/// KLV track is announced FIRST: RFC 6597 `SMPTE336M/90000` (upper case)
/// on dynamic PT 96 under `a=control:stream=1`, then H.264 on PT 99 under
/// `a=control:stream=0`. GStreamer SETs UP the tracks in that order.
/// Verbatim except `sprop-parameter-sets`: the capture carried the PPS only
/// (the SPS travelled in band); this carries [`SPROP_SPS_B64`] and
/// [`SPROP_PPS_B64`] so the out-of-band parameter sets still parse.
pub const SDP_H264_KLV: &str = "v=0\r\no=- 2559276897 1 IN IP4 127.0.0.1\r\ns=Session streamed with GStreamer\r\ni=rtspclientsink\r\nt=0 0\r\na=tool:GStreamer\r\nm=application 0 RTP/AVP 96\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 SMPTE336M/90000\r\na=control:stream=1\r\na=ts-refclk:local\r\na=mediaclk:sender\r\na=ssrc:2807290026 cname:user4152540281@host-d1ba556f\r\nm=video 0 RTP/AVP 99\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:99 H264/90000\r\na=fmtp:99 packetization-mode=1;sprop-parameter-sets=Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq,aM4PGSA=\r\na=control:stream=0\r\na=ts-refclk:local\r\na=mediaclk:sender\r\na=ssrc:2996025368 cname:user4152540281@host-d1ba556f\r\n";

/// Plain TCP or (with `tls`) a rustls client stream over TCP.
enum Stream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(t) => t,
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.read(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.write(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(t) => t.flush(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// One publisher connection.
pub struct RawPublisher {
    stream: Stream,
    port: u16,
    scheme: &'static str,
    session: Option<String>,
    cseq: u32,
}

fn status_of(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"))
}

fn tcp_to(port: u16) -> TcpStream {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("publisher connects");
    tcp.set_nodelay(true).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    tcp
}

impl RawPublisher {
    /// Plain `rtsp://` publisher, 2 s read timeout.
    pub fn connect(port: u16) -> Self {
        Self {
            stream: Stream::Plain(tcp_to(port)),
            port,
            scheme: "rtsp",
            session: None,
            cseq: 0,
        }
    }

    /// `rtsps://` publisher trusting `roots`. The handshake runs lazily on
    /// the first write (rustls `StreamOwned`), bounded by the same 2 s read
    /// timeout.
    #[cfg(feature = "tls")]
    pub fn connect_tls(port: u16, roots: rustls::RootCertStore) -> Self {
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name)
            .expect("rustls client connection");
        Self {
            stream: Stream::Tls(Box::new(rustls::StreamOwned::new(conn, tcp_to(port)))),
            port,
            scheme: "rtsps",
            session: None,
            cseq: 0,
        }
    }

    fn uri(&self, path: &str) -> String {
        format!("{}://127.0.0.1:{}{path}", self.scheme, self.port)
    }

    /// Send `method uri` with `extra` header lines (each ending `\r\n`) and
    /// `body`; returns the response head.
    fn exchange(&mut self, method: &str, path: &str, extra: &str, body: &str) -> String {
        self.cseq += 1;
        let session = match &self.session {
            Some(s) => format!("Session: {s}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "{method} {} RTSP/1.0\r\nCSeq: {}\r\n{session}{extra}\r\n{body}",
            self.uri(path),
            self.cseq
        );
        request(&mut self.stream, &req)
    }

    /// ANNOUNCE `sdp` into `mount`; returns the status code.
    pub fn announce(&mut self, mount: &str, sdp: &str) -> u16 {
        let extra = format!(
            "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
            sdp.len()
        );
        status_of(&self.exchange("ANNOUNCE", mount, &extra, sdp))
    }

    /// SETUP `mode=record` over TCP-interleaved. Returns the status and,
    /// on 200, the RTP channel the SERVER allocated (its `interleaved=`).
    pub fn setup_interleaved(&mut self, mount: &str, control: &str) -> (u16, Option<u8>) {
        let (status, pair) = self.setup_interleaved_pair(mount, control);
        (status, pair.map(|(rtp, _)| rtp))
    }

    /// [`Self::setup_interleaved`] returning the whole `(rtp, rtcp)`
    /// channel pair the server allocated. Every SETUP asks for `0-1`; a
    /// second track's SETUP on the same session gets whatever pair the
    /// server picks instead, which is why callers read it back.
    pub fn setup_interleaved_pair(
        &mut self,
        mount: &str,
        control: &str,
    ) -> (u16, Option<(u8, u8)>) {
        let r = self.exchange(
            "SETUP",
            &format!("{mount}/{control}"),
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n",
            "",
        );
        let status = status_of(&r);
        if status != 200 {
            return (status, None);
        }
        self.session = Some(session_id(&r));
        let pair = transport_param(header(&r, "Transport"), "interleaved").and_then(|v| {
            let (a, b) = v.split_once('-')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        });
        (status, pair)
    }

    /// SETUP `mode=record` over UDP announcing `client_port`. Returns the
    /// status and, on 200, the server's RTP/RTCP port pair.
    pub fn setup_udp(
        &mut self,
        mount: &str,
        control: &str,
        client_port: u16,
    ) -> (u16, Option<(u16, u16)>) {
        let extra = format!(
            "Transport: RTP/AVP;unicast;client_port={}-{};mode=record\r\n",
            client_port,
            // A kernel-picked port can be 65535; never overflow in a test.
            client_port.saturating_add(1)
        );
        let r = self.exchange("SETUP", &format!("{mount}/{control}"), &extra, "");
        let status = status_of(&r);
        if status != 200 {
            return (status, None);
        }
        self.session = Some(session_id(&r));
        let ports = transport_param(header(&r, "Transport"), "server_port").and_then(|v| {
            let (a, b) = v.split_once('-')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        });
        (status, ports)
    }

    /// RECORD; returns the status code.
    pub fn record(&mut self, mount: &str) -> u16 {
        status_of(&self.exchange("RECORD", mount, "", ""))
    }

    /// RECORD with `Range: npt=0.000-`, the way ffmpeg sends it (spec
    /// appendix A); returns the status code.
    pub fn record_from_start(&mut self, mount: &str) -> u16 {
        status_of(&self.exchange("RECORD", mount, "Range: npt=0.000-\r\n", ""))
    }

    /// TEARDOWN; returns the status code.
    pub fn teardown(&mut self, mount: &str) -> u16 {
        status_of(&self.exchange("TEARDOWN", mount, "", ""))
    }

    /// One RFC 2326 §10.12 interleaved frame: `$`, channel, BE16 length, payload.
    pub fn send_frame(&mut self, ch: u8, payload: &[u8]) {
        let len = u16::try_from(payload.len()).expect("frame fits u16");
        let mut f = Vec::with_capacity(4 + payload.len());
        f.push(b'$');
        f.push(ch);
        f.extend_from_slice(&len.to_be_bytes());
        f.extend_from_slice(payload);
        self.stream.write_all(&f).expect("frame written");
        self.stream.flush().expect("frame flushed");
    }

    /// The session id from the first SETUP.
    pub fn session_id(&self) -> &str {
        self.session.as_deref().expect("SETUP ran")
    }

    /// Read whatever the server pushes until `needle` appears, EOF, or
    /// `deadline`. Returns `(bytes, saw_eof)`.
    pub fn read_until(&mut self, needle: &[u8], deadline: Instant) -> (Vec<u8>, bool) {
        self.stream
            .tcp()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        while Instant::now() < deadline {
            match self.stream.read(&mut chunk) {
                Ok(0) => return (buf, true),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(needle.len()).any(|w| w == needle) {
                        return (buf, false);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return (buf, true),
            }
        }
        (buf, false)
    }
}

/// `name=value` out of a `Transport:` header value.
fn transport_param<'a>(transport: &'a str, name: &str) -> Option<&'a str> {
    transport.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// A 12-byte RTP header (V=2, no marker) followed by `payload`.
pub fn rtp_wrap(seq: u16, ts: u32, ssrc: u32, pt: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + payload.len());
    v.push(0x80);
    v.push(pt & 0x7F);
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&ts.to_be_bytes());
    v.extend_from_slice(&ssrc.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// `n` TS bundles of at most 7×188 bytes, pulled from a real `Muxer` built
/// from [`make_muxer_cfg`] fed synthetic 300-byte H.264 AUs (an IDR, then
/// P slices) at PTS steps of 3003. A bundle pulled at the end of a push
/// may be shorter than 1316 bytes; every bundle is a whole number of TS
/// packets.
pub fn ts_fixture_packets(n: usize) -> Vec<Vec<u8>> {
    let mut mux = Muxer::new(make_muxer_cfg()).expect("muxer");
    let mut out = Vec::new();
    let mut buf = [0u8; 7 * 188];
    let mut i: i64 = 0;
    while out.len() < n {
        let nal_type = if i == 0 { 0x65 } else { 0x41 };
        let mut au = vec![0u8, 0, 0, 1, nal_type];
        au.extend((0..295).map(|k| (k as u8).wrapping_add(i as u8) | 1));
        mux.push_video(&au, Pts90khz::new(i * 3003), i == 0)
            .expect("push AU");
        loop {
            let got = mux.pull(&mut buf);
            if got == 0 || out.len() == n {
                break;
            }
            out.push(buf[..got].to_vec());
        }
        i += 1;
    }
    out
}

/// RTP timestamp step between the synthetic AUs of [`h264_rtp_packets`]
/// (29.97 fps at 90 kHz).
pub const H264_AU_STEP: u32 = 3003;

/// Size of the synthetic IDR NALU in [`h264_rtp_packets`]: over the
/// 1400-byte payload budget, so the IDR travels as FU-A fragments.
pub const H264_IDR_LEN: usize = 3000;

/// Size of each synthetic P-slice NALU in [`h264_rtp_packets`].
pub const H264_P_LEN: usize = 300;

/// A synthetic NALU: `header`, then `len - 1` non-zero body bytes (no zero
/// byte, so no start-code emulation once Annex-B framed).
fn synthetic_nalu(header: u8, len: usize, salt: usize) -> Vec<u8> {
    let mut n = vec![header];
    n.extend((1..len).map(|i| ((i + salt) % 251) as u8 | 1));
    n
}

/// `n` H.264 access units as RFC 6184 RTP packets through the shared test
/// payloader (MTU 1400): AU 0 an IDR slice (`0x65`, [`H264_IDR_LEN`]
/// bytes, so FU-A fragmented), then P slices (`0x41`, [`H264_P_LEN`]
/// bytes, single-NALU packets). AU `i` carries RTP timestamp
/// `ts0 + i × H264_AU_STEP`; the last packet of each AU has the marker
/// bit. No in-band SPS/PPS: they travel in the SDP's
/// `sprop-parameter-sets`, as ffmpeg sends them.
pub fn h264_rtp_packets(n: usize, pt: u8, seq0: u16, ssrc: u32, ts0: u32) -> Vec<Vec<u8>> {
    let aus: Vec<(u32, Vec<Vec<u8>>)> = (0..n)
        .map(|i| {
            let ts = ts0.wrapping_add(i as u32 * H264_AU_STEP);
            let nalu = if i == 0 {
                synthetic_nalu(0x65, H264_IDR_LEN, i)
            } else {
                synthetic_nalu(0x41, H264_P_LEN, i)
            };
            (ts, vec![nalu])
        })
        .collect();
    packetize(&aus, 1400, seq0, ssrc, pt)
}

/// One RFC 6597 KLV RTP packet carrying a whole KLV unit, marker set (the
/// marker closes the unit).
pub fn klv_rtp_packet(seq: u16, ts: u32, ssrc: u32, pt: u8, bytes: &[u8]) -> Vec<u8> {
    build_rtp_packet(seq, ts, ssrc, pt, true, bytes)
}

/// A real ST 0601 local set (UL, BER length, TLVs, checksum) from the
/// tst-core encoder; `seq` varies the timestamp and heading so units are
/// told apart.
pub fn klv_unit(seq: u32) -> Vec<u8> {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000 + u64::from(seq) * 100_000),
        platform_heading_deg: Some(f64::from(seq % 360)),
        sensor_lat_deg: Some(38.5 + f64::from(seq) * 1e-4),
        sensor_lon_deg: Some(-121.5 - f64::from(seq) * 1e-4),
        ..Default::default()
    };
    st0601::encode_to_vec(&record).expect("ST 0601 encodes")
}

/// A 28-byte RTCP sender report (PT 200, no report blocks): NTP
/// `ntp_secs.ntp_frac` (32.32) ↔ RTP timestamp `rtp`.
pub fn sr_packet(ssrc: u32, ntp_secs: u32, ntp_frac: u32, rtp: u32) -> Vec<u8> {
    SenderReport {
        ssrc,
        ntp_timestamp: (u64::from(ntp_secs) << 32) | u64::from(ntp_frac),
        rtp_timestamp: rtp,
        sender_packet_count: 0,
        sender_octet_count: 0,
        report_blocks: vec![],
    }
    .encode()
    .expect("SR encodes")
}

/// Every event `rx` yields until its transport has been quiet for one
/// whole recv timeout (the caller sets it with `set_recv_timeout`; the
/// shells report the expiry as `Backpressure`), the stream ends, or
/// `deadline` passes. Call it after the publisher has sent everything:
/// quiet then means "all of it has been demuxed". The demuxer holds a
/// PID's last PES until the next one starts, so the newest sample of an
/// unbounded video PES may not be among the events.
pub fn demux_until_quiet<R: RecvTransport>(
    rx: &mut DemuxReceiver<R>,
    deadline: Instant,
    who: &str,
) -> Vec<DemuxEvent> {
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_event() {
            Ok(Some(ev)) => events.push(ev),
            Ok(None) => break,
            Err(e) if e.kind == ShellErrorKind::Backpressure => break,
            Err(e) => panic!("{who}: demux failed after {} events: {e:?}", events.len()),
        }
    }
    events
}
