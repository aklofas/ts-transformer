//! The datagram source a receiving transport reports, and the optional pin
//! that restricts it.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Which sender a receiver accepts datagrams from.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePin {
    /// Only this IP address (any port), compared in canonical form: an
    /// IPv4-mapped IPv6 address matches the IPv4 address.
    Ip(IpAddr),
    /// Whichever address delivers first; every other one is refused after.
    First,
}

impl SourcePin {
    /// Reads `first` or an IP literal. Anything else, including a host name
    /// or a bracketed address, is `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s == "first" {
            return Some(Self::First);
        }
        s.parse::<IpAddr>()
            .ok()
            .map(|ip| Self::Ip(ip.to_canonical()))
    }
}

/// Prints the URL value: the IP address in canonical form, or `first`.
impl std::fmt::Display for SourcePin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ip(ip) => ip.to_canonical().fmt(f),
            Self::First => f.write_str("first"),
        }
    }
}

#[derive(Debug)]
struct Shared {
    pin: Option<SourcePin>,
    latched: OnceLock<IpAddr>,
    peer: Mutex<Option<SocketAddr>>,
    rejected: AtomicU64,
}

impl Shared {
    fn peer_addr(&self) -> Option<SocketAddr> {
        *self.peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

/// A receiver's view of where its datagrams come from. Clones share one
/// state, so a binding can read it after the transport moved into a shell.
///
/// It only reads: the transport writes through its [`SourceRecord`].
///
/// ```compile_fail,E0599
/// fn writes(h: &tst_core::transport::SourceHandle) {
///     let _ = h.admit("192.0.2.1:9".parse().unwrap());
/// }
/// ```
/// ```compile_fail,E0599
/// fn writes(h: &tst_core::transport::SourceHandle) {
///     h.record("192.0.2.1:9".parse().unwrap());
/// }
/// ```
/// ```compile_fail,E0599
/// fn writes(h: &tst_core::transport::SourceHandle) {
///     h.count_rejected();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct SourceHandle(Arc<Shared>);

impl SourceHandle {
    /// The pin the receiver was made with.
    #[must_use]
    pub fn pin(&self) -> Option<SourcePin> {
        self.0.pin
    }

    /// The address that last delivered, if one was recorded. It is the
    /// datagrams' source address, which nothing verifies.
    #[must_use]
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.0.peer_addr()
    }

    /// Datagrams refused for their source so far.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.0.rejected()
    }
}

/// The writing side of a receiver's source record, for transport
/// implementations: it applies the pin, counts refusals and stores the
/// delivering address. Readers get a [`SourceHandle`] from
/// [`Self::handle`], which cannot write.
#[derive(Debug)]
pub struct SourceRecord(Arc<Shared>);

impl Default for SourceRecord {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SourceRecord {
    /// A record enforcing `pin`, or recording only when `pin` is `None`.
    #[must_use]
    pub fn new(pin: Option<SourcePin>) -> Self {
        let pin = match pin {
            Some(SourcePin::Ip(ip)) => Some(SourcePin::Ip(ip.to_canonical())),
            other => other,
        };
        Self(Arc::new(Shared {
            pin,
            latched: OnceLock::new(),
            peer: Mutex::new(None),
            rejected: AtomicU64::new(0),
        }))
    }

    /// A read-only handle on this record, for a transport's
    /// `source_handle()`.
    #[must_use]
    pub fn handle(&self) -> SourceHandle {
        SourceHandle(Arc::clone(&self.0))
    }

    /// The pin this record was made with.
    #[must_use]
    pub fn pin(&self) -> Option<SourcePin> {
        self.0.pin
    }

    /// The address that last delivered, as [`SourceHandle::peer_addr`].
    #[must_use]
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.0.peer_addr()
    }

