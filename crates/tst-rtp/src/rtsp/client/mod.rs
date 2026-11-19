//! `RtspClient` sync facade.
//!
//! The control connection (plain TCP for `rtsp://`, rustls over TCP for
//! `rtsps://`) belongs to one thread, `tst-rtsp-client` (see `driver`),
//! which reads it all the time, writes requests and keepalive pings, and
//! hands interleaved media to the receiver. The request methods run on
//! the caller's thread: they hand a request to that thread and wait for
//! its reply.

pub mod builder;
pub(crate) mod driver;
pub(crate) mod engine;
pub(crate) mod media_queue;
pub mod options_describe;
pub mod play;
pub mod push;
pub mod session;
pub(crate) mod setup;
pub(crate) mod stream;
pub(crate) mod teardown;
#[cfg(all(test, target_os = "linux"))]
mod test_sockets;
pub(crate) mod tls;

use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::error::RtspError;
use crate::url::{RtspScheme, RtspUrl};
use driver::{Command, DriverHandle};
use engine::{ClientEngine, Credentials, EngineConfig};
use secrecy::SecretString;
use tst_core::cancel::FlagCancel;
use tst_core::transport::{EndReason, EndReasonHandle};

/// Queued control-connection bytes the peer takes none of for this long
/// end the stream (`Io(TimedOut)`); also the write timeout of each socket
/// write of the `rtsps://` handshake.
pub(crate) const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest a request's reply wait sleeps before it re-checks the cancel
/// flag and the request deadline.
pub(crate) const REQUEST_POLL_TICK: Duration = Duration::from_millis(100);

/// How long [`RtspClient::close`] waits for the client's thread to end.
const CLOSE_JOIN_BOUND: Duration = Duration::from_secs(2);

/// The bound [`RtspClient::close`] and `Drop` give their TEARDOWN.
const CLOSE_TEARDOWN_BOUND: Duration = Duration::from_millis(500);

/// Sync RTSP client. One instance per server.
///
/// Construct via [`Self::connect`] / [`Self::connect_with`], then drive
/// the session: `options` / `describe` / `setup_mp2t_auto` (or the
/// explicit `setup*` variants) / `play` / `pause` / `teardown`.
/// To publish instead, call [`Self::announce`], [`Self::setup_record`]
/// and [`Self::record`], which turns the client into the
/// [`RtspPush`](push::RtspPush) that sends.
/// End it with [`Self::close`]: with a session open it writes TEARDOWN
/// and waits for the reply (500 ms at most), then waits for the client's
/// thread (2 s at most). Dropping the client sets its cancel, writes
/// TEARDOWN without waiting for the reply, and tells the thread to stop
/// without waiting for it.
///
/// Keepalive pings, when [`RtspClientBuilder`](crate::RtspClientBuilder)
/// enables them (its default), are written by the client's own thread at
/// half the session timeout, retuned to the one SETUP learns.
///
/// # Send
///
/// This type is `Send`: moving it to a dedicated receive/watchdog thread
/// is a supported, documented use — a regression here is a breaking
/// change.
#[derive(Debug)]
pub struct RtspClient {
    /// Negotiated URL — caller can re-parse for re-connects.
    pub(crate) url: RtspUrl,
    /// Session ID from the most recent SETUP success. None before
    /// SETUP / after TEARDOWN.
    pub(crate) session_id: Option<String>,
    /// Cancel flag — set by `RtspCancelHandle::cancel` to end a request's
    /// reply wait.
    pub(crate) cancel: FlagCancel,
    /// Per-request response deadline
    /// ([`crate::RtspClientBuilder::request_timeout`]); `None` = unbounded.
    pub(crate) request_timeout: Option<Duration>,
    /// Structured record of why the session ended, first writer wins,
    /// written by the client's thread, and with `Cancelled` by `Drop` and
    /// by a TEARDOWN sent on a cancelled client. Cloned into
    /// [`session::RtspSession`] so the receiver reports the same reason.
    pub(crate) end_reason: EndReasonHandle,
    /// Set by the client's thread when the session is known dead: the
    /// control connection ended, or a keepalive ping was answered `454` or
    /// could not be encoded. The end reason is recorded first. Also set
    /// when the thread itself ends, however it ends.
    pub(crate) session_dead: Arc<AtomicBool>,
    /// The client's I/O thread.
    pub(crate) driver: DriverHandle,
    /// The control socket's local address, which an announced description
    /// names as its origin; `None` when the socket did not tell.
    pub(crate) local_ip: Option<IpAddr>,
    /// The address the control socket is connected to.
    pub(crate) peer_ip: IpAddr,
    /// Frames the peer sent on the RTP channel of a track this side sends
    /// on, counted by the client's thread.
    pub(crate) frames_dropped_inbound: Arc<AtomicU64>,
    /// The tracks of an accepted ANNOUNCE, until RECORD hands them to the
    /// [`push::RtspPush`].
    pub(crate) push: Option<push::Announced>,
}

