//! A receiver's record of which sender each librist peer is, kept by
//! librist's connect and disconnect callbacks, and the `source` pin applied
//! to it.
//!
//! librist makes a peer for every new datagram source (IP and UDP port) and
//! tags each delivered block with that peer's pointer. The connect callback
//! learns the source's address as text; this table maps the pointer to it so
//! the read path can name, and filter, the sender of each block.

use std::collections::HashMap;
use std::ffi::CStr;
use std::net::{IpAddr, SocketAddr};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tst_core::transport::SourceHandle;

/// What the connect callback decided for a new peer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Accepted and its address remembered.
    Recorded,
    /// Accepted without remembering it: the address text did not parse and
    /// no pin is set. Its blocks are delivered as before this table existed.
    Unrecorded,
    /// Declined: librist keeps its data out.
    Declined,
}

/// Decide on a peer librist just created for the datagram source
/// `ip_text`:`port`. Without a pin nothing is declined, so a receiver
/// without `source` delivers what it always did.
pub(crate) fn on_connect(
    table: &mut HashMap<usize, SocketAddr>,
    source: &SourceHandle,
    peer: usize,
    ip_text: &str,
    port: u16,
) -> Verdict {
    let Ok(ip) = ip_text.parse::<IpAddr>() else {
        if source.pin().is_some() {
            source.count_rejected();
            return Verdict::Declined;
        }
        return Verdict::Unrecorded;
    };
    let addr = SocketAddr::new(ip.to_canonical(), port);
    // `allows`, not `admit`: a peer is created before its key is proven, so
    // only a delivered block may latch a `first` pin.
    if !source.allows(addr.ip()) {
        source.count_rejected();
        return Verdict::Declined;
    }
    table.insert(peer, addr);
    Verdict::Recorded
}

/// Forget a peer librist freed. True when it was in the table.
pub(crate) fn on_disconnect(table: &mut HashMap<usize, SocketAddr>, peer: usize) -> bool {
    table.remove(&peer).is_some()
}

/// The table and the pin of one receiver, shared with librist's callbacks.
pub(crate) struct Peers {
    /// At most one entry per peer librist holds: made when the connect callback
    /// accepts it, removed when the disconnect callback fires. It has no cap
    /// of its own: librist allocates far more per peer than an entry costs,
    /// so a cap here bounds nothing librist does not, and a full table would
    /// let peers that never deliver keep the real sender out.
    table: Mutex<HashMap<usize, SocketAddr>>,
    /// Bumped, under the table lock, at every change of the table. A reader
    /// that cached a (pointer, address) pair re-reads the table when it moves,
    /// which also covers librist reusing a freed peer's pointer.
    generation: AtomicU64,
    pub(crate) source: SourceHandle,
}

impl Peers {
    pub(crate) fn new(source: SourceHandle) -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            source,
        }
    }

    /// The table, also after a panic poisoned its lock: every change to it
    /// is a single insert or remove, so a poisoned table is still whole.
    fn lock(&self) -> MutexGuard<'_, HashMap<usize, SocketAddr>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn connect(&self, peer: usize, ip_text: &str, port: u16) -> Verdict {
        let mut table = self.lock();
        let v = on_connect(&mut table, &self.source, peer, ip_text, port);
        if v == Verdict::Recorded {
            self.generation.fetch_add(1, Ordering::Release);
        }
        v
    }

    fn disconnect(&self, peer: usize) {
        let mut table = self.lock();
        if on_disconnect(&mut table, peer) {
            self.generation.fetch_add(1, Ordering::Release);
            // A map keeps its peak capacity; once a flood of peers has gone,
            // give it back rather than hold it for the receiver's life.
            if table.is_empty() {
                table.shrink_to_fit();
            }
        }
    }

    /// Register the callbacks on `ctx` (a receiver, not yet started) with one
    /// leaked reference to `peers` as their argument. Returns that argument,
    /// to hand to [`Self::reclaim`] exactly once after `rist_destroy`.
    pub(crate) fn register(ctx: *mut rist_sys::rist_ctx, peers: &Arc<Self>) -> *mut c_void {
        let arg = Arc::into_raw(peers.clone()) as *mut c_void;
        // SAFETY: ctx is a live context whose protocol thread has not started;
        // arg stays valid until reclaimed after rist_destroy. A non-zero rc
        // leaves the callbacks unset: no address is learned and, with a pin,
        // every block is refused on the read path.
        let _rc = unsafe {
            rist_sys::rist_auth_handler_set(
                ctx,
                Some(connect_trampoline),
                Some(disconnect_trampoline),
                arg,
            )
        };
        arg
    }

    /// Drop the reference [`Self::register`] leaked.
    ///
    /// # Safety
    /// `arg` came from [`Self::register`], is reclaimed once, and librist can
    /// no longer call the callbacks (`rist_destroy` returned, or `rist_start`
    /// never ran).
    pub(crate) unsafe fn reclaim(arg: *mut c_void) {
        unsafe { drop(Arc::from_raw(arg as *const Self)) };
    }
}

