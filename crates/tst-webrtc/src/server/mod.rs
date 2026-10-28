//! The WHEP server: [`WhepServer`].
//!
//! The submodules [`http`] and [`negotiate`] are public only for the
//! crate's integration tests and fuzz targets; they are not a supported
//! API.
//!
//! Each viewer session builds its own `Rtc`, so its DTLS certificate is
//! generated per session; no certificate is shared between viewers.

pub(crate) mod accept;
#[doc(hidden)]
pub mod http;
pub(crate) mod media;
#[doc(hidden)]
pub mod negotiate;
pub(crate) mod session;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(feature = "tls")]
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tracing::warn;

use crate::clock::{Clock, SystemClock};
use crate::config::MountConfig;
use crate::error::WhepServerError;
use crate::mount::{MediaMsg, MountHandle, MountId, MountState};
use crate::stats::{MountStats, ServerStats, SessionStats};
use crate::url::WhepUrl;

use accept::AcceptThread;
use media::{ControlMsg, MediaConfig, MediaThread, ServerCounters, Waker};

/// How long a control request (an offer, a delete, a statistics read)
/// waits for the media thread's reply.
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long [`WhepServer::stop`] waits for each of its two threads.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `stop` waits for its own connection to the signalling port,
/// which wakes the accept thread.
const STOP_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Control requests queued for the media thread at most.
const CONTROL_QUEUE: usize = 64;

/// Mount messages (frames, KLV records) queued for the media thread at
/// most; a push that finds the queue full drops its frame and counts it.
const MEDIA_QUEUE: usize = 4096;

/// How long a viewer may take to connect (ICE and DTLS) before it is
/// closed.
const ICE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connected viewer may stay disconnected, or its queue stay
/// full, before it is closed.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The one path from the server's control side to the media thread.
///
/// A request queues a [`ControlMsg`] with `try_send` (never blocking), wakes
/// the thread, and waits a bounded time for the reply on a channel with
/// room for exactly one message; the media thread never blocks on a reply.
#[derive(Clone)]
pub(crate) struct ControlLink {
    tx: SyncSender<ControlMsg>,
    waker: Waker,
}

impl ControlLink {
    /// Sends the request `make` builds around a fresh reply channel and
    /// waits up to [`CONTROL_REPLY_TIMEOUT`] for the reply. `None` when the
    /// queue is full, the media thread is gone, or the reply is late.
    pub(crate) fn request<T>(&self, make: impl FnOnce(SyncSender<T>) -> ControlMsg) -> Option<T> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx.try_send(make(reply)).ok()?;
        self.waker.wake();
        rx.recv_timeout(CONTROL_REPLY_TIMEOUT).ok()
    }

    /// Every open session's statistics.
    pub(crate) fn sessions(&self) -> Option<(ServerCounters, Vec<SessionStats>)> {
        self.request(|reply| ControlMsg::Stats { reply })
    }
}

/// The state the server, its accept thread and its mounts share.
pub(crate) struct Shared {
    pub(crate) url: WhepUrl,
    pub(crate) shutdown: AtomicBool,
    pub(crate) control: ControlLink,
    media_tx: SyncSender<MediaMsg>,
    /// Also read by the media thread (offer lookup, viewer gauges).
    pub(crate) mounts: Arc<Mutex<HashMap<MountId, Arc<MountState>>>>,
    next_mount: AtomicU32,
    /// Requests (POST and DELETE) refused for a missing or wrong
    /// bearer token.
    pub(crate) rejected_auth: AtomicU64,
    /// The server session config of a `wheps://` server.
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<Arc<rustls::ServerConfig>>,
    /// The newest of the media thread's snapshots any `stats` call read,
    /// returned after stop.
    last_counters: Mutex<ServerCounters>,
}

impl Shared {
    /// The mount at `path`, if any.
    pub(crate) fn mount_by_path(&self, path: &str) -> Option<Arc<MountState>> {
        let mounts = self.mounts.lock().unwrap_or_else(PoisonError::into_inner);
        mounts.values().find(|m| m.path == path).cloned()
    }
}

/// The server's two threads, taken by the first `stop`.
struct Threads {
    media: MediaThread,
    accept: AcceptThread,
}