/// `WWW-Authenticate` challenge cache + `qop=auth` nonce-count pair.
///
/// Never split them: an `nc` allocation must pair with the challenge
/// (nonce) it is sent with, or a challenge rotation could repeat an `nc`
/// for a reused nonce, which a `qop=auth` server rejects as a replay
/// (RFC 7616 §3.4).
#[derive(Debug, Default)]
pub(crate) struct AuthState {
    /// Cached challenge from the most recent 401.
    pub(crate) challenge: Option<String>,
    /// Monotonic nonce-count for the CURRENT challenge; reset to 0
    /// when a *different* challenge is cached (a new nonce restarts
    /// the count at 1).
    pub(crate) nc: u32,
    /// Which challenge schemes this client may answer.
    pub(crate) schemes: crate::rtsp::auth::AuthSchemes,
}

impl AuthState {
    /// Cache `www_auth` as the current challenge, resetting the
    /// nonce-count when the challenge changed (a new nonce restarts the
    /// count at 1 — RFC 7616 §3.4).
    pub(crate) fn cache_challenge(&mut self, www_auth: String) {
        if self.challenge.as_deref() != Some(www_auth.as_str()) {
            self.nc = 0;
        }
        self.challenge = Some(www_auth);
    }
}

/// Millisecond count of `d`, saturating at `u64::MAX`. A plain
/// `as_millis() as u64` cast silently truncates for durations past
/// ~584 million years — a hostile-but-spec-legal `Session: timeout=`
/// near `u64::MAX` seconds would wrap the derived keepalive cadence
/// into a tiny value and turn the pinger into a hot loop.
pub(crate) fn duration_ms_saturating(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Feed `data` as bytes read from the control connection, once in 7-byte
/// slices and once whole, to each of three fresh client engines:
///
/// - a pull client with interleaved channels 0/1 installed, one OPTIONS
///   (CSeq 1) waiting for its reply and one keepalive ping (CSeq 2)
///   waiting for its own;
/// - a pull client with a SETUP (CSeq 1) waiting for the reply that
///   installs its route, and a ping (CSeq 2): a `200` with an interleaved
///   `Transport` and a `Session` installs it, and frames behind the reply
///   are media;
/// - a publish client recording two tracks (channels 0/1 and 2/3, session
///   `fuzz`) after scripted `200` replies to ANNOUNCE, two SETUPs and
///   RECORD (CSeq 1 to 4), with an OPTIONS (CSeq 5) and a ping (CSeq 6)
///   waiting.
///
/// Besides not panicking, the engines keep three rules: media comes only
/// from a route a SETUP reply or the setup installed, a publish client
/// hands out no media, and it stays recording, with media allowed, until
/// its session or stream ends.
//
// `#[doc(hidden)] pub` for the fuzz workspace's `rtsp_client_pump_framing`
// target. Not part of the crate's stable public API.
#[doc(hidden)]
pub fn fuzz_client_engine_input(data: &[u8]) {
    let start = std::time::Instant::now();
    let now = start
        .checked_add(fuzz_engines::PING_INTERVAL)
        .unwrap_or(start);
    for kind in fuzz_engines::KINDS {
        let mut sliced = kind.build(start, now);
        let mut watch = fuzz_engines::Watch::default();
        for chunk in data.chunks(7) {
            sliced.handle_input(chunk, now);
            watch.drain(kind, &mut sliced);
        }
        let mut whole = kind.build(start, now);
        whole.handle_input(data, now);
        fuzz_engines::Watch::default().drain(kind, &mut whole);
    }
}

/// The engines [`fuzz_client_engine_input`] feeds, and the rules it checks.
mod fuzz_engines {
    use std::time::{Duration, Instant};

    use crate::rtsp::message::RtspMethod;

    use super::engine::{
        ClientEngine, EngineConfig, Event, PushPhase, ReplyOutcome, RequestSpec, RouteId,
        RouteKind, SetupRoute,
    };

    pub(super) const PING_INTERVAL: Duration = Duration::from_millis(100);

    const URI: &str = "rtsp://fuzz.invalid/live";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Kind {
        OptionsWaiting,
        SetupWaiting,
        Recording,
    }

    pub(super) const KINDS: [Kind; 3] = [Kind::OptionsWaiting, Kind::SetupWaiting, Kind::Recording];

    /// The replies that take a publish client to recording, in order, for
    /// the requests of [`record_requests`]: CSeq 1 to 4.
    const RECORD_REPLIES: [&[u8]; 4] = [
        b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nSession: fuzz;timeout=60\r\n\
          Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: fuzz;timeout=60\r\n\
          Transport: RTP/AVP/TCP;unicast;interleaved=2-3;mode=record\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: fuzz\r\n\r\n",
    ];

    fn request(method: RtspMethod, headers: Vec<(&'static str, String)>) -> RequestSpec {
        RequestSpec {
            method,
            uri: URI.to_string(),
            headers,
            body: None,
            announce_tracks: None,
            setup: None,
        }
    }

    /// A SETUP asking for `channels` interleaved, installed under `route`.
    fn setup(route: SetupRoute, channels: &str) -> RequestSpec {
        let mode = if route.kind == RouteKind::Push {
            ";mode=record"
        } else {
            ""
        };
        RequestSpec {
            setup: Some(route),
            ..request(
                RtspMethod::Setup,
                vec![(
                    "transport",
                    format!("RTP/AVP/TCP;unicast;interleaved={channels}{mode}"),
                )],
            )
        }
    }

    /// ANNOUNCE of two tracks, a recording SETUP of each, RECORD.
    fn record_requests() -> [RequestSpec; 4] {
        let push = |id| SetupRoute {
            id: RouteId(id),
            kind: RouteKind::Push,
        };
        [
            RequestSpec {
                announce_tracks: Some(2),
                ..request(RtspMethod::Announce, Vec::new())
            },
            setup(push(0), "0-1"),
            setup(push(1), "2-3"),
            request(RtspMethod::Record, vec![("session", "fuzz".to_string())]),
        ]
    }

    impl Kind {
        pub(super) fn build(self, start: Instant, now: Instant) -> ClientEngine {
            let mut e = ClientEngine::new(EngineConfig {
                uri: URI.to_string(),
                version: crate::url::RtspVersion::V1_0,
                user_agent: "tst-rtp-fuzz".to_string(),
                credentials: None,
                schemes: crate::rtsp::auth::AuthSchemes::default(),
            });
            let pull = SetupRoute {
                id: RouteId(0),
                kind: RouteKind::Pull,
            };
            let waiting = match self {
                Kind::OptionsWaiting => {
                    let _ = e.install_route(pull, 0, 1);
                    request(RtspMethod::Options, Vec::new())
                }
                Kind::SetupWaiting => setup(pull, "0-1"),
                Kind::Recording => {
                    for (spec, reply) in record_requests().into_iter().zip(RECORD_REPLIES) {
                        let txn = e.submit(spec, None, start).expect("a publish request");
                        e.handle_input(reply, start);
                        let taken = std::iter::from_fn(|| e.poll_event()).any(
                            |ev| matches!(ev, Event::Reply { txn: t, result: Ok(_) } if t == txn),
                        );
                        assert!(taken, "a scripted publish reply was not taken: {e:?}");
                    }
                    while e.poll_transmit().is_some() {}
                    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
                    request(RtspMethod::Options, Vec::new())
                }
            };
            let _ = e.submit(waiting, None, start);
            e.enable_keepalive(Some(PING_INTERVAL), start);
            e.handle_timeout(now, false);
            e
        }
    }

    /// What one engine has handed out so far.
    #[derive(Default)]
    pub(super) struct Watch {
        /// A SETUP reply installed a route.
        installed: bool,
    }

    impl Watch {
        /// Take every event and transmit `e` holds, checking the rules.
        pub(super) fn drain(&mut self, kind: Kind, e: &mut ClientEngine) {
            while let Some(ev) = e.poll_event() {
                match ev {
                    Event::Reply {
                        result: Ok(ReplyOutcome { setup: Some(_), .. }),
                        ..
                    } => self.installed = true,
                    Event::Media { .. } => assert!(
                        match kind {
                            Kind::OptionsWaiting => true,
                            Kind::SetupWaiting => self.installed,
                            Kind::Recording => false,
                        },
                        "{kind:?}: media from a route that was not installed for pulling"
                    ),
                    _ => {}
                }
            }
            while e.poll_transmit().is_some() {}
            if kind == Kind::Recording {
                let phase = e.push_phase();
                assert!(
                    matches!(phase, Some(PushPhase::Recording | PushPhase::Ended)),
                    "a recording publish client moved to {phase:?}"
                );
                assert_eq!(e.media_allowed(), phase == Some(PushPhase::Recording));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::rtsp::client::engine::SetupAccepted;

        fn events(e: &mut ClientEngine) -> Vec<Event> {
            std::iter::from_fn(|| e.poll_event()).collect()
        }

        fn rtp_frame(channel: u8) -> Vec<u8> {
            let mut f = vec![b'$', channel, 0, 16];
            f.extend_from_slice(&[0x80, 33, 0, 1, 0, 0, 0, 9, 0, 0, 0, 7]);
            f.extend_from_slice(&[0x47, 1, 2, 3]);
            f
        }

        fn rtcp_frame(channel: u8) -> Vec<u8> {
            let mut f = vec![b'$', channel, 0, 8];
            f.extend_from_slice(&[0x80, 201, 0, 1, 0, 0, 0, 5]);
            f
        }

        #[test]
        fn the_recording_engine_records_with_two_push_routes() {
            let t = Instant::now();
            let mut e = Kind::Recording.build(t, t + PING_INTERVAL);
            assert_eq!(e.push_phase(), Some(PushPhase::Recording));
            assert!(e.media_allowed());
            // RTP on either route's RTP channel is dropped, RTCP on either
            // RTCP channel is the route's.
            let input = [rtp_frame(0), rtp_frame(2), rtcp_frame(1), rtcp_frame(3)].concat();
            e.handle_input(&input, t);
            let ev = events(&mut e);
            assert!(
                matches!(
                    ev.as_slice(),
                    [
                        Event::InboundDropped,
                        Event::InboundDropped,
                        Event::Rtcp {
                            route: RouteId(0),
                            ..
                        },
                        Event::Rtcp {
                            route: RouteId(1),
                            ..
                        },
                    ]
                ),
                "{ev:?}"
            );
        }

        #[test]
        fn the_setup_engine_installs_its_route_from_the_reply() {
            let t = Instant::now();
            let mut e = Kind::SetupWaiting.build(t, t + PING_INTERVAL);
            // Before the reply, a frame on channel 0 has no route.
            e.handle_input(&rtp_frame(0), t);
            assert!(events(&mut e).is_empty());
            let reply = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nSession: s1\r\n\
                Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n";
            e.handle_input(&[&reply[..], &rtp_frame(0)].concat(), t);
            let ev = events(&mut e);
            assert!(
                matches!(
                    ev.as_slice(),
                    [
                        Event::Reply {
                            result: Ok(ReplyOutcome {
                                setup: Some(SetupAccepted {
                                    route: RouteId(0),
                                    rtp_channel: 0,
                                    rtcp_channel: 1,
                                }),
                                ..
                            }),
                            ..
                        },
                        Event::Media {
                            route: RouteId(0),
                            ..
                        },
                    ]
                ),
                "{ev:?}"
            );
        }

        /// The committed seeds of `rtsp_client_pump_framing` run through
        /// the entry.
        #[test]
        fn the_fuzz_seeds_keep_the_rules() {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fuzz/seeds/rtsp_client_pump_framing");
            let mut seeds = 0;
            for entry in std::fs::read_dir(&dir).unwrap() {
                let data = std::fs::read(entry.unwrap().path()).unwrap();
                crate::rtsp::client::fuzz_client_engine_input(&data);
                seeds += 1;
            }
            assert!(seeds > 0, "no seed in {}", dir.display());
        }
    }
}

/// Cancel handle for the RTSP client. Covers the control plane; the
/// transport plane (post-PLAY RTP data) uses its own
/// [`crate::RtpCancelHandle`] returned from the
/// [`crate::RtpRecvTransport`].
/// Obtain it from the owner's `cancel_handle()`; a handle built with
/// `new()` or `Default` is read by nothing.
pub type RtspCancelHandle = tst_core::cancel::FlagCancel;

/// Per-connection knobs carried from [`RtspClientBuilder`](crate::RtspClientBuilder)
/// into the connect path. Internal — the public `connect`/`connect_with`/
/// `connect_with_roots` entry points use [`ConnectParams::default`].
pub(crate) struct ConnectParams {
    /// TCP connect timeout.
    pub(crate) connect_timeout: Duration,
    /// Socket read timeout during the `rtsps://` handshake (each read is
    /// bounded by it or the time left on the connect budget).
    pub(crate) read_timeout: Duration,
    /// Per-request response deadline (`RtspClientBuilder::request_timeout`);
    /// `None` = unbounded.
    pub(crate) request_timeout: Option<Duration>,
    /// `User-Agent:` header value sent on every outbound request.
    pub(crate) user_agent: String,
    /// Which `WWW-Authenticate` schemes the client may answer.
    pub(crate) auth_schemes: crate::rtsp::auth::AuthSchemes,
    /// Gets a clone of the control socket before the client's thread
    /// takes it, so a test can read or set its options.
    #[cfg(test)]
    pub(crate) socket_probe: Option<std::sync::mpsc::Sender<TcpStream>>,
    /// Handed to the client's thread: see `driver::Shared::exit_gate`.
    #[cfg(test)]
    pub(crate) exit_gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

impl Default for ConnectParams {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_millis(100),
            request_timeout: Some(Duration::from_secs(10)),
            user_agent: "tst-rtp/0.1".into(),
            auth_schemes: crate::rtsp::auth::AuthSchemes::default(),
            #[cfg(test)]
            socket_probe: None,
            #[cfg(test)]
            exit_gate: None,
        }
    }
}

impl RtspClient {
    /// Connect to an `rtsp://` or `rtsps://` URL. The `rtsps://` scheme
    /// requires the `tls` cargo feature; otherwise it returns
    /// [`RtspError::Tls`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Url`] if the URL cannot be parsed.
    /// - [`RtspError::Io`] on socket-level failure (DNS, refused, etc.).
    /// - [`RtspError::Tls`] if the URL scheme is `rtsps://` and the
    ///   `tls` cargo feature is not enabled, or on rustls handshake
    ///   failure (server name validation, untrusted cert, etc.).
    pub fn connect(url: &str) -> Result<Self, RtspError> {
        let parsed = RtspUrl::parse(url)?;
        Self::connect_with(&parsed)
    }

    /// Connect using an already-parsed URL.
    ///
    /// `rtsps://` URLs are not supported by this entry point on a build
    /// without the `tls` cargo feature; they return
    /// [`RtspError::Tls`] in that case.
    ///
    /// # Errors
    ///
    /// See [`Self::connect`].
    pub fn connect_with(url: &RtspUrl) -> Result<Self, RtspError> {
        Self::connect_with_params(url, None, ConnectParams::default())
    }

    /// Connect with an optional client-side TLS root-cert store.
    ///
    /// `roots = None` falls back to the platform native trust roots
    /// (loaded via `rustls-native-certs`; the first load that yields a
    /// certificate is kept for the process, an empty one is retried).
    /// `roots = Some(custom)` is used by `RtspClientBuilder::tls_root_certs`
    /// callers that need to trust a self-signed cert (e.g., test fixtures).
    ///
    /// For plain `rtsp://` URLs the roots argument is ignored.
    ///
    /// Uses the default connect/read timeouts and User-Agent. To
    /// customize those, use [`RtspClientBuilder`](crate::RtspClientBuilder).
    ///
    /// # Errors
    ///
    /// See [`Self::connect`].
    pub fn connect_with_roots(
        url: &RtspUrl,
        #[cfg(feature = "tls")] roots: Option<rustls::RootCertStore>,
        #[cfg(not(feature = "tls"))] roots: Option<()>,
    ) -> Result<Self, RtspError> {
        Self::connect_with_params(url, roots, ConnectParams::default())
    }

    /// Connect with explicit per-connection parameters (timeouts +
    /// User-Agent). Internal: callers use [`Self::connect_with_roots`]
    /// for defaults or [`RtspClientBuilder`](crate::RtspClientBuilder) to
    /// override `params`. Holds the real connect logic that the public
    /// entry points delegate to.
    pub(crate) fn connect_with_params(
        url: &RtspUrl,
        #[cfg(feature = "tls")] roots: Option<rustls::RootCertStore>,
        #[cfg(not(feature = "tls"))] roots: Option<()>,
        params: ConnectParams,
    ) -> Result<Self, RtspError> {
        let _ = &roots; // silence unused on non-tls builds
        let is_tls = matches!(url.scheme(), RtspScheme::Rtsps);
        #[cfg(not(feature = "tls"))]
        if is_tls {
            return Err(RtspError::Tls(
                "TLS support requires the 'tls' cargo feature".into(),
            ));
        }

        let host_port = (url.host.as_str(), url.port);
        let mut addrs = host_port
            .to_socket_addrs()
            .map_err(|e| RtspError::Io(e.kind()))?;
        let peer = addrs
            .next()
            .ok_or(RtspError::Io(std::io::ErrorKind::AddrNotAvailable))?;
        // The OS trust store is read before the budget starts (kept for the
        // process once a load yields a certificate), so a slow read cannot
        // use up the handshake's time.
        #[cfg(feature = "tls")]
        if is_tls && roots.is_none() {
            let _ = tls::native_roots();
        }
        // One budget for the TCP connect and the TLS handshake together,
        // starting after name resolution: the connect gets all of it (the
        // deadline was just taken) and the handshake what is left.
        // `None` (the budget overflowed `Instant`) means unbounded.
        #[cfg(feature = "tls")]
        let deadline = std::time::Instant::now().checked_add(params.connect_timeout);
        let tcp = TcpStream::connect_timeout(&peer, params.connect_timeout)
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_read_timeout(Some(params.read_timeout))
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_write_timeout(Some(CONTROL_WRITE_TIMEOUT))
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_nodelay(true).ok();
        // An IPv4 address stays one when the socket holds it mapped into
        // IPv6.
        let local_ip = tcp.local_addr().ok().map(|a| a.ip().to_canonical());
        // `?tcp_keepalive=` / `RtspClientBuilder::tcp_keepalive`: applied
        // to the raw TCP socket before any TLS wrap so it covers rtsps://
        // too. Unlike nodelay above this propagates failure — the caller
        // explicitly asked for keepalive, so silently running without it
        // would defeat the dead-peer detection they configured.
        if let Some(idle) = url.tcp_keepalive {
            let ka = socket2::TcpKeepalive::new().with_time(idle);
            socket2::SockRef::from(&tcp)
                .set_tcp_keepalive(&ka)
                .map_err(|e| RtspError::Io(e.kind()))?;
        }
        #[cfg(test)]
        if let Some(probe) = &params.socket_probe {
            let _ = probe.send(tcp.try_clone().map_err(|e| RtspError::Io(e.kind()))?);
        }

        let stream = if is_tls {
            #[cfg(feature = "tls")]
            {
                stream::Stream::from_tls(tls::TlsStream::connect(
                    url,
                    tcp,
                    roots,
                    params.read_timeout,
                    deadline,
                )?)
            }
            #[cfg(not(feature = "tls"))]
            {
                // Unreachable: the early-return above already short-
                // circuited. Kept as a compile-time guard.
                unreachable!("tls feature disabled but rtsps:// reached connect path")
            }
        } else {
            stream::Stream::from_plain(tcp)
        }
        .map_err(|e| RtspError::Io(e.kind()))?;

        // A user name without a password answers a challenge with an empty
        // one (RFC 7617 allows it), so `u@h` and `u:@h` behave the same.
        let credentials = url
            .username
            .as_ref()
            .filter(|username| !username.is_empty())
            .map(|username| Credentials {
                username: username.clone(),
                password: url
                    .password
                    .clone()
                    .unwrap_or_else(|| SecretString::new("".into())),
            });
        let engine = ClientEngine::new(EngineConfig {
            uri: url.render_no_credentials(),
            version: url.rtsp_version,
            user_agent: params.user_agent,
            credentials,
            schemes: params.auth_schemes,
        });
        let cancel = FlagCancel::new();
        let session_dead = Arc::new(AtomicBool::new(false));
        let end_reason = EndReasonHandle::default();
        let frames_dropped_inbound = Arc::new(AtomicU64::new(0));
        let driver = driver::spawn(
            stream,
            engine,
            driver::Shared {
                cancel: cancel.clone(),
                session_dead: session_dead.clone(),
                end_reason: end_reason.clone(),
                media_dropped: Arc::new(AtomicU64::new(0)),
                frames_dropped_inbound: frames_dropped_inbound.clone(),
                #[cfg(test)]
                exit_gate: params.exit_gate,
            },
        )
        .map_err(|e| RtspError::Io(e.kind()))?;

        Ok(Self {
            url: url.clone(),
            session_id: None,
            cancel,
            request_timeout: params.request_timeout,
            end_reason,
            session_dead,
            driver,
            local_ip,
            peer_ip: peer.ip().to_canonical(),
            frames_dropped_inbound,
            push: None,
        })
    }

    /// Get a clone-able cancel handle.
    pub fn cancel_handle(&self) -> RtspCancelHandle {
        self.cancel.clone()
    }

    /// Start the OPTIONS keepalive pings on the client's thread.
    ///
    /// `override_interval` lets callers force a specific cadence
    /// (typically supplied by `RtspClientBuilder::keepalive_interval`);
    /// when `None`, the cadence starts at half the session timeout (60 s
    /// until SETUP learns one) and is retuned when SETUP learns the
    /// server-advertised timeout.
    ///
    /// Only the first call acts: a later one keeps the first cadence.
    //
    // Exposed `#[doc(hidden)] pub` so the integration test in
    // `tests/rtsp_client/keepalive.rs` can drive it without going
    // through `RtspClientBuilder`. The builder also calls this.
    ///
    /// # Errors
    ///
    /// Returns [`RtspError::Io`] if the client's thread has ended or its
    /// command queue is full.
    #[doc(hidden)]
    pub fn spawn_keepalive_if_needed(
        &mut self,
        override_interval: Option<Duration>,
    ) -> Result<(), RtspError> {
        self.driver
            .send(Command::EnableKeepalive(override_interval))
    }

    /// Returns false once the session is known dead: the control
    /// connection ended (the peer closed or reset it, a write made no
    /// progress, or the peer broke the message framing), or a keepalive
    /// ping was answered `454 Session Not Found` or could not be encoded.
    /// Returns true until then.
    pub fn is_session_alive(&self) -> bool {
        !self.session_dead.load(Ordering::Acquire)
    }

    /// End the session and the client. When a session is open, send
    /// TEARDOWN with one 500 ms bound over writing it and its reply
    /// ([`Self::teardown_within`]); then stop the client's thread and wait
    /// up to 2 s for it to end. When it returns, the thread has closed the
    /// control connection, unless the 2 s ran out: the thread is then left
    /// to end on its own.
    ///
    /// This does not set the cancel flag while it waits, so the reply is
    /// awaited; the `Drop` that runs as `close` returns sets it, so a cancel
    /// handle the caller kept reads cancelled afterwards. On a client whose
    /// cancel is already set, the TEARDOWN is still written, its reply is
    /// not awaited and the result is [`RtspError::LocalCancel`].
    ///
    /// The end reason it leaves (unless one was recorded before: first
    /// writer wins) is `CleanTeardown` when the server accepts the TEARDOWN
    /// (`2xx` or `454`) on a client whose cancel was not set; `Cancelled`
    /// when it refuses or does not answer it, or when the client's cancel
    /// was set before the TEARDOWN: that `Cancelled` is recorded before the
    /// TEARDOWN is written, so the server's answer cannot replace it.
    ///
    /// Dropping the client instead sets the cancel, writes TEARDOWN within
    /// the same 500 ms bound when a session is open, and tells the thread
    /// to stop without waiting for it. It records `Cancelled` before the
    /// TEARDOWN unless a reason was recorded before.
    ///
    /// # Errors
    ///
    /// The TEARDOWN's result, as [`Self::teardown_within`] reports it, so
    /// a TEARDOWN the server refuses is [`RtspError::Protocol`] with its
    /// status; `Ok(())` when no session is open. The thread is stopped on
    /// every outcome.
    pub fn close(self) -> Result<(), RtspError> {
        self.close_with(CLOSE_JOIN_BOUND)
    }

    /// [`Self::close`] with `join_bound` as the wait for the thread.
    fn close_with(mut self, join_bound: Duration) -> Result<(), RtspError> {
        let r = self.teardown_within(CLOSE_TEARDOWN_BOUND);
        self.driver.stop();
        let _ = self.driver.join_within(join_bound);
        // `Drop` runs next with the session already cleared: it writes no
        // second TEARDOWN.
        r
    }
}

impl Drop for RtspClient {
    // Drop does not join: the thread, told to stop, closes the socket and
    // ends on its own. `close` is the call that waits for it.
    fn drop(&mut self) {
        // The cancel first: with it set, the TEARDOWN below waits only for
        // its bytes to reach the kernel, not for the reply.
        self.cancel.cancel();
        // The reason before the TEARDOWN, so nothing the server does after
        // the drop (an answer, a close, garbage, a reset) can replace it.
        crate::transport::record_end(&self.end_reason, EndReason::Cancelled);
        if self.session_id.is_some() {
            let _ = self.teardown_within(CLOSE_TEARDOWN_BOUND);
        }
        self.driver.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    use tst_test_helpers::timing::scaled;

    #[test]
    fn connect_to_loopback_listener_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept in background so connect_with doesn't hang.
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let c = RtspClient::connect(&url).unwrap();
        assert!(matches!(c.url.scheme(), RtspScheme::Rtsp));
    }

    /// `?tcp_keepalive=` must actually reach the OS: SO_KEEPALIVE reads
    /// back enabled on the connected control socket, and stays at the OS
    /// default (off) when the knob is absent.
    #[test]
    fn tcp_keepalive_query_applies_to_control_socket() {
        let keepalive_state = |query: &str| -> bool {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let _ = listener.accept();
            });
            let url = RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test{query}")).unwrap();
            let (probe, socket) = std::sync::mpsc::channel();
            let _c = RtspClient::connect_with_params(
                &url,
                None,
                ConnectParams {
                    socket_probe: Some(probe),
                    ..Default::default()
                },
            )
            .unwrap();
            let socket = socket.recv().unwrap();
            socket2::SockRef::from(&socket).keepalive().unwrap()
        };
        assert!(
            keepalive_state("?tcp_keepalive=30"),
            "knob set -> SO_KEEPALIVE enabled"
        );
        assert!(
            !keepalive_state(""),
            "knob absent -> SO_KEEPALIVE stays off"
        );
    }

    #[test]
    #[cfg(not(feature = "tls"))]
    fn rtsps_without_tls_feature_errors() {
        let e = RtspClient::connect("rtsps://localhost:322/test").unwrap_err();
        assert!(matches!(e, RtspError::Tls(_)));
    }

    /// A hostile-but-parseable `Session: timeout=` near `u64::MAX`
    /// seconds must saturate, not wrap through `as u64` into a tiny
    /// cadence (a hot ping loop).
    #[test]
    fn duration_ms_saturates_instead_of_truncating() {
        assert_eq!(
            duration_ms_saturating(Duration::from_secs(u64::MAX)),
            u64::MAX
        );
        assert_eq!(duration_ms_saturating(Duration::from_secs(30)), 30_000);
    }

    /// Only the first keepalive call acts: a second one with a far longer
    /// interval does not replace the first cadence, so the pings keep
    /// coming at the first interval.
    #[test]
    fn spawn_keepalive_if_needed_is_noop_when_already_running() {
        use std::io::Read;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
                if String::from_utf8_lossy(&seen).contains("OPTIONS ") {
                    let _ = seen_tx.send(());
                    // Hold the connection until the client hangs up.
                    let _ = sock.read_to_end(&mut Vec::new());
                    return;
                }
            }
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let mut c = RtspClient::connect(&url).unwrap();
        c.spawn_keepalive_if_needed(Some(Duration::from_millis(20)))
            .unwrap();
        c.spawn_keepalive_if_needed(Some(Duration::from_secs(600)))
            .unwrap();
        seen_rx
            .recv_timeout(scaled(Duration::from_secs(2)))
            .expect("a ping at the first call's 20 ms interval");
        drop(c);
        peer.join().unwrap();
    }

    #[test]
    fn cancel_handle_toggles_flag() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let c = RtspClient::connect(&url).unwrap();
        let h = c.cancel_handle();
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
    }

    /// Fake peer: accepts one TCP connection, holds it open briefly, never
    /// replies, then closes.
    fn client_against_silent_peer() -> RtspClient {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(200));
            drop(sock);
        });
        RtspClient::connect(&format!("rtsp://{addr}/s")).unwrap()
    }

    /// Ends the client's thread.
    fn end_the_driver(client: &mut RtspClient) {
        client.driver.stop();
        assert!(
            client.driver.join_within(scaled(Duration::from_secs(5))),
            "the client's thread did not end"
        );
    }

    /// `Drop` runs its TEARDOWN path against a thread that has already
    /// ended, and neither panics nor waits out its bounds.
    #[test]
    fn drop_does_not_panic_when_the_driver_has_ended() {
        let mut client = client_against_silent_peer();
        // Make Drop take the TEARDOWN path.
        client.session_id = Some("12345".into());
        end_the_driver(&mut client);
        let t0 = Instant::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(client)));
        assert!(r.is_ok(), "Drop panicked with the client's thread ended");
        assert!(
            t0.elapsed() < scaled(Duration::from_millis(400)),
            "Drop took {:?}",
            t0.elapsed()
        );
    }

    /// `Drop` does not wait for the client's thread: it returns while the
    /// thread is held after its loop with the socket still open, and once
    /// released the thread ends on its own and the peer reads the end.
    #[test]
    fn drop_does_not_wait_for_the_thread() {
        use std::io::Read;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Bounds a client that never hangs up; not reached by a
            // passing test.
            sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
            let _ = sock.read_to_end(&mut Vec::new());
        });
        let (held_tx, held) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let url = RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let client = RtspClient::connect_with_params(
            &url,
            None,
            ConnectParams {
                exit_gate: Some((held_tx, release_rx)),
                ..Default::default()
            },
        )
        .unwrap();

        let t0 = Instant::now();
        drop(client);
        let elapsed = t0.elapsed();
        held.recv_timeout(scaled(Duration::from_secs(5)))
            .expect("the client's thread left its loop");
        // Unscaled on purpose: `Drop` does microseconds of work, and a join
        // would take the whole `CLOSE_JOIN_BOUND` with the thread held.
        assert!(
            elapsed < CLOSE_JOIN_BOUND,
            "Drop waited {elapsed:?} for the client's thread"
        );
        assert!(
            !peer.is_finished(),
            "the held thread closed its socket before it was released"
        );

        release.send(()).unwrap();
        let deadline = Instant::now() + scaled(Duration::from_secs(1));
        while !peer.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the released thread did not close its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        peer.join().unwrap();
    }

    /// A client and a peer that reads until the connection ends, with the
    /// client's thread held after its loop until `release` is sent (when
    /// `gate` is set).
    fn gated_client(
        gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    ) -> (RtspClient, std::thread::JoinHandle<()>) {
        use std::io::Read;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Bounds a client that never hangs up; not reached by a
            // passing test.
            sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
            let _ = sock.read_to_end(&mut Vec::new());
        });
        let url = RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let client = RtspClient::connect_with_params(
            &url,
            None,
            ConnectParams {
                exit_gate: gate,
                ..Default::default()
            },
        )
        .unwrap();
        (client, peer)
    }

    /// On a client without a session `Drop` records the cancel itself: no
    /// TEARDOWN runs, and the client's thread, held before it records its
    /// own end, has recorded nothing when the slot is read.
    #[test]
    fn drop_records_the_cancel_on_a_client_without_a_session() {
        use std::sync::mpsc;

        let (held_tx, held) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (client, peer) = gated_client(Some((held_tx, release_rx)));
        assert!(client.session_id.is_none());
        let end_reason = client.end_reason.clone();

        drop(client);
        assert_eq!(end_reason.get(), Some(EndReason::Cancelled));

        held.recv_timeout(scaled(Duration::from_secs(5)))
            .expect("the client's thread left its loop");
        release.send(()).unwrap();
        peer.join().unwrap();
    }

    /// `close` waits for the client's thread: when it returns, the thread
    /// has ended (its copy of the shared state is gone), and it did not
    /// need the whole join bound to see that.
    #[test]
    fn close_returns_after_the_thread_has_ended() {
        assert_eq!(CLOSE_JOIN_BOUND, Duration::from_secs(2));
        let (client, peer) = gated_client(None);
        let session_dead = client.session_dead.clone();

        let t0 = Instant::now();
        let r = client.close();
        let elapsed = t0.elapsed();

        assert!(r.is_ok(), "got {r:?}");
        assert_eq!(
            Arc::strong_count(&session_dead),
            1,
            "the client's thread still runs after close returned"
        );
        assert!(
            elapsed < CLOSE_JOIN_BOUND,
            "close took {elapsed:?}: the thread did not end at once"
        );
        peer.join().unwrap();
    }

    /// A thread that does not end within the join bound: `close` waits the
    /// whole bound, returns with the thread still running, and the thread
    /// ends on its own once it can (the peer then reads the end).
    #[test]
    fn close_leaves_a_thread_that_outlives_its_bound_to_finish() {
        use std::sync::mpsc;

        let (held_tx, held) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (client, peer) = gated_client(Some((held_tx, release_rx)));
        let session_dead = client.session_dead.clone();
        let bound = Duration::from_millis(300);

        let t0 = Instant::now();
        let r = client.close_with(bound);
        let elapsed = t0.elapsed();

        assert!(r.is_ok(), "got {r:?}");
        assert!(
            elapsed >= bound,
            "close returned in {elapsed:?}, before its bound"
        );
        held.recv_timeout(scaled(Duration::from_secs(5)))
            .expect("the client's thread left its loop");
        assert_eq!(
            Arc::strong_count(&session_dead),
            2,
            "the held thread ended before it was released"
        );
        assert!(!peer.is_finished(), "the held thread closed its socket");

        release.send(()).unwrap();
        let deadline = Instant::now() + scaled(Duration::from_secs(1));
        while !peer.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the released thread did not close its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        peer.join().unwrap();
    }

    /// A request when the client's thread has ended is `Io(UnexpectedEof)`
    /// at once, and the session is still cleared.
    #[test]
    fn a_request_when_the_driver_has_ended_is_io() {
        let mut client = client_against_silent_peer();
        client.session_id = Some("12345".into());
        end_the_driver(&mut client);
        let deadline = Instant::now() + Duration::from_millis(300);
        let r = client.teardown_with_deadline(Some(deadline), None);
        assert!(
            matches!(r, Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof))),
            "got {r:?}"
        );
        assert!(client.session_id.is_none());
    }
}
