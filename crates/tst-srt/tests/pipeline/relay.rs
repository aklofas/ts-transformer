//! A UDP relay between SRT callers and one listener, with a mute switch:
//! the way a test makes a connected peer go silent without closing it.
//!
//! Callers dial [`Relay::addr`]. Each caller address gets its own upstream
//! socket, so the listener sees one source per caller, and a re-dial (a
//! fresh SRT socket, a fresh source port) gets a fresh forwarding flow.
//! [`Relay::mute_existing`] stops forwarding in both directions for every
//! flow that exists at that moment; a flow created later forwards normally.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long a relay thread waits for a datagram before it looks at the
/// stop flag again.
const POLL: Duration = Duration::from_millis(50);

struct Flow {
    up: UdpSocket,
    client: SocketAddr,
    muted: AtomicBool,
}

struct Shared {
    front: UdpSocket,
    upstream: SocketAddr,
    flows: Mutex<HashMap<SocketAddr, Arc<Flow>>>,
    stop: AtomicBool,
    /// The threads that forward from the listener back to each caller,
    /// joined by [`Relay`]'s `Drop`.
    back: Mutex<Vec<JoinHandle<()>>>,
}

/// The test's handle on a running relay. Its threads each hold the shared
/// state, so it never drops by itself: dropping this handle sets the stop
/// flag and joins every thread.
pub struct Relay {
    shared: Arc<Shared>,
    front_thread: Option<JoinHandle<()>>,
}

impl Relay {
    /// Start a relay on a loopback port that forwards to `upstream`.
    pub fn start(upstream: SocketAddr) -> Self {
        let front = UdpSocket::bind("127.0.0.1:0").expect("relay front bind");
        front.set_read_timeout(Some(POLL)).expect("relay timeout");
        let shared = Arc::new(Shared {
            front,
            upstream,
            flows: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            back: Mutex::new(Vec::new()),
        });
        let s = Arc::clone(&shared);
        let front_thread = thread::spawn(move || front_loop(&s));
        Self {
            shared,
            front_thread: Some(front_thread),
        }
    }

    /// The address callers dial.
    pub fn addr(&self) -> SocketAddr {
        self.shared.front.local_addr().expect("relay addr")
    }

    /// Stop forwarding, both ways, for every flow that exists now.
    /// Returns how many flows were muted.
    pub fn mute_existing(&self) -> usize {
        let flows = self.shared.flows.lock().unwrap();
        for f in flows.values() {
            f.muted.store(true, Ordering::SeqCst);
        }
        flows.len()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // The front thread is the only one that starts back threads, so
        // once it has ended the list is complete.
        if let Some(h) = self.front_thread.take() {
            let _ = h.join();
        }
        let back: Vec<_> = self.shared.back.lock().unwrap().drain(..).collect();
        for h in back {
            let _ = h.join();
        }
    }
}

// Every receive error is skipped, not fatal: a read timeout is how a
// thread gets to look at the stop flag, and Windows reports an ICMP port
// unreachable (a peer socket already closed) as `ConnectionReset` on the
// next receive.
fn front_loop(shared: &Arc<Shared>) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::SeqCst) {
        let Ok((n, from)) = shared.front.recv_from(&mut buf) else {
            continue;
        };
        let Some(flow) = flow_for(shared, from) else {
            continue;
        };
        if flow.muted.load(Ordering::SeqCst) {
            continue;
        }
        let _ = flow.up.send(&buf[..n]);
    }
}

fn flow_for(shared: &Arc<Shared>, client: SocketAddr) -> Option<Arc<Flow>> {
    let mut flows = shared.flows.lock().unwrap();
    if let Some(f) = flows.get(&client) {
        return Some(Arc::clone(f));
    }
    let up = UdpSocket::bind("127.0.0.1:0").ok()?;
    up.connect(shared.upstream).ok()?;
    up.set_read_timeout(Some(POLL)).ok()?;
    let flow = Arc::new(Flow {
        up,
        client,
        muted: AtomicBool::new(false),
    });
    flows.insert(client, Arc::clone(&flow));
    let (s, f) = (Arc::clone(shared), Arc::clone(&flow));
    let h = thread::spawn(move || back_loop(&s, &f));
    shared.back.lock().unwrap().push(h);
    Some(flow)
}

fn back_loop(shared: &Shared, flow: &Flow) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::SeqCst) {
        let Ok(n) = flow.up.recv(&mut buf) else {
            continue;
        };
        if flow.muted.load(Ordering::SeqCst) {
            continue;
        }
        let _ = shared.front.send_to(&buf[..n], flow.client);
    }
}
