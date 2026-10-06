//! TLS support via rustls 0.23 (feature `tls`).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Instant;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, StreamOwned};

use crate::config::SocketConfig;
use crate::error::TcpError;
use crate::recv_knobs::apply_knobs;
use crate::transport::TcpTransport;
use crate::url::TcpUrl;

/// TLS stream — wraps a rustls ClientConnection or ServerConnection + the underlying TcpStream.
pub enum TlsStream {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl TlsStream {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(s) => s.read(buf),
            Self::Server(s) => s.read(buf),
        }
    }
    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Client(s) => s.write(buf),
            Self::Server(s) => s.write(buf),
        }
    }

    /// Best-effort TLS shutdown: consume whatever the peer already sent,
    /// queue a `close_notify` alert, make one attempt to flush it, then shut
    /// the TCP socket down in both directions.
    ///
    /// Every step is deliberately best-effort and non-blocking on the peer:
    /// the drain reads only what is already in the socket buffer,
    /// `write_tls` is called exactly once (never looped) and we never wait for
    /// the peer's own `close_notify`, so a wedged or already-gone peer cannot
    /// stall `Transport::close`. If that single write cannot flush the alert,
    /// the socket shutdown that follows still gives the peer an EOF.
    ///
    /// The drain is what makes the alert reliably *arrive*. A TLS 1.3 server
    /// sends `NewSessionTicket` records right after the handshake (rustls:
    /// two by default); a write-only caller never reads them, so they sit
    /// unread in the receive buffer. The kernel answers a shutdown or close
    /// of a socket with unread receive data with RST instead of FIN (Windows
    /// at `shutdown`, Linux at `close`), and a RST can overtake and purge the
    /// `close_notify` just written, so the peer sees a reset
    /// (`BrokenCause::Unspecified`) rather than a clean EOF.
    pub(crate) fn shutdown(&mut self) {
        match self {
            Self::Client(s) => {
                drain_unread(&mut s.conn, &mut s.sock);
                s.conn.send_close_notify();
                let _ = s.conn.write_tls(&mut s.sock);
                let _ = s.sock.shutdown(std::net::Shutdown::Both);
            }
            Self::Server(s) => {
                drain_unread(&mut s.conn, &mut s.sock);
                s.conn.send_close_notify();
                let _ = s.conn.write_tls(&mut s.sock);
                let _ = s.sock.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// Read and discard the TLS records the peer has already delivered, without
/// ever waiting for more: the socket is switched to non-blocking for the
/// duration, every outcome other than "a record was read" ends the loop, and
/// the loop is capped so a peer still streaming cannot hold `close()` either.
/// Application data consumed here is discarded — the caller is closing.
fn drain_unread<C, D>(conn: &mut C, sock: &mut TcpStream)
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<D>>,
{
    const MAX_READS: usize = 32;
    if sock.set_nonblocking(true).is_err() {
        return;
    }
    for _ in 0..MAX_READS {
        match conn.read_tls(sock) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if conn.process_new_packets().is_err() {
                    break;
                }
            }
        }
    }
    let _ = sock.set_nonblocking(false);
}

/// Build a TLS-wrapped TcpTransport (caller side).
///
/// The TLS server name is the host as written in the URL — a DNS hostname or
/// an IP literal. The server certificate must carry a matching SAN:
/// - hostname → `dnsName` SAN
/// - IP literal → `iPAddress` SAN
///
/// Resolution to a socket address happens at connect time.
pub fn connect_tls(url: &TcpUrl, cfg: &SocketConfig) -> Result<TcpTransport, TcpError> {
    let mut roots = rustls::RootCertStore::empty();

    if let Some(ca_path) = &url.ca {
        let ca_data = std::fs::read(ca_path).map_err(TcpError::Io)?;
        let mut reader = std::io::BufReader::new(&ca_data[..]);
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(TcpError::Io)?;
            roots
                .add(cert)
                .map_err(|e| TcpError::Tls(format!("add CA cert: {e}")))?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs()
            .map_err(|e| TcpError::Tls(format!("load native certs: {e:?}")))?;
        for cert in native {
            roots
                .add(cert)
                .map_err(|e| TcpError::Tls(format!("add native cert: {e}")))?;
        }
    }

    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let server_name = ServerName::try_from(url.host.clone())
        .map_err(|e| TcpError::Tls(format!("invalid server name '{}': {e}", url.host)))?;

    let conn = ClientConnection::new(Arc::new(client_config), server_name)
        .map_err(|e| TcpError::Tls(format!("ClientConnection::new: {e}")))?;

    let (socket, peer) =
        crate::transport::connect_stream(&url.host, url.port, cfg.connect_timeout_or_default())
            .map_err(|e| crate::transport::map_connect_err(e, cfg))?;
    apply_knobs(&socket, cfg).map_err(TcpError::Io)?;

    let mut stream = StreamOwned::new(conn, socket);
    complete_client_handshake(&mut stream.conn, &mut stream.sock, cfg)?;
    let tls = TlsStream::Client(stream);
    Ok(TcpTransport::from_tls(tls, peer, cfg))
}

/// Drive the client half of the TLS handshake to completion inside
/// `connect_tls`, under its own `connect_timeout` budget that starts after
/// the TCP connect.
///
/// rustls otherwise completes the handshake lazily, inside the first
/// `read`/`write` on the stream. Under the 100 ms cancel-poll socket
/// timeouts `apply_knobs` sets, that first I/O returned a zero-progress
/// `Backpressure` whenever the server's reply took longer than one tick —
/// and did so forever against a peer that accepted TCP but never spoke TLS,
/// so a managed sender believed it was connected and never reconnected,
/// while a certificate rejection surfaced as a send/recv error on a
/// "connected" transport. Running the handshake here gives every caller the
/// connect-time outcome the builder documents: a handshake that does not
/// finish in time is [`TcpError::ConnectTimeout`] (the same outcome an
/// unanswered SYN gets), a TLS-level failure (certificate, protocol) is
/// [`TcpError::Tls`], and a peer that closes or resets mid-handshake is
/// [`TcpError::Io`].
///
/// The loop is one socket operation per iteration — flush what rustls
/// wants written, then ONE `read_tls` — with the deadline checked every
/// time round. `ClientConnection::complete_io` is deliberately not used:
/// it loops internally until the handshake completes, so a deadline
/// checked only between its calls never runs against a peer that keeps
/// every read succeeding by dripping a byte inside each 100 ms socket
/// timeout (a slow-loris handshake). Here a read that returns data and a
/// read that times out (`WouldBlock` on Unix, `TimedOut` on Windows — the
/// same pair `classify_send_error` treats as transient) both come back to
/// the deadline check, so the budget is a wall-clock bound to within one
/// socket-timeout tick. `EINTR` is retried. The server half stays lazy
/// (`accept_tls`): an accept has no connect budget, and the server's
/// handshake completes on its first I/O as before.
fn complete_client_handshake(
    conn: &mut ClientConnection,
    sock: &mut TcpStream,
    cfg: &SocketConfig,
) -> Result<(), TcpError> {
    let budget = cfg.connect_timeout_or_default();
    // `None` = no deadline: a budget too large to add to `now` (the URL
    // accepts any u64 seconds) means "wait as long as it takes".
    let deadline = Instant::now().checked_add(budget);
    let timed_out = || TcpError::ConnectTimeout {
        seconds: budget.as_secs(),
    };
    let is_tick = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    };
    while conn.is_handshaking() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(timed_out());
        }
        // Flush everything rustls has queued (ClientHello, Finished, …).
        // A write that times out just comes back round to the deadline.
        let mut flushed = true;
        while conn.wants_write() {
            match conn.write_tls(sock) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if is_tick(&e) => {
                    flushed = false;
                    break;
                }
                Err(e) => return Err(TcpError::Io(e)),
            }
        }
        if !flushed || !conn.wants_read() {
            continue;
        }
        // One read, bounded by the socket's 100 ms receive timeout.
        match conn.read_tls(sock) {
            Ok(0) => {
                return Err(TcpError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed the connection during the TLS handshake",
                )));
            }
            Ok(_) => {
                if let Err(e) = conn.process_new_packets() {
                    // Let the alert rustls queued for the peer go out
                    // (best effort), then report the TLS-level failure —
                    // certificate verification, protocol violation, or an
                    // alert from the peer.
                    let _ = conn.write_tls(sock);
                    return Err(TcpError::Tls(format!("TLS handshake failed: {e}")));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_tick(&e) => {}
            Err(e) => return Err(TcpError::Io(e)),
        }
    }
    Ok(())
}