/// A WHEP (WebRTC-HTTP Egress Protocol) playback server.
///
/// [`bind`](Self::bind) binds the signalling port (HTTP) and the media
/// port (UDP, shared by every viewer) and serves at once: there is no
/// separate start. Mounts are added with [`add_mount`](Self::add_mount);
/// a browser plays one by POSTing its SDP offer to the mount's path and
/// receives the answer and a session resource
/// (`/<mount>/sessions/<id>`) it can DELETE.
///
/// ```no_run
/// use tst_webrtc::{MountConfig, WhepServer};
/// use tst_core::mpegts::common::Pts90khz;
///
/// let server = WhepServer::bind("whep://0.0.0.0:8889?advertise=192.0.2.10")?;
/// let cam = server.add_mount("/cam1", MountConfig::default())?;
/// # let (au, key) = (Vec::<u8>::new(), true);
/// cam.push_video(&au, Pts90khz::new(0), key)?;
/// server.stop();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// A `wheps://` URL serves the signalling over HTTPS with the certificate
/// and key named by `cert=` and `key=` (feature `tls`, on by default);
/// the media stays on UDP, encrypted by DTLS-SRTP as always. Without the
/// feature a `wheps://` bind is refused.
///
/// # Closing
///
/// [`stop`](Self::stop) closes the server and is idempotent: a second
/// call returns at once. It cancels first: every mount is marked closed
/// (a later push returns [`PushError::Closed`](crate::PushError::Closed)),
/// the media thread is told to stop and the accept thread is woken, and
/// only then does it wait, at most 2 seconds for each thread. A request
/// being read when the server stops is answered with 503 or its connection
/// is closed; an offer waiting for the media thread is answered with 503.
/// [`Drop`] calls `stop`, so dropping the server frees both ports once
/// both threads have exited within that bound. A thread that has not (the
/// accept thread still writing a response to a slow client, up to its
/// 5-second write timeout) keeps its socket until it finishes.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `server.stop();` or let the server drop |
/// | Java | `server.stop()`, or `close()` (try-with-resources) to also free the native server |
/// | Kotlin | `server.use { … }`, or `server.stop()` |
/// | Swift | Not exposed in this release |
/// | Python | `with WhepServer.bind(...) as server:`, or `server.stop()` |
/// | C | Not exposed by the C binding in this release |
pub struct WhepServer {
    shared: Arc<Shared>,
    local: SocketAddr,
    media_addr: SocketAddr,
    threads: Mutex<Option<Threads>>,
    /// Both threads exited within `stop`'s bound.
    threads_joined: AtomicBool,
}

impl std::fmt::Debug for WhepServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepServer")
            .field("local_addr", &self.local)
            .field("media_addr", &self.media_addr)
            .finish_non_exhaustive()
    }
}

impl WhepServer {
    /// Binds a server on a `whep://` or `wheps://` URL and starts serving.
    ///
    /// The query keys are listed in [`url`](crate::url).
    ///
    /// # Errors
    ///
    /// [`WhepServerError::InvalidUrl`] when the URL does not parse, and
    /// the errors of [`bind_with`](Self::bind_with).
    pub fn bind(url: &str) -> Result<Self, WhepServerError> {
        Self::bind_with(WhepUrl::parse(url)?)
    }

    /// Binds a server on a parsed URL and starts serving.
    ///
    /// # Errors
    ///
    /// - [`WhepServerError::InvalidUrl`] when `url` fails
    ///   [`WhepUrl::validate`] (a hand-edited value). No port is bound.
    /// - [`WhepServerError::Tls`] for a `wheps://` URL whose certificate or
    ///   key cannot be loaded, or when the crate is built without the `tls`
    ///   feature. No port is bound in either case.
    /// - [`WhepServerError::Bind`] when the signalling or media port cannot
    ///   be bound, or a server thread cannot be started.
    pub fn bind_with(url: WhepUrl) -> Result<Self, WhepServerError> {
        Self::bind_with_clock(url, Arc::new(SystemClock))
    }