/// The read path's view of the table: which sender a delivered block came
/// from, through a one-entry cache, and whether the pin lets it through.
#[derive(Default)]
pub(crate) struct Attribution {
    /// The peer of the last block, the table's generation when it was looked
    /// up, and the address found. A sender's blocks arrive in runs from one
    /// peer, so the table lock is taken only when the pointer or the
    /// generation moves, not once per block.
    last_peer: Option<(usize, u64, Option<SocketAddr>)>,
    /// The address last stored in the source handle; it is written only when
    /// the delivering address changes.
    recorded: Option<SocketAddr>,
}

impl Attribution {
    /// The address the table holds for `peer`.
    ///
    /// Known limit, accepted: librist keeps a block's peer pointer after the
    /// peer is freed, so a block read after its peer's entry was replaced by a
    /// new peer at the same pointer is attributed to the new peer. librist
    /// frees a peer only after hearing nothing from it for its liveness
    /// timeout (the larger of the session timeout and twice the recovery
    /// buffer) and then holding it dead for 5 s (`rist_timeout_check`), so
    /// the reader must leave the old peer's blocks queued at least that long.
    /// Under an unlatched `first`, such a stale block can latch the pin to the
    /// new peer's address; under an IP pin every entry carries the pinned IP.
    fn address(&mut self, peers: &Peers, peer: usize) -> Option<SocketAddr> {
        // Read before the lookup: a change landing in between leaves the pair
        // with an older generation, so it is re-read next time.
        let generation = peers.generation.load(Ordering::Acquire);
        if let Some((p, g, addr)) = self.last_peer
            && p == peer
            && g == generation
        {
            return addr;
        }
        let addr = peers.lock().get(&peer).copied();
        self.last_peer = Some((peer, generation, addr));
        addr
    }

    /// Apply the `source` pin to a block from `peer` about to be delivered,
    /// and record its sender. False: the block is refused (and counted).
    pub(crate) fn admit_block(&mut self, peers: &Peers, peer: usize) -> bool {
        let source = &peers.source;
        match self.address(peers, peer) {
            Some(addr) => {
                if !source.admit(addr) {
                    return false;
                }
                if self.recorded != Some(addr) {
                    source.record(addr);
                    self.recorded = Some(addr);
                }
                true
            }
            // A peer the table does not hold (its address text did not parse,
            // or it was freed since): without a pin it is delivered as
            // before, unrecorded.
            None if source.pin().is_some() => {
                source.count_rejected();
                false
            }
            None => true,
        }
    }
}