    /// Datagrams refused for their source so far.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.0.rejected()
    }

    /// Lock-free. No pin, or `First` not yet latched: true. Otherwise the
    /// canonical form of `ip` equals the pinned or latched IP.
    #[must_use]
    pub fn allows(&self, ip: IpAddr) -> bool {
        match self.0.pin {
            None => true,
            Some(SourcePin::Ip(want)) => ip.to_canonical() == want,
            Some(SourcePin::First) => self.0.latched.get().is_none_or(|l| ip.to_canonical() == *l),
        }
    }

    /// For a datagram about to be delivered. Not allowed: counts it and
    /// returns false. Allowed: latches a `First` pin to `from`'s IP and
    /// returns true. Lock-free. Does not record the address.
    #[must_use]
    pub fn admit(&self, from: SocketAddr) -> bool {
        let ip = from.ip().to_canonical();
        if self.0.pin == Some(SourcePin::First) {
            // Racing threads agree on one winner; the loser is judged against it.
            let latched = *self.0.latched.get_or_init(|| ip);
            if latched != ip {
                self.count_rejected();
                return false;
            }
            return true;
        }
        if self.allows(ip) {
            true
        } else {
            self.count_rejected();
            false
        }
    }

    /// The form this record stores and reports: an IPv4-mapped IPv6 address
    /// becomes the IPv4 address with the same port; any other address, an
    /// IPv6 scope id included, is kept as it is. A pin compares the IP alone.
    #[must_use]
    pub fn canonical(addr: SocketAddr) -> SocketAddr {
        match addr {
            SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
                Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
                None => addr,
            },
            SocketAddr::V4(_) => addr,
        }
    }

    /// Store `from`, in [`Self::canonical`] form, as the delivering address.
    pub fn record(&self, from: SocketAddr) {
        let canon = Self::canonical(from);
        *self.0.peer.lock().unwrap_or_else(PoisonError::into_inner) = Some(canon);
    }

    /// Count one datagram refused for its source.
    pub fn count_rejected(&self) {
        self.0.rejected.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parse_reads_first_and_ip_literals() {
        assert_eq!(SourcePin::parse("first"), Some(SourcePin::First));
        assert_eq!(
            SourcePin::parse("10.0.0.5"),
            Some(SourcePin::Ip(ip("10.0.0.5")))
        );
        assert_eq!(
            SourcePin::parse("fe80::1"),
            Some(SourcePin::Ip(ip("fe80::1")))
        );
        let mapped = SourcePin::parse("::ffff:10.0.0.5");
        assert!(matches!(mapped, Some(SourcePin::Ip(_))));
        assert_eq!(
            SourceRecord::new(mapped).pin(),
            Some(SourcePin::Ip(ip("10.0.0.5")))
        );
        for bad in ["camera.local", "", "[::1]", "First "] {
            assert_eq!(SourcePin::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn no_pin_allows_everyone_and_counts_nothing() {
        let h = SourceRecord::new(None);
        assert!(h.allows(ip("10.0.0.5")));
        assert!(h.admit(sa("10.0.0.6:1")));
        assert!(h.admit(sa("[fe80::1]:1")));
        assert_eq!(h.rejected(), 0);
        assert_eq!(h.pin(), None);
        assert_eq!(SourceRecord::default().pin(), None);
    }

    #[test]
    fn ip_pin_admits_the_address_in_both_forms() {
        let h = SourceRecord::new(Some(SourcePin::Ip(ip("10.0.0.5"))));
        assert!(h.admit(sa("[::ffff:10.0.0.5]:9")));
        assert!(!h.admit(sa("10.0.0.6:9")));
        assert_eq!(h.rejected(), 1);
    }

    #[test]
    fn allows_reads_both_forms_of_a_pinned_or_latched_address() {
        let h = SourceRecord::new(Some(SourcePin::Ip(ip("10.0.0.5"))));
        assert!(h.allows(ip("::ffff:10.0.0.5")));
        assert!(!h.allows(ip("::ffff:10.0.0.6")));
        let h = SourceRecord::new(Some(SourcePin::First));
        assert!(h.admit(sa("[::ffff:10.0.0.5]:9")));
        assert!(h.allows(ip("10.0.0.5")));
        assert!(h.allows(ip("::ffff:10.0.0.5")));
        assert!(!h.allows(ip("10.0.0.6")));
    }

    #[test]
    fn first_latches_on_the_first_admit_only() {
        let h = SourceRecord::new(Some(SourcePin::First));
        let (a, b) = (sa("10.0.0.5:1"), sa("10.0.0.6:1"));
        assert!(h.allows(b.ip()));
        assert!(h.admit(a));
        assert!(!h.admit(b));
        assert!(h.admit(sa("10.0.0.5:2")));
        assert!(!h.allows(b.ip()));
        assert_eq!(h.rejected(), 1);
    }

    #[test]
    fn peer_addr_is_none_until_recorded_and_canonical_after() {
        let h = SourceRecord::default();
        assert_eq!(h.peer_addr(), None);
        h.record(sa("[::ffff:10.0.0.5]:9"));
        assert_eq!(h.peer_addr(), Some(sa("10.0.0.5:9")));
    }

    #[test]
    fn record_keeps_an_ipv6_scope_id_and_maps_ipv4_back() {
        let h = SourceRecord::default();
        h.record(sa("[fe80::1%3]:9"));
        match h.peer_addr() {
            Some(SocketAddr::V6(v6)) => assert_eq!(v6.scope_id(), 3, "{v6}"),
            other => panic!("{other:?}"),
        }
        h.record(sa("[::ffff:10.0.0.5]:9"));
        assert_eq!(h.peer_addr(), Some(sa("10.0.0.5:9")));
    }

    #[test]
    fn canonical_maps_ipv4_back_and_keeps_everything_else() {
        assert_eq!(
            SourceRecord::canonical(sa("[::ffff:10.0.0.5]:9")),
            sa("10.0.0.5:9")
        );
        for keep in ["10.0.0.5:9", "[fe80::1%3]:9", "[::1]:9"] {
            assert_eq!(SourceRecord::canonical(sa(keep)), sa(keep), "{keep}");
        }
        match SourceRecord::canonical(sa("[fe80::1%3]:9")) {
            SocketAddr::V6(v6) => assert_eq!(v6.scope_id(), 3),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn display_prints_the_url_value_and_parses_back() {
        for pin in [
            SourcePin::First,
            SourcePin::Ip(ip("10.0.0.5")),
            SourcePin::Ip(ip("fe80::1")),
        ] {
            assert_eq!(SourcePin::parse(&pin.to_string()), Some(pin), "{pin}");
        }
        assert_eq!(SourcePin::First.to_string(), "first");
        let mapped = SourcePin::Ip(ip("::ffff:10.0.0.5"));
        assert_eq!(mapped.to_string(), "10.0.0.5");
        assert_eq!(
            SourcePin::parse(&mapped.to_string()),
            Some(SourcePin::Ip(ip("10.0.0.5")))
        );
    }

    #[test]
    fn admit_does_not_record() {
        let h = SourceRecord::default();
        assert!(h.admit(sa("10.0.0.5:9")));
        assert_eq!(h.peer_addr(), None);
    }

    #[test]
    fn a_handle_sees_what_its_record_wrote() {
        let r = SourceRecord::new(Some(SourcePin::First));
        let h = r.handle();
        let c = h.clone();
        assert_eq!(h.pin(), Some(SourcePin::First));
        assert!(r.admit(sa("10.0.0.5:1")));
        assert!(!r.admit(sa("10.0.0.6:1")));
        r.count_rejected();
        r.record(sa("[::ffff:10.0.0.5]:1"));
        for h in [&h, &c, &r.handle()] {
            assert_eq!(h.rejected(), 2);
            assert_eq!(h.peer_addr(), Some(sa("10.0.0.5:1")));
        }
    }

    #[test]
    fn two_threads_racing_to_latch_agree_on_one_winner() {
        use std::sync::Barrier;
        const N: u64 = 1_000;
        let h = SourceRecord::new(Some(SourcePin::First));
        let barrier = Barrier::new(2);
        let run = |addr: SocketAddr| {
            let h = &h;
            let barrier = &barrier;
            move || {
                barrier.wait();
                (0..N).filter(|_| !h.admit(addr)).count() as u64
            }
        };
        let (a, b) = (sa("10.0.0.5:1"), sa("10.0.0.6:1"));
        let (refused_a, refused_b) = std::thread::scope(|s| {
            let ta = s.spawn(run(a));
            let tb = s.spawn(run(b));
            (ta.join().unwrap(), tb.join().unwrap())
        });
        assert_ne!(h.allows(a.ip()), h.allows(b.ip()));
        let loser_refusals = refused_a + refused_b;
        assert_eq!(loser_refusals, N);
        assert_eq!(h.rejected(), loser_refusals);
    }

    #[test]
    fn a_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SourceHandle>();
        assert_send_sync::<SourceRecord>();
    }
}