    /// [`bind_with`](Self::bind_with) with the media thread's clock
    /// supplied. A test seam; not a supported API.
    #[doc(hidden)]
    pub fn bind_with_clock(url: WhepUrl, clock: Arc<dyn Clock>) -> Result<Self, WhepServerError> {
        // Refused before any socket is bound, so a refused bind holds no port.
        url.validate()?;
        #[cfg(feature = "tls")]
        let tls = if url.tls { Some(load_tls(&url)?) } else { None };
        #[cfg(not(feature = "tls"))]
        if url.tls {
            return Err(WhepServerError::Tls("built without the tls feature".into()));
        }
        crate::ensure_crypto();

        let listener = TcpListener::bind(SocketAddr::new(url.addr, url.port))
            .map_err(|e| WhepServerError::Bind(e.kind()))?;
        let local = listener
            .local_addr()
            .map_err(|e| WhepServerError::Bind(e.kind()))?;

        let (media_tx, media_rx) = mpsc::sync_channel(MEDIA_QUEUE);
        let (ctrl_tx, ctrl_rx) = mpsc::sync_channel(CONTROL_QUEUE);
        let mounts = Arc::new(Mutex::new(HashMap::new()));
        let cfg = MediaConfig {
            bind: SocketAddr::new(url.addr, url.media_port),
            advertise: url.advertise,
            max_viewers: usize::try_from(url.max_viewers).unwrap_or(usize::MAX),
            ice_timeout: ICE_TIMEOUT,
            disconnect_timeout: DISCONNECT_TIMEOUT,
        };
        let (media, waker, media_addr) =
            MediaThread::spawn(cfg, clock, media_rx, ctrl_rx, Arc::clone(&mounts))
                .map_err(|e| WhepServerError::Bind(e.kind()))?;

        let shared = Arc::new(Shared {
            url,
            shutdown: AtomicBool::new(false),
            control: ControlLink { tx: ctrl_tx, waker },
            media_tx,
            mounts,
            next_mount: AtomicU32::new(0),
            rejected_auth: AtomicU64::new(0),
            #[cfg(feature = "tls")]
            tls,
            last_counters: Mutex::new(ServerCounters::default()),
        });
        let accept = match AcceptThread::spawn(listener, Arc::clone(&shared)) {
            Ok(accept) => accept,
            Err(e) => {
                let _ = shared.media_tx.try_send(MediaMsg::Stop);
                shared.control.waker.wake();
                media.join(STOP_JOIN_TIMEOUT);
                return Err(WhepServerError::Bind(e.kind()));
            }
        };
        Ok(Self {
            shared,
            local,
            media_addr,
            threads: Mutex::new(Some(Threads { media, accept })),
            threads_joined: AtomicBool::new(false),
        })
    }