/// librist's connect callback, on its protocol thread: 0 accepts the peer,
/// non-zero declines it. Nothing unwinds out of it.
unsafe extern "C" fn connect_trampoline(
    arg: *mut c_void,
    conn_ip: *const c_char,
    conn_port: u16,
    _local_ip: *const c_char,
    _local_port: u16,
    peer: *mut rist_sys::rist_peer,
) -> c_int {
    // Never null (`register` always passes one); with no table there is no
    // pin to apply either.
    if arg.is_null() {
        return 0;
    }
    // SAFETY: arg is the live leaked reference from `register`; borrowed.
    let peers = unsafe { &*(arg as *const Peers) };
    let verdict = catch_unwind(AssertUnwindSafe(|| {
        let text = if conn_ip.is_null() {
            ""
        } else {
            // SAFETY: librist passes a NUL-terminated buffer valid for the call.
            unsafe { CStr::from_ptr(conn_ip) }.to_str().unwrap_or("")
        };
        peers.connect(peer as usize, text, conn_port)
    }))
    // A receiver without a pin refuses nothing, even after a panic.
    .unwrap_or_else(|_| {
        if peers.source.pin().is_some() {
            Verdict::Declined
        } else {
            Verdict::Unrecorded
        }
    });
    if verdict == Verdict::Declined { -1 } else { 0 }
}

