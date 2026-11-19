//! A TCP relay between a client under test and a real server, for what the
//! server does not show: the bytes the client wrote, and a server that
//! reads slowly. One connection; both directions copied on threads of
//! their own.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct TcpRelay {
    port: u16,
    /// Everything the client wrote that was passed on.
    upstream: Arc<Mutex<Vec<u8>>>,
    /// Everything the server wrote that was passed on.
    downstream: Arc<Mutex<Vec<u8>>>,
    /// Set: the relay reads from the client slowly.
    slow: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    /// The two sockets once the client connected, to end the copies.
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    threads: Vec<JoinHandle<()>>,
}

impl TcpRelay {
    /// A relay on a loopback port that passes its first connection on to
    /// `server`.
    pub fn to(server: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = Arc::new(Mutex::new(Vec::new()));
        let downstream = Arc::new(Mutex::new(Vec::new()));
        let slow = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let (upstream, downstream, slow, done, sockets) = (
                upstream.clone(),
                downstream.clone(),
                slow.clone(),
                done.clone(),
                sockets.clone(),
            );
            std::thread::spawn(move || {
                let Ok((client, _)) = listener.accept() else {
                    return;
                };
                let server = {
                    // Under the lock `Drop` takes to shut the sockets: they
                    // are either shut by it or never copied.
                    let mut open = sockets.lock().unwrap();
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    let server = TcpStream::connect(server).expect("the relay reaches the server");
                    open.extend([client.try_clone().unwrap(), server.try_clone().unwrap()]);
                    server
                };
                let down = {
                    let (from, to) = (server.try_clone().unwrap(), client.try_clone().unwrap());
                    let done = done.clone();
                    std::thread::spawn(move || {
                        copy(from, to, &downstream, &AtomicBool::new(false), &done);
                    })
                };
                copy(client, server, &upstream, &slow, &done);
                let _ = down.join();
            })
        };
        Self {
            port,
            upstream,
            downstream,
            slow,
            done,
            sockets,
            threads: vec![accept],
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Read from the client slowly: at most [`SLOW_READ`] bytes every
    /// [`SLOW_PAUSE`]. The client's writes back up behind that, as with a
    /// server that reads slowly, but the connection keeps taking bytes, so
    /// the client never sees it stalled.
    pub fn slow_down(&self) {
        self.slow.store(true, Ordering::Release);
    }

    /// The client's bytes passed on so far.
    pub fn upstream(&self) -> Vec<u8> {
        self.upstream.lock().unwrap().clone()
    }

    /// The server's bytes passed on so far.
    pub fn downstream(&self) -> Vec<u8> {
        self.downstream.lock().unwrap().clone()
    }
}

/// What a slowed relay reads at a time.
pub const SLOW_READ: usize = 64 * 1024;
/// How long a slowed relay waits after each read.
pub const SLOW_PAUSE: Duration = Duration::from_millis(200);

/// Copy `from` to `to` until either side ends or the relay is dropped,
/// noting what passes in `seen`, slowly while `slow` is set.
fn copy(
    mut from: TcpStream,
    mut to: TcpStream,
    seen: &Mutex<Vec<u8>>,
    slow: &AtomicBool,
    done: &AtomicBool,
) {
    let mut buf = vec![0u8; SLOW_READ];
    from.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    while !done.load(Ordering::Acquire) {
        if slow.load(Ordering::Acquire) {
            std::thread::sleep(SLOW_PAUSE);
        }
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => break,
        };
        seen.lock().unwrap().extend_from_slice(&buf[..n]);
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

impl Drop for TcpRelay {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        for s in self
            .sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = s.shutdown(Shutdown::Both);
        }
        // An accept still waiting is ended by a connection of our own.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
