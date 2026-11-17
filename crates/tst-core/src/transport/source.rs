//! The datagram source a receiving transport reports, and the optional pin
//! that restricts it.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Which sender a receiver accepts datagrams from.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePin {
    /// Only this IP address (any port). Held in canonical form.
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

#[derive(Debug)]
struct Shared {
    pin: Option<SourcePin>,
    latched: OnceLock<IpAddr>,
    peer: Mutex<Option<SocketAddr>>,
    rejected: AtomicU64,
}

/// A receiver's view of where its datagrams come from. Clones share one
/// state, so a binding can read it after the transport moved into a shell.
#[derive(Debug, Clone)]
pub struct SourceHandle(Arc<Shared>);

impl Default for SourceHandle {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SourceHandle {
    /// A handle enforcing `pin`, or recording only when `pin` is `None`.
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

    /// The pin this handle was made with.
    #[must_use]
    pub fn pin(&self) -> Option<SourcePin> {
        self.0.pin
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

    /// Store `from` (canonical IP, same port) as the delivering address.
    pub fn record(&self, from: SocketAddr) {
        let canon = SocketAddr::new(from.ip().to_canonical(), from.port());
        *self.0.peer.lock().unwrap_or_else(PoisonError::into_inner) = Some(canon);
    }

    /// Count one datagram refused for its source.
    pub fn count_rejected(&self) {
        self.0.rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// The address that last delivered, if one was recorded.
    #[must_use]
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        *self.0.peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Datagrams refused for their source so far.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.0.rejected.load(Ordering::Relaxed)
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
            SourceHandle::new(mapped).pin(),
            Some(SourcePin::Ip(ip("10.0.0.5")))
        );
        for bad in ["camera.local", "", "[::1]", "First "] {
            assert_eq!(SourcePin::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn no_pin_allows_everyone_and_counts_nothing() {
        let h = SourceHandle::new(None);
        assert!(h.allows(ip("10.0.0.5")));
        assert!(h.admit(sa("10.0.0.6:1")));
        assert!(h.admit(sa("[fe80::1]:1")));
        assert_eq!(h.rejected(), 0);
        assert_eq!(h.pin(), None);
        assert_eq!(SourceHandle::default().pin(), None);
    }

    #[test]
    fn ip_pin_admits_the_address_in_both_forms() {
        let h = SourceHandle::new(Some(SourcePin::Ip(ip("10.0.0.5"))));
        assert!(h.admit(sa("[::ffff:10.0.0.5]:9")));
        assert!(!h.admit(sa("10.0.0.6:9")));
        assert_eq!(h.rejected(), 1);
    }

    #[test]
    fn first_latches_on_the_first_admit_only() {
        let h = SourceHandle::new(Some(SourcePin::First));
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
        let h = SourceHandle::default();
        assert_eq!(h.peer_addr(), None);
        h.record(sa("[::ffff:10.0.0.5]:9"));
        assert_eq!(h.peer_addr(), Some(sa("10.0.0.5:9")));
    }

    #[test]
    fn admit_does_not_record() {
        let h = SourceHandle::default();
        assert!(h.admit(sa("10.0.0.5:9")));
        assert_eq!(h.peer_addr(), None);
    }

    #[test]
    fn clones_share_state() {
        let h = SourceHandle::new(Some(SourcePin::First));
        let c = h.clone();
        assert!(h.admit(sa("10.0.0.5:1")));
        assert!(!c.allows(ip("10.0.0.6")));
        c.count_rejected();
        assert_eq!(h.rejected(), 1);
        c.record(sa("10.0.0.5:1"));
        assert_eq!(h.peer_addr(), Some(sa("10.0.0.5:1")));
    }

    #[test]
    fn a_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SourceHandle>();
    }
}
