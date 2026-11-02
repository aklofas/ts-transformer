//! TLS support via rustls 0.23 (feature `tls`).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Instant;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, StreamOwned};

use crate::config::TcpConfig;
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
    /// Read plaintext with at most one socket read per call.
    ///
    /// `StreamOwned::read` loops until a whole record arrives, so a peer
    /// dripping a record one byte at a time keeps it inside that loop past
    /// any cancel. Returning `WouldBlock` after each socket read hands the
    /// caller's receive loop a chance to observe a cancel in between.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(s) => read_stepwise(&mut s.conn, &mut s.sock, buf),
            Self::Server(s) => read_stepwise(&mut s.conn, &mut s.sock, buf),
        }
    }
    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Client(s) => s.write(buf),
            Self::Server(s) => s.write(buf),
        }
    }

    /// Flush what rustls wants written, then make at most ONE `read_tls`
    /// and process it. Errors: the poll tick (`WouldBlock`/`TimedOut`) when
    /// nothing arrived; `UnexpectedEof` on `read_tls` == 0; `InvalidData`
    /// wrapping the rustls error when processing fails (the queued alert is
    /// flushed best-effort first); `Interrupted` is retried inside.
    pub(crate) fn handshake_step(&mut self) -> io::Result<HandshakeStep> {
        match self {
            Self::Client(s) => handshake_step(&mut s.conn, &mut s.sock),
            Self::Server(s) => handshake_step(&mut s.conn, &mut s.sock),
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

/// One step of the lazy server handshake, driven by the first send.
pub(crate) enum HandshakeStep {
    /// `is_handshaking()` is false: application writes may proceed.
    Done,
    /// Bytes were flushed or read and processed; still handshaking. The
    /// caller re-checks its cancel and liveness flags, then steps again.
    Progress,
}

/// One socket step of a handshake: one `write_tls` while output is pending
/// (a partial write ends the step), then at most one `read_tls`.
/// `StreamOwned`'s lazy `complete_io` loops until the handshake ends, so a
/// peer delivering a byte inside every socket tick would keep it there; one
/// step per call lets the caller observe a cancel in between.
fn handshake_step<C, D>(conn: &mut C, sock: &mut TcpStream) -> io::Result<HandshakeStep>
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<D>>,
{
    if !conn.is_handshaking() {
        return Ok(HandshakeStep::Done);
    }
    let mut flushed = false;
    while conn.wants_write() {
        match conn.write_tls(sock) {
            // A partial write took up to a socket tick: end the step so the
            // caller reads its flags before the next one. A peer draining a
            // large flight a little per tick would otherwise hold the loop
            // until the whole flight is out.
            Ok(_) if conn.wants_write() => return Ok(HandshakeStep::Progress),
            Ok(_) => flushed = true,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // The peer is not reading: report the tick unless this step
            // already moved bytes.
            Err(e) if is_poll_timeout(&e) && flushed => return Ok(HandshakeStep::Progress),
            Err(e) => return Err(e),
        }
    }
    if !conn.wants_read() {
        // Unreachable on the accepted server path: the send buffer is empty
        // after the flush above, rustls ignores a close_notify before
        // application data, and early data is off, so a handshaking server
        // always wants to read. Fail instead of returning `Progress`, which
        // would spin the caller's loop with no I/O.
        return Err(io::Error::other(
            "TLS handshake stalled: rustls wants neither to read nor to write",
        ));
    }
    let n = loop {
        match conn.read_tls(sock) {
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_poll_timeout(&e) && flushed => return Ok(HandshakeStep::Progress),
            Err(e) => return Err(e),
        }
    };
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed the connection during the TLS handshake",
        ));
    }
    if let Err(e) = conn.process_new_packets() {
        // Best effort: let the peer see the alert rustls queued.
        let _ = conn.write_tls(sock);
        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
    }
    Ok(HandshakeStep::Progress)
}

