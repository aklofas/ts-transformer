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

/// Most peers remembered at once. librist makes a peer per source port, so
/// a sender cycling ports cannot grow the table past this.
pub(crate) const MAX_PEERS: usize = 256;

/// What the connect callback decided for a new peer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Accepted and its address remembered.
    Recorded,
    /// Accepted without remembering it: no pin is set, and the table is full
    /// or the text is not an address. Its blocks are delivered as before
    /// this table existed.
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
    let pinned = source.pin().is_some();
    let Ok(ip) = ip_text.parse::<IpAddr>() else {
        return if pinned {
            Verdict::Declined
        } else {
            Verdict::Unrecorded
        };
    };
    let addr = SocketAddr::new(ip.to_canonical(), port);
    // `allows`, not `admit`: a peer is created before its key is proven, so
    // only a delivered block may latch a `first` pin.
    if !source.allows(addr.ip()) {
        source.count_rejected();
        return Verdict::Declined;
    }
    if table.len() >= MAX_PEERS && !table.contains_key(&peer) {
        if pinned {
            source.count_rejected();
            return Verdict::Declined;
        }
        return Verdict::Unrecorded;
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
        }
    }

    /// Read before [`Self::lookup`]: a change after this read leaves the
    /// pair just looked up with an older generation, so it is re-read next
    /// time rather than trusted.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn lookup(&self, peer: usize) -> Option<SocketAddr> {
        self.lock().get(&peer).copied()
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
    let verdict = catch_unwind(AssertUnwindSafe(|| {
        if arg.is_null() {
            return Verdict::Declined;
        }
        // SAFETY: arg is the live leaked reference from `register`; borrowed.
        let peers = unsafe { &*(arg as *const Peers) };
        let text = if conn_ip.is_null() {
            ""
        } else {
            // SAFETY: librist passes a NUL-terminated buffer valid for the call.
            unsafe { CStr::from_ptr(conn_ip) }.to_str().unwrap_or("")
        };
        peers.connect(peer as usize, text, conn_port)
    }))
    .unwrap_or(Verdict::Declined);
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
    fn the_table_is_capped_at_256() {
        let mut t = HashMap::new();
        let h = pinned("127.0.0.1");
        for peer in 0..MAX_PEERS {
            assert_eq!(
                on_connect(&mut t, &h, peer, "127.0.0.1", 1000 + peer as u16),
                Verdict::Recorded
            );
        }
        assert_eq!(t.len(), 256);
        assert_eq!(
            on_connect(&mut t, &h, 9999, "127.0.0.1", 9999),
            Verdict::Declined
        );
        assert_eq!(h.rejected(), 1);
        assert_eq!(t.len(), 256);
        // A peer already in the table is not a new entry.
        assert_eq!(
            on_connect(&mut t, &h, 7, "127.0.0.1", 7777),
            Verdict::Recorded
        );
        assert_eq!(t.get(&7), Some(&sa("127.0.0.1:7777")));
        // A disconnect frees a place.
        assert!(on_disconnect(&mut t, 0));
        assert_eq!(
            on_connect(&mut t, &h, 9999, "127.0.0.1", 9999),
            Verdict::Recorded
        );
        assert_eq!(h.rejected(), 1);

        // Without a pin a full table accepts the peer and does not record it.
        let open = SourceHandle::default();
        assert_eq!(
            on_connect(&mut t, &open, 12345, "10.0.0.9", 1),
            Verdict::Unrecorded
        );
        assert_eq!(open.rejected(), 0);
        assert_eq!(t.len(), 256);
        assert!(!t.contains_key(&12345));
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