/// librist's disconnect callback, on its protocol thread and in
/// `rist_destroy`. Nothing unwinds out of it.
unsafe extern "C" fn disconnect_trampoline(
    arg: *mut c_void,
    peer: *mut rist_sys::rist_peer,
) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !arg.is_null() {
            // SAFETY: as in connect_trampoline.
            unsafe { &*(arg as *const Peers) }.disconnect(peer as usize);
        }
    }));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::SourcePin;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn pinned(ip: &str) -> SourceHandle {
        SourceHandle::new(Some(SourcePin::Ip(ip.parse().unwrap())))
    }

    #[test]
    fn connect_accepts_and_records_the_canonical_address() {
        let mut t = HashMap::new();
        let h = SourceHandle::default();
        assert_eq!(
            on_connect(&mut t, &h, 1, "::ffff:127.0.0.1", 5000),
            Verdict::Recorded
        );
        assert_eq!(
            on_connect(&mut t, &h, 2, "fe80::1", 5001),
            Verdict::Recorded
        );
        assert_eq!(t.get(&1), Some(&sa("127.0.0.1:5000")));
        assert_eq!(t.get(&2), Some(&sa("[fe80::1]:5001")));
        assert_eq!(h.rejected(), 0);
        // The connect path records the table only: the handle's address is
        // the one that delivered, set on the read path.
        assert_eq!(h.peer_addr(), None);
    }

    #[test]
    fn connect_declines_a_foreign_ip_under_a_pin_and_counts_it() {
        let mut t = HashMap::new();
        let h = pinned("127.0.0.1");
        assert_eq!(
            on_connect(&mut t, &h, 1, "127.0.0.2", 5000),
            Verdict::Declined
        );
        assert_eq!(h.rejected(), 1);
        assert!(t.is_empty());
        // The pinned address in its mapped form is accepted.
        assert_eq!(
            on_connect(&mut t, &h, 2, "::ffff:127.0.0.1", 5001),
            Verdict::Recorded
        );
        assert_eq!(h.rejected(), 1);

        // `first` is judged against the latched address only; the connect
        // path never latches (a sender with a wrong key connects too).
        let first = SourceHandle::new(Some(SourcePin::First));
        assert_eq!(
            on_connect(&mut t, &first, 3, "127.0.0.2", 5002),
            Verdict::Recorded
        );
        assert!(first.allows("127.0.0.1".parse().unwrap()));
        assert!(first.admit(sa("127.0.0.1:5000")));
        assert_eq!(
            on_connect(&mut t, &first, 4, "127.0.0.2", 5003),
            Verdict::Declined
        );
        assert_eq!(first.rejected(), 1);
    }

    #[test]
    fn connect_declines_unparseable_text() {
        for text in ["", "camera.local", "127.0.0.1:5000", "[::1]"] {
            let mut t = HashMap::new();
            let h = pinned("127.0.0.1");
            assert_eq!(on_connect(&mut t, &h, 1, text, 5000), Verdict::Declined);
            assert!(t.is_empty(), "{text:?}");
            assert_eq!(h.rejected(), 1, "{text:?}");
            let first = SourceHandle::new(Some(SourcePin::First));
            assert_eq!(on_connect(&mut t, &first, 1, text, 5000), Verdict::Declined);
            // Without a pin nothing is turned away: the peer is delivered as
            // before, unrecorded.
            let open = SourceHandle::default();
            assert_eq!(
                on_connect(&mut t, &open, 1, text, 5000),
                Verdict::Unrecorded
            );
            assert!(t.is_empty(), "{text:?}");
            assert_eq!(open.rejected(), 0);
        }
    }

    #[test]
    fn many_peers_from_one_ip_never_keep_another_out() {
        const MANY: usize = 10_000;
        // `first`, not yet latched: every peer of the first IP is recorded,
        // and a second IP still gets in.
        let mut t = HashMap::new();
        let first = SourceHandle::new(Some(SourcePin::First));
        for peer in 0..MANY {
            assert_eq!(
                on_connect(&mut t, &first, peer, "127.0.0.2", peer as u16),
                Verdict::Recorded,
                "peer {peer}"
            );
        }
        assert_eq!(
            on_connect(&mut t, &first, MANY, "127.0.0.1", 5000),
            Verdict::Recorded
        );
        assert_eq!(t.len(), MANY + 1);
        assert_eq!(first.rejected(), 0);

        // An IP pin: the pinned address is recorded however many peers it has.
        let mut t = HashMap::new();
        let ip = pinned("127.0.0.1");
        for peer in 0..=MANY {
            assert_eq!(
                on_connect(&mut t, &ip, peer, "127.0.0.1", peer as u16),
                Verdict::Recorded,
                "peer {peer}"
            );
        }
        assert_eq!(t.len(), MANY + 1);
        assert_eq!(ip.rejected(), 0);
    }

    #[test]
    fn an_unknown_peer_is_refused_under_a_pin_and_delivered_without() {
        let pinned = Peers::new(pinned("127.0.0.1"));
        assert!(!Attribution::default().admit_block(&pinned, 7));
        assert_eq!(pinned.source.rejected(), 1);
        assert_eq!(pinned.source.peer_addr(), None);

        let open = Peers::new(SourceHandle::default());
        assert!(Attribution::default().admit_block(&open, 7));
        assert_eq!(open.source.rejected(), 0);
        assert_eq!(open.source.peer_addr(), None);
    }

    #[test]
    fn a_pointer_reused_after_a_disconnect_reports_the_new_address() {
        let peers = Peers::new(SourceHandle::default());
        let mut a = Attribution::default();
        assert_eq!(peers.connect(1, "127.0.0.1", 5000), Verdict::Recorded);
        assert!(a.admit_block(&peers, 1));
        assert_eq!(peers.source.peer_addr(), Some(sa("127.0.0.1:5000")));
        // librist frees the peer and allocates a new one at the same pointer.
        peers.disconnect(1);
        assert_eq!(peers.connect(1, "127.0.0.2", 6000), Verdict::Recorded);
        assert!(a.admit_block(&peers, 1));
        assert_eq!(peers.source.peer_addr(), Some(sa("127.0.0.2:6000")));
    }

    #[test]
    fn an_emptied_table_gives_its_memory_back() {
        const MANY: usize = 10_000;
        let peers = Peers::new(SourceHandle::default());
        for peer in 0..MANY {
            assert_eq!(
                peers.connect(peer, "127.0.0.1", peer as u16),
                Verdict::Recorded
            );
        }
        assert!(peers.lock().capacity() >= MANY);
        for peer in 0..MANY {
            peers.disconnect(peer);
        }
        let left = peers.lock().capacity();
        assert!(left < 64, "capacity {left} kept after the table emptied");
    }

    #[test]
    fn disconnect_forgets_the_peer() {
        let mut t = HashMap::new();
        let h = SourceHandle::default();
        assert_eq!(
            on_connect(&mut t, &h, 1, "127.0.0.1", 5000),
            Verdict::Recorded
        );
        assert_eq!(
            on_connect(&mut t, &h, 2, "127.0.0.2", 5000),
            Verdict::Recorded
        );
        assert!(on_disconnect(&mut t, 1));
        assert_eq!(t.get(&1), None);
        assert_eq!(t.get(&2), Some(&sa("127.0.0.2:5000")));
        // Forgetting an unknown peer is a no-op.
        assert!(!on_disconnect(&mut t, 42));
        assert_eq!(t.len(), 1);
    }
}