/// Load a server certificate + key from PEM files.
pub fn load_server_config(cert_path: &str, key_path: &str) -> Result<ServerConfig, TcpError> {
    let certs = {
        let data = std::fs::read(cert_path).map_err(TcpError::Io)?;
        let mut reader = std::io::BufReader::new(&data[..]);
        rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(TcpError::Io)?
    };

    let key = {
        let data = std::fs::read(key_path).map_err(TcpError::Io)?;
        let mut reader = std::io::BufReader::new(&data[..]);
        rustls_pemfile::private_key(&mut reader)
            .map_err(TcpError::Io)?
            .ok_or_else(|| TcpError::Tls(format!("no private key in {key_path}")))?
    };

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TcpError::Tls(format!("ServerConfig build: {e}")))?;
    Ok(config)
}

/// Accept a TLS connection (called by TcpListener::accept_blocking when tls_config is set).
pub fn accept_tls(
    socket: TcpStream,
    peer: SocketAddr,
    cfg: &SocketConfig,
    server_config: Arc<ServerConfig>,
) -> Result<TcpTransport, TcpError> {
    apply_knobs(&socket, cfg).map_err(TcpError::Io)?;
    let conn = ServerConnection::new(server_config)
        .map_err(|e| TcpError::Tls(format!("ServerConnection::new: {e}")))?;
    let stream = StreamOwned::new(conn, socket);
    let tls = TlsStream::Server(stream);
    Ok(TcpTransport::from_tls(tls, peer, cfg))
}
