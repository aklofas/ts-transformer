//! The RTSP client's control stream in non-blocking mode, plain TCP or
//! rustls over TCP: reads take what the socket has, writes queue and go
//! out as the kernel takes them. Nothing here ever waits.

use std::io::{self, Read, Write};

/// Bytes `read_available` takes in one call before it hands them back, so
/// a peer that never pauses cannot grow the caller's buffer without bound.
const READ_BUDGET: usize = 256 * 1024;

const READ_CHUNK: usize = 16 * 1024;

/// What the read side looks like after a `read_available`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadState {
    /// Everything the socket had was read.
    Open,
    /// The read budget was used up: the socket may still hold bytes, and
    /// no readiness event will say so.
    More,
    /// The peer closed its side (for TLS: a `close_notify`).
    Eof,
}

enum Io {
    Plain(mio::net::TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<TlsIo>),
}

#[cfg(feature = "tls")]
struct TlsIo {
    conn: rustls::ClientConnection,
    sock: mio::net::TcpStream,
}

/// A non-blocking control stream with its own write queue.
pub(crate) struct Stream {
    io: Io,
    /// Queued bytes; those before `out_pos` have left the queue.
    out: Vec<u8>,
    out_pos: usize,
    /// Plaintext bytes handed to rustls, counted from the start.
    #[cfg(feature = "tls")]
    handed: u64,
    /// Queued bytes known to be in the kernel, counted from the start.
    written: u64,
    /// Bytes the kernel has taken (ciphertext for TLS), counted from the
    /// start.
    wire: u64,
    read_budget: usize,
}

// By hand: the queue holds request bytes, which can carry credentials.
impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.io {
            Io::Plain(_) => "plain",
            #[cfg(feature = "tls")]
            Io::Tls(_) => "tls",
        };
        f.debug_struct("Stream")
            .field("kind", &kind)
            .field("unsent", &self.out.len().saturating_sub(self.out_pos))
            .field("written", &self.written)
            .field("wire", &self.wire)
            .finish()
    }
}

impl Stream {
    /// A plain TCP control stream; the socket is switched to non-blocking.
    pub(crate) fn from_plain(tcp: std::net::TcpStream) -> io::Result<Self> {
        tcp.set_nonblocking(true)?;
        Ok(Self::new(Io::Plain(mio::net::TcpStream::from_std(tcp))))
    }

    /// A TLS control stream from a completed handshake. Plaintext rustls
    /// has already decrypted and bytes it still has to write are kept.
    #[cfg(feature = "tls")]
    pub(crate) fn from_tls(tls: super::tls::TlsStream) -> io::Result<Self> {
        let (conn, sock) = tls.into_parts();
        Self::from_tls_parts(conn, sock)
    }