    /// Adds a mount at `path` and returns its push handle.
    ///
    /// A path starts with `/`, names at least one segment, has no empty
    /// segment (no `//`, no trailing `/`), and holds printable ASCII other
    /// than `?` and `#`: the bytes a request target can carry, routed
    /// without decoding. Its next-to-last segment is not `sessions`, since
    /// `/<mount>/sessions/<id>` names a session. These rules are a subset of
    /// the RTSP server's mount-path rules (printable ASCII, no empty segment).
    ///
    /// # Errors
    ///
    /// - [`WhepServerError::InvalidMountPath`] for a path that breaks
    ///   those rules.
    /// - [`WhepServerError::InvalidConfig`] when [`MountConfig::validate`]
    ///   refuses `cfg`.
    /// - [`WhepServerError::MountExists`] when `path` is already mounted.
    /// - [`WhepServerError::Shutdown`] after [`stop`](Self::stop).
    pub fn add_mount(&self, path: &str, cfg: MountConfig) -> Result<MountHandle, WhepServerError> {
        let mut mounts = self
            .shared
            .mounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Read under the lock `stop` sweeps with: `stop` sets the flag before
        // it takes the lock, so a mount inserted here is either refused or
        // already in the map when the sweep marks every mount closed.
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(WhepServerError::Shutdown);
        }
        validate_mount_path(path)?;
        if mounts.values().any(|m| m.path == path) {
            return Err(WhepServerError::MountExists {
                path: path.to_string(),
            });
        }
        let id = MountId(self.shared.next_mount.fetch_add(1, Ordering::Relaxed));
        let state = Arc::new(MountState::new(
            id,
            path.to_string(),
            cfg,
            self.shared.media_tx.clone(),
            self.shared.control.waker.clone(),
            Some(self.shared.control.clone()),
        )?);
        mounts.insert(id, Arc::clone(&state));
        Ok(MountHandle { state })
    }

    /// Removes the mount at `path`: its handles' pushes return
    /// [`PushError::Closed`](crate::PushError::Closed) and its viewers'
    /// sessions are closed.
    ///
    /// # Errors
    ///
    /// [`WhepServerError::MountNotFound`] when nothing is mounted at
    /// `path`.
    pub fn remove_mount(&self, path: &str) -> Result<(), WhepServerError> {
        let removed = {
            let mut mounts = self
                .shared
                .mounts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let id = mounts
                .iter()
                .find(|(_, m)| m.path == path)
                .map(|(id, _)| *id);
            id.and_then(|id| mounts.remove(&id))
        };
        let Some(state) = removed else {
            return Err(WhepServerError::MountNotFound {
                path: path.to_string(),
            });
        };
        state.closed.store(true, Ordering::Release);
        // The mount is out of the map the media thread's gauge pass walks,
        // so a retained handle must not keep the last counts.
        state.stats.viewers.store(0, Ordering::Relaxed);
        state
            .stats
            .viewers_waiting_keyframe
            .store(0, Ordering::Relaxed);
        // The media thread also closes the sessions of a closed mount on
        // its own, so a full queue cannot keep them open.
        let _ = self
            .shared
            .media_tx
            .try_send(MediaMsg::MountRemoved(state.id));
        self.shared.control.waker.wake();
        Ok(())
    }

    /// The server's counters. After [`stop`](Self::stop), the counters as
    /// a `stats` call last read them before it.
    pub fn stats(&self) -> ServerStats {
        // Ask the media thread first, holding no lock: the wait is bounded
        // but long, and concurrent callers must not queue behind it.
        let fresh = if self.shared.shutdown.load(Ordering::Acquire) {
            None
        } else {
            self.shared.control.sessions().map(|(counters, _)| counters)
        };
        let counters = remember_newest(&self.shared.last_counters, fresh);
        let mounts = self
            .shared
            .mounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len() as u64;
        counters.into_stats(mounts, self.shared.rejected_auth.load(Ordering::Relaxed))
    }

    /// The counters of the mount at `path`, or `None` when nothing is
    /// mounted there.
    pub fn mount_stats(&self, path: &str) -> Option<MountStats> {
        self.shared.mount_by_path(path).map(|m| m.stats.snapshot())
    }

    /// The signalling (HTTP) socket's address, with the kernel's port when
    /// the URL asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The media (UDP) socket's address, shared by every viewer.
    pub fn media_addr(&self) -> SocketAddr {
        self.media_addr
    }

    /// Stops the server; see the type's `# Closing` section. Idempotent and
    /// bounded: it waits at most 2 seconds for each of its two threads.
    pub fn stop(&self) {
        if self.shared.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mounts = self
                .shared
                .mounts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for m in mounts.values() {
                m.closed.store(true, Ordering::Release);
            }
        }
        let _ = self.shared.media_tx.try_send(MediaMsg::Stop);
        self.shared.control.waker.wake();
        // A connection wakes the accept thread out of accept(); it sees the
        // flag and exits.
        if let Err(e) = TcpStream::connect_timeout(&reachable(self.local), STOP_CONNECT_TIMEOUT) {
            warn!(error = %e, "WHEP server stop: could not wake the accept thread");
        }
        let threads = self
            .threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(Threads { media, accept }) = threads {
            let media_joined = media.join(STOP_JOIN_TIMEOUT);
            if !media_joined {
                warn!("WHEP server stop: the media thread did not exit within 2 s");
            }
            let accept_joined = accept.join(STOP_JOIN_TIMEOUT);
            if !accept_joined {
                warn!("WHEP server stop: the accept thread did not exit within 2 s");
            }
            self.threads_joined
                .store(media_joined && accept_joined, Ordering::Release);
        }
    }

    /// Wakes the media thread and waits (at most 2 s) until it has run one
    /// full pass (timers, reapers, gauges) after this call, so the pass saw
    /// the clock as it is now. `false` when the thread did not answer. A
    /// test seam for a server bound with a manual clock; not a supported
    /// API.
    #[doc(hidden)]
    pub fn wake_for_test(&self) -> bool {
        self.shared
            .control
            .request(|reply| ControlMsg::Sync { reply })
            .is_some()
    }

    /// `true` once [`stop`](Self::stop) has joined both server threads
    /// within its bound. A test seam; not a supported API.
    #[doc(hidden)]
    pub fn threads_joined_for_test(&self) -> bool {
        self.threads_joined.load(Ordering::Acquire)
    }
}

