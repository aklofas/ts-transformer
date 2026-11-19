//! Socket recipes shared by the client's unit tests. Linux only: the fill
//! latch is tuned to Linux TCP (a pinned 4 KiB send buffer, and a delayed
//! ACK of at most 200 ms as the last thing that frees space) and has not
//! been measured on macOS or Windows.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A peer behind a 4 KiB receive buffer (set before `listen`, so the
/// accepted socket inherits it) that accepts and never reads. It
/// drops the connection when `done` is signalled, which resets it
/// and fails any write still blocked on it.
pub(crate) fn non_reading_peer() -> (u16, mpsc::Sender<()>, JoinHandle<()>) {
    non_reading_peer_after(|_| {})
}

/// [`non_reading_peer`] that first runs `opening` on the accepted
/// connection; it reads nothing after that.
pub(crate) fn non_reading_peer_after(
    opening: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> (u16, mpsc::Sender<()>, JoinHandle<()>) {
    use socket2::{Domain, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    sock.bind(
        &"127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
    )
    .unwrap();
    sock.listen(1).unwrap();
    let listener: TcpListener = sock.into();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let h = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            opening(&mut sock);
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
            drop(sock);
        }
    });
    (port, done_tx, h)
}

/// Fills `tcp`'s send buffer: pins it to 4 KiB (no autotuning),
/// writes 4 KiB chunks without blocking until the kernel refuses one,
/// then single bytes until none has been taken for 300 ms. The quiet
/// period outlasts the peer's delayed ACK (at most 200 ms on Linux),
/// the last thing that frees send-buffer space: the peer never
/// reads, so after it the buffer stays full and the next blocking
/// write parks.
pub(crate) fn fill_send_buffer(tcp: &TcpStream) {
    socket2::SockRef::from(tcp)
        .set_send_buffer_size(4096)
        .unwrap();
    tcp.set_nonblocking(true).unwrap();
    let chunk = [0u8; 4096];
    let give_up = Instant::now() + Duration::from_secs(10);
    let would_block = |r: std::io::Result<usize>| match r {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(e) => panic!("filling the send buffer: {e}"),
    };
    while !would_block((&*tcp).write(&chunk)) {
        assert!(Instant::now() < give_up, "the send buffer never filled");
    }
    let mut last_taken = Instant::now();
    while last_taken.elapsed() < Duration::from_millis(300) {
        assert!(
            Instant::now() < give_up,
            "the send buffer never stayed full"
        );
        if would_block((&*tcp).write(&chunk[..1])) {
            std::thread::sleep(Duration::from_millis(10));
        } else {
            last_taken = Instant::now();
        }
    }
    tcp.set_nonblocking(false).unwrap();
}