    #[cfg(feature = "tls")]
    pub(super) fn from_tls_parts(
        conn: rustls::ClientConnection,
        sock: std::net::TcpStream,
    ) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        Ok(Self::new(Io::Tls(Box::new(TlsIo {
            conn,
            sock: mio::net::TcpStream::from_std(sock),
        }))))
    }

    fn new(io: Io) -> Self {
        Self {
            io,
            out: Vec::new(),
            out_pos: 0,
            #[cfg(feature = "tls")]
            handed: 0,
            written: 0,
            wire: 0,
            read_budget: READ_BUDGET,
        }
    }

    /// Bytes one `read_available` takes before it returns `More`.
    #[cfg(test)]
    pub(crate) fn with_read_budget(mut self, budget: usize) -> Self {
        self.read_budget = budget;
        self
    }

    /// The socket, to register with a `mio::Poll`.
    pub(crate) fn source(&mut self) -> &mut mio::net::TcpStream {
        match &mut self.io {
            Io::Plain(s) => s,
            #[cfg(feature = "tls")]
            Io::Tls(t) => &mut t.sock,
        }
    }

    /// Append what can be read without waiting to `out`, up to the read
    /// budget. Bytes read before an end or an error are in `out` too.
    pub(crate) fn read_available(&mut self, out: &mut Vec<u8>) -> io::Result<ReadState> {
        let limit = out.len().saturating_add(self.read_budget);
        match &mut self.io {
            Io::Plain(s) => read_plain(s, out, limit),
            #[cfg(feature = "tls")]
            Io::Tls(t) => read_tls(t, out, limit),
        }
    }

    /// Queue `bytes` behind everything queued before.
    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// Write what the kernel takes without waiting. `true`: nothing is
    /// left to write.
    pub(crate) fn flush(&mut self) -> io::Result<bool> {
        let r = match &mut self.io {
            Io::Plain(s) => flush_plain(
                s,
                &self.out,
                &mut self.out_pos,
                &mut self.written,
                &mut self.wire,
            ),
            #[cfg(feature = "tls")]
            Io::Tls(t) => flush_tls(
                t,
                &self.out,
                &mut self.out_pos,
                &mut self.handed,
                &mut self.written,
                &mut self.wire,
            ),
        };
        self.compact();
        r
    }

    /// Queued bytes the kernel has taken, counted from the first `queue`.
    /// For TLS a byte counts once rustls has nothing left to write after
    /// it was handed over.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Bytes the kernel has taken, counted from the start; it moves with
    /// every accepted byte, ciphertext included.
    pub(crate) fn wire_bytes(&self) -> u64 {
        self.wire
    }

    fn compact(&mut self) {
        if self.out_pos >= self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        } else if self.out_pos > 0 && self.out_pos >= self.out.len() / 2 {
            // Moving the unsent half costs no more than what was sent.
            self.out.drain(..self.out_pos);
            self.out_pos = 0;
        }
    }
}

fn read_plain(
    s: &mut mio::net::TcpStream,
    out: &mut Vec<u8>,
    limit: usize,
) -> io::Result<ReadState> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        if out.len() >= limit {
            return Ok(ReadState::More);
        }
        match s.read(&mut chunk) {
            Ok(0) => return Ok(ReadState::Eof),
            Ok(n) => out.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(ReadState::Open),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn flush_plain(
    s: &mut mio::net::TcpStream,
    out: &[u8],
    out_pos: &mut usize,
    written: &mut u64,
    wire: &mut u64,
) -> io::Result<bool> {
    loop {
        let rest = out.get(*out_pos..).unwrap_or_default();
        if rest.is_empty() {
            return Ok(true);
        }
        match s.write(rest) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                *out_pos += n;
                *written += n as u64;
                *wire += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// rustls's model: decrypted plaintext is drained before each socket read
/// (it can be there before any read, from the handshake), each read is
/// followed by `process_new_packets`, and reads go on until `WouldBlock`.
#[cfg(feature = "tls")]
fn read_tls(t: &mut TlsIo, out: &mut Vec<u8>, limit: usize) -> io::Result<ReadState> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        loop {
            if out.len() >= limit {
                return Ok(ReadState::More);
            }
            match t.conn.reader().read(&mut chunk) {
                // A `close_notify` was read and every byte before it taken.
                Ok(0) => return Ok(ReadState::Eof),
                Ok(n) => out.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // A TCP end without a `close_notify` is `UnexpectedEof`.
                Err(e) => return Err(e),
            }
        }
        match t.conn.read_tls(&mut t.sock) {
            Ok(_) => {
                // A TCP end (`Ok(0)`) is noticed here too, and the reader
                // then reports it.
                t.conn.process_new_packets().map_err(io::Error::other)?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(ReadState::Open),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Plaintext is handed to rustls only when it has nothing left to write,
/// so once it has written everything again every handed byte is known to
/// be in the kernel.
#[cfg(feature = "tls")]
fn flush_tls(
    t: &mut TlsIo,
    out: &[u8],
    out_pos: &mut usize,
    handed: &mut u64,
    written: &mut u64,
    wire: &mut u64,
) -> io::Result<bool> {
    loop {
        while t.conn.wants_write() {
            match t.conn.write_tls(&mut t.sock) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => *wire += n as u64,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        *written = *handed;
        let rest = out.get(*out_pos..).unwrap_or_default();
        if rest.is_empty() {
            return Ok(true);
        }
        // rustls takes up to its buffer limit; the rest waits for the next
        // round.
        let n = t.conn.writer().write(rest)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        *out_pos += n;
        *handed += n as u64;
    }
}