/// Stores `fresh` in `last` unless `last` already holds a newer snapshot,
/// and returns what `last` then holds. Two `stats` calls can receive their
/// snapshots in one order and reach here in the other; the sequence number
/// keeps the older one from replacing the newer.
fn remember_newest(last: &Mutex<ServerCounters>, fresh: Option<ServerCounters>) -> ServerCounters {
    let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(counters) = fresh
        && counters.snapshot_seq > last.snapshot_seq
    {
        *last = counters;
    }
    last.clone()
}

impl Drop for WhepServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The address `stop` connects to: `local`, with loopback for a wildcard
/// bind.
fn reachable(local: SocketAddr) -> SocketAddr {
    let ip = match local.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, local.port())
}

/// The server certificate and key a `wheps://` URL names. `WhepUrl::parse`
/// guarantees both are present.
#[cfg(feature = "tls")]
fn load_tls(url: &WhepUrl) -> Result<Arc<rustls::ServerConfig>, WhepServerError> {
    match (&url.cert, &url.key) {
        (Some(cert), Some(key)) => crate::tls::load_server_config(Path::new(cert), Path::new(key)),
        _ => Err(WhepServerError::Tls(
            "wheps:// needs both cert= and key=".into(),
        )),
    }
}

/// Checks a mount path. The first three rules are the RTSP server's: a
/// leading `/`, no URL-reserved `?` or `#`, no control byte (C0 or DEL; the
/// detail does not echo such a path). The last two follow from how a WHEP
/// request reaches a mount: a request target is printable ASCII and is
/// routed by segment without decoding, so a space, a non-ASCII byte or an
/// empty segment would name a mount no request can reach, and a path shaped
/// `/<mount>/sessions/<id>` would be read as a session resource.
pub(crate) fn validate_mount_path(path: &str) -> Result<(), WhepServerError> {
    let invalid = |detail: String| Err(WhepServerError::InvalidMountPath { detail });
    if path.is_empty() || !path.starts_with('/') {
        return invalid(format!("path must start with '/'; got '{path}'"));
    }
    if path.contains('?') || path.contains('#') {
        return invalid(format!("path contains URL-reserved character: '{path}'"));
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return invalid("path contains a control byte".to_string());
    }
    if !path.bytes().all(|b| b.is_ascii_graphic()) {
        return invalid(format!(
            "path contains a space or a non-ASCII byte: '{path}'"
        ));
    }
    if path[1..].split('/').any(str::is_empty) {
        return invalid(format!("path has an empty segment: '{path}'"));
    }
    if http::is_session_target(path) {
        return invalid(format!(
            "path has a session resource's shape (`/<mount>/sessions/<id>`): '{path}'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    fn snapshot(seq: u64) -> ServerCounters {
        ServerCounters {
            snapshot_seq: seq,
            // A value that tells the snapshots apart in a failure message.
            rtp_packets_sent: seq * 100,
            ..ServerCounters::default()
        }
    }

    #[test]
    fn an_older_snapshot_arriving_last_does_not_replace_the_newer() {
        let last = Mutex::new(ServerCounters::default());
        assert_eq!(remember_newest(&last, Some(snapshot(2))).snapshot_seq, 2);
        let kept = remember_newest(&last, Some(snapshot(1)));
        assert_eq!((kept.snapshot_seq, kept.rtp_packets_sent), (2, 200));
        // A call that read nothing (after stop) returns the newest.
        assert_eq!(remember_newest(&last, None).snapshot_seq, 2);
    }

    #[test]
    fn concurrent_writers_leave_the_newest_snapshot() {
        const WRITERS: u64 = 8;
        for _ in 0..50 {
            let last = Mutex::new(ServerCounters::default());
            let start = Barrier::new(WRITERS as usize);
            thread::scope(|s| {
                // The writers start together, so they reach the cache in
                // whatever order the scheduler picks.
                for seq in 1..=WRITERS {
                    let (last, start) = (&last, &start);
                    s.spawn(move || {
                        start.wait();
                        remember_newest(last, Some(snapshot(seq)));
                    });
                }
            });
            let got = remember_newest(&last, None);
            assert_eq!(
                (got.snapshot_seq, got.rtp_packets_sent),
                (WRITERS, WRITERS * 100)
            );
        }
    }
}