/// One step of a TLS read: buffered plaintext first; otherwise flush any
/// pending handshake output, make one `read_tls`, process it and look for
/// plaintext once more. `WouldBlock` means "no plaintext yet, call again".
fn read_stepwise<C, D>(conn: &mut C, sock: &mut TcpStream, buf: &mut [u8]) -> io::Result<usize>
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<D>>,
{
    if let Some(r) = buffered_plaintext(conn, buf) {
        return r;
    }
    if conn.wants_write() {
        // Handshake replies and key updates. A full send buffer must not
        // stop us reading: the next call retries the write.
        match conn.write_tls(sock) {
            Ok(_) => {}
            Err(e) if is_poll_timeout(&e) => {}
            Err(e) => return Err(e),
        }
    }
    if conn.read_tls(sock)? == 0 {
        // TCP EOF without a preceding close_notify: a truncated stream.
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if let Err(e) = conn.process_new_packets() {
        // Best effort: let the peer see the alert rustls queued.
        let _ = conn.write_tls(sock);
        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
    }
    buffered_plaintext(conn, buf).unwrap_or_else(|| Err(io::ErrorKind::WouldBlock.into()))
}

/// `Some` when the connection already holds an answer for the caller:
/// plaintext, `Ok(0)` after the peer's close_notify, or a reader error.
/// `None` when no plaintext is buffered yet.
fn buffered_plaintext<C, D>(conn: &mut C, buf: &mut [u8]) -> Option<io::Result<usize>>
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<D>>,
{
    match conn.reader().read(buf) {
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
        other => Some(other),
    }
}

/// The socket's poll interval elapsing (Linux spells it `WouldBlock`,
/// Windows `TimedOut`).
pub(crate) fn is_poll_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
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
pub fn connect_tls(url: &TcpUrl, cfg: &TcpConfig) -> Result<TcpTransport, TcpError> {
    let roots = if let Some(ca_path) = &cfg.ca {
        let pem = std::fs::read(ca_path).map_err(TcpError::Io)?;
        tst_core::net::tls::load_root_store(&pem).map_err(tls_load_error)?
    } else {
        native_roots(rustls_native_certs::load_native_certs())?.0
    };

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

/// Build the root store from the OS trust store's load result, returning it
/// with the number of load errors. Errors beside at least one loaded
/// certificate are logged and counted; errors with no certificate at all fail
/// the connect, as a failed load did before rustls-native-certs 0.8 reported
/// partial results.
fn native_roots(
    native: rustls_native_certs::CertificateResult,
) -> Result<(rustls::RootCertStore, usize), TcpError> {
    if native.certs.is_empty() && !native.errors.is_empty() {
        return Err(TcpError::Tls(format!(
            "load native certs: {:?}",
            native.errors
        )));
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in native.certs {
        roots
            .add(cert)
            .map_err(|e| TcpError::Tls(format!("add native cert: {e}")))?;
    }
    let errors = native.errors.len();
    if let Some(first) = native.errors.first() {
        tracing::warn!(errors, first = %first, "some native certificates failed to load");
    }
    Ok((roots, errors))
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
/// (`accept_tls`): an accept has no connect budget. Its first send steps the
/// handshake one socket operation at a time under the transport's cancel
/// and liveness flags, and its first receive steps it inside
/// `read_stepwise`.
fn complete_client_handshake(
    conn: &mut ClientConnection,
    sock: &mut TcpStream,
    cfg: &TcpConfig,
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

/// Read and PEM-parse failures keep the `Io` kind they always had; the rest
/// are TLS configuration errors.
pub(crate) fn tls_load_error(e: tst_core::net::tls::TlsLoadError) -> TcpError {
    use tst_core::net::tls::TlsLoadError as E;
    match e {
        E::Read { source, .. } | E::Pem { source, .. } => TcpError::Io(source),
        other => TcpError::Tls(other.to_string()),
    }
}

/// Accept a TLS connection (called by TcpListener::accept_blocking when tls_config is set).
///
/// The handshake is not run here: the returned transport's first I/O drives
/// it, one socket step at a time, so a cancel can end it.
pub fn accept_tls(
    socket: TcpStream,
    peer: SocketAddr,
    cfg: &TcpConfig,
    server_config: Arc<ServerConfig>,
) -> Result<TcpTransport, TcpError> {
    apply_knobs(&socket, cfg).map_err(TcpError::Io)?;
    let conn = ServerConnection::new(server_config)
        .map_err(|e| TcpError::Tls(format!("ServerConnection::new: {e}")))?;
    let stream = StreamOwned::new(conn, socket);
    let tls = TlsStream::Server(stream);
    Ok(TcpTransport::from_tls(tls, peer, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_error() -> rustls_native_certs::Error {
        rustls_native_certs::Error {
            context: "test",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: io::Error::other("unreadable"),
                path: "/nonexistent/cert.pem".into(),
            },
        }
    }

    #[test]
    fn native_roots_partial_load_counts_errors() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut partial = rustls_native_certs::CertificateResult::default();
        partial.certs.push(cert.cert.der().clone());
        partial.errors.push(load_error());
        let (roots, errors) = native_roots(partial).expect("a partial load keeps its certs");
        assert_eq!(roots.len(), 1);
        assert_eq!(errors, 1);

        let mut failed = rustls_native_certs::CertificateResult::default();
        failed.errors.push(load_error());
        match native_roots(failed) {
            Err(TcpError::Tls(msg)) => assert!(msg.starts_with("load native certs: "), "{msg}"),
            other => panic!("expected TcpError::Tls, got {other:?}"),
        }
    }
}
