//! Per-stream-ID rules a listener consults during each SRT handshake.
//!
//! Every [`SrtListener`](crate::SrtListener) installs libsrt's listen
//! callback before it starts listening. The callback looks the caller's
//! stream ID up in the listener's [`ListenRules`] and accepts, accepts with
//! a passphrase, or rejects with a reason the caller reads. A listener whose
//! table was never edited accepts every caller, as libsrt does by default.

use crate::error::{OptionError, RejectReason, last_error};
use crate::options::Passphrase;
use crate::socket::set_passphrase;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The lowest reject code an application may set (`SRT_REJC_PREDEFINED`);
/// libsrt reserves everything below for itself.
const FIRST_APPLICATION_REASON: i32 = 1000;

/// What the listener does with a caller whose stream ID matched a rule, or
/// with any other caller when this is the default rule.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum ListenRule {
    /// Accept with the listener's own settings (libsrt's default behavior).
    Accept,
    /// Accept, with this passphrase applied to the connection. libsrt then
    /// rejects a caller whose key material does not match with
    /// [`RejectReason::BadSecret`]. The key length is negotiated from the
    /// caller's key material (libsrt HSv5), so a rule cannot set it; the
    /// listener-wide `SrtListenerBuilder::key_length` only advertises a
    /// preference to callers that leave theirs unset.
    AcceptWith {
        /// The passphrase the connection must use.
        passphrase: Passphrase,
    },
    /// Refuse during the handshake; the caller reads `reason`. Only
    /// predefined (`1000..=1999`) or user-defined (`2000..`) reasons are
    /// allowed.
    Reject(RejectReason),
}

/// Why a [`ListenRules`] edit was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ListenRuleError {
    /// A [`ListenRule::Reject`] with a reason below 1000: those codes belong
    /// to libsrt.
    #[error("reject reason {0:?} is reserved for libsrt (codes below 1000)")]
    InternalReason(RejectReason),
    /// The listener this table belongs to is closed.
    #[error("listener is closed")]
    Closed,
}

/// Handle to one listener's rules, from
/// [`SrtListener::rules`](crate::SrtListener::rules).
///
/// A clone is the same table. Usable from any thread, including while
/// another thread is parked in `accept()` (obtain it before the accept loop
/// starts, as with `cancel_handle()`). An edit takes effect for the next
/// handshake. Once the listener is closed, `set` and `set_default` return
/// [`ListenRuleError::Closed`] and `remove` returns `false`; `clear` still
/// succeeds on the dead table and `len` stays readable.
///
/// Matching is exact on the stream ID. A caller with no stream ID, an empty
/// one, or one that is not UTF-8 gets the default rule.
#[derive(Clone)]
pub struct ListenRules {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    table: RwLock<Table>,
    closed: AtomicBool,
}

pub(crate) struct Table {
    rules: HashMap<String, ListenRule>,
    default: ListenRule,
}

impl std::fmt::Debug for ListenRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.inner.read();
        let default = match &t.default {
            ListenRule::Accept => "Accept",
            ListenRule::AcceptWith { .. } => "AcceptWith",
            ListenRule::Reject(_) => "Reject",
        };
        f.debug_struct("ListenRules")
            .field("rules", &t.rules.len())
            .field("default", &default)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl Inner {
    fn new() -> Self {
        Self {
            table: RwLock::new(Table {
                rules: HashMap::new(),
                default: ListenRule::Accept,
            }),
            closed: AtomicBool::new(false),
        }
    }

    // Poison is recovered: every write is a single map operation or
    // assignment, so a panic cannot leave the table torn.
    fn read(&self) -> RwLockReadGuard<'_, Table> {
        self.table.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Table> {
        self.table.write().unwrap_or_else(|e| e.into_inner())
    }

    fn check_open(&self) -> Result<(), ListenRuleError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ListenRuleError::Closed);
        }
        Ok(())
    }
}

fn validate(rule: &ListenRule) -> Result<(), ListenRuleError> {
    match rule {
        ListenRule::Reject(r) if r.raw() < FIRST_APPLICATION_REASON => {
            Err(ListenRuleError::InternalReason(*r))
        }
        _ => Ok(()),
    }
}

impl ListenRules {
    /// Set the rule for callers whose stream ID is exactly `stream_id`,
    /// replacing any rule already there. A rule for the empty string is
    /// stored but never matches: an empty stream ID gets the default.
    pub fn set(&self, stream_id: &str, rule: ListenRule) -> Result<(), ListenRuleError> {
        validate(&rule)?;
        self.inner.check_open()?;
        self.inner.write().rules.insert(stream_id.to_owned(), rule);
        Ok(())
    }

    /// Remove the rule for `stream_id`. Returns `false` when there was none
    /// or the listener is closed.
    pub fn remove(&self, stream_id: &str) -> bool {
        if self.inner.check_open().is_err() {
            return false;
        }
        self.inner.write().rules.remove(stream_id).is_some()
    }

    /// Set the rule for callers no explicit rule matches. It starts as
    /// [`ListenRule::Accept`].
    pub fn set_default(&self, rule: ListenRule) -> Result<(), ListenRuleError> {
        validate(&rule)?;
        self.inner.check_open()?;
        self.inner.write().default = rule;
        Ok(())
    }

    /// Remove every explicit rule. The default rule stays. Succeeds on a
    /// closed table too, where it only empties the dead table.
    pub fn clear(&self) {
        self.inner.write().rules.clear();
    }

    /// The number of explicit rules (the default is not counted).
    pub fn len(&self) -> usize {
        self.inner.read().rules.len()
    }

    /// Whether there are no explicit rules.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A table bound to no socket.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }
}

/// The table of every open listener, keyed by its listening socket. The
/// hook's opaque pointer carries the `SRTSOCKET`, never an `Arc`: libsrt can
/// still run the hook for an in-flight handshake while the listener closes
/// on another thread, and a missing entry then simply accepts.
static REGISTRY: LazyLock<Mutex<HashMap<srt_sys::SRTSOCKET, Arc<Inner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// Poison is recovered: every critical section is a single map operation.
fn registry() -> std::sync::MutexGuard<'static, HashMap<srt_sys::SRTSOCKET, Arc<Inner>>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Insert the registry entry for `lsn` and install the trampoline. Called by
/// `SrtListener::bind_with` after `srt_bind`, before `srt_listen`.
pub(crate) fn install(lsn: srt_sys::SRTSOCKET) -> Result<ListenRules, OptionError> {
    let inner = Arc::new(Inner::new());
    registry().insert(lsn, inner.clone());
    // SAFETY: `lsn` is a live socket the caller owns; `listen_hook` matches
    // `srt_listen_callback_fn` and the opaque value is a plain integer.
    let rc = unsafe {
        srt_sys::srt_listen_callback(lsn, Some(listen_hook), lsn as usize as *mut c_void)
    };
    if rc < 0 {
        let err = last_error().into();
        registry().remove(&lsn);
        return Err(err);
    }
    Ok(ListenRules { inner })
}

/// Remove the entry (if any) and flip `closed`. Called before every
/// `srt_close` in `exit_guard`. No-op for sockets that are not listeners.
///
/// Never logs: the exit handler's drain calls this after glibc destroyed the
/// exiting thread's thread-locals.
pub(crate) fn uninstall(handle: srt_sys::SRTSOCKET) {
    let removed = registry().remove(&handle);
    if let Some(inner) = removed {
        inner.closed.store(true, Ordering::Release);
    }
}

/// The decision for one handshake. `None`, an empty stream ID (handshake
/// v4 included) and any stream ID that is not UTF-8 get the default rule.
pub(crate) fn decide(table: &Table, stream_id: Option<&[u8]>) -> ListenRule {
    stream_id
        .and_then(|b| std::str::from_utf8(b).ok())
        // libsrt hands a handshake-v4 caller an empty string, not NULL.
        .filter(|id| !id.is_empty())
        .and_then(|id| table.rules.get(id))
        .unwrap_or(&table.default)
        .clone()
}

/// Apply `rule` to the incoming socket `ns`: 0 accepts, -1 rejects (the
/// reason is already set on `ns`).
pub(crate) fn apply(ns: srt_sys::SRTSOCKET, rule: &ListenRule) -> c_int {
    match rule {
        ListenRule::Accept => 0,
        ListenRule::AcceptWith { passphrase } => match set_passphrase(ns, passphrase) {
            Ok(()) => 0,
            Err(e) => {
                tracing::warn!(error = %e, "listen rule: could not apply the passphrase; rejecting the caller");
                reject(ns, FIRST_APPLICATION_REASON)
            }
        },
        ListenRule::Reject(r) => reject(ns, r.raw()),
    }
}

fn reject(ns: srt_sys::SRTSOCKET, code: i32) -> c_int {
    // SAFETY: plain libsrt call on the socket libsrt handed the hook.
    unsafe { srt_sys::srt_setrejectreason(ns, code) };
    -1
}

/// libsrt's listen callback, installed on every listener. Runs on libsrt's
/// receiver thread during the handshake.
extern "C" fn listen_hook(
    opaque: *mut c_void,
    ns: srt_sys::SRTSOCKET,
    _hs_version: c_int,
    _peer: *const libc::sockaddr,
    streamid: *const c_char,
) -> c_int {
    let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let lsn = opaque as usize as srt_sys::SRTSOCKET;
        // Clone out and drop the registry lock before the table lock.
        let inner = registry().get(&lsn).cloned();
        let Some(inner) = inner else {
            // The listener is closing: libsrt's default.
            return 0;
        };
        let stream_id = if streamid.is_null() {
            None
        } else {
            // SAFETY: libsrt passes a NUL-terminated string valid for the
            // duration of the call.
            Some(unsafe { CStr::from_ptr(streamid) }.to_bytes())
        };
        let rule = decide(&inner.read(), stream_id);
        apply(ns, &rule)
    }));
    decided.unwrap_or_else(|_| reject(ns, FIRST_APPLICATION_REASON))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_with_internal_reason_is_refused() {
        let rules = ListenRules::detached();
        assert!(matches!(
            rules.set("a", ListenRule::Reject(RejectReason::BadSecret)),
            Err(ListenRuleError::InternalReason(RejectReason::BadSecret))
        ));
        assert!(
            rules
                .set("a", ListenRule::Reject(RejectReason::Forbidden))
                .is_ok()
        );
        assert!(
            rules
                .set("b", ListenRule::Reject(RejectReason::Other(2042)))
                .is_ok()
        );
        assert!(matches!(
            rules.set_default(ListenRule::Reject(RejectReason::Timeout)),
            Err(ListenRuleError::InternalReason(_))
        ));
    }

    #[test]
    fn decide_uses_default_for_missing_invalid_utf8_and_v4() {
        let rules = ListenRules::detached();
        rules
            .set("publish:amber", ListenRule::Reject(RejectReason::Conflict))
            .unwrap();
        rules
            .set_default(ListenRule::Reject(RejectReason::Overload))
            .unwrap();
        let t = rules.inner.table.read().unwrap();
        assert!(matches!(
            decide(&t, Some(b"publish:amber")),
            ListenRule::Reject(RejectReason::Conflict)
        ));
        assert!(matches!(
            decide(&t, Some(b"publish:other")),
            ListenRule::Reject(RejectReason::Overload)
        ));
        assert!(matches!(
            decide(&t, Some(b"\xff\xfe")),
            ListenRule::Reject(RejectReason::Overload)
        ));
        assert!(matches!(
            decide(&t, Some(b"")),
            ListenRule::Reject(RejectReason::Overload)
        ));
        assert!(matches!(
            decide(&t, None),
            ListenRule::Reject(RejectReason::Overload)
        ));
        drop(t);
        rules
            .set("", ListenRule::Reject(RejectReason::Conflict))
            .unwrap();
        let t = rules.inner.table.read().unwrap();
        assert!(matches!(
            decide(&t, Some(b"")),
            ListenRule::Reject(RejectReason::Overload)
        ));
        assert!(matches!(
            decide(&t, None),
            ListenRule::Reject(RejectReason::Overload)
        ));
        let long = "x".repeat(512);
        drop(t);
        rules.set(&long, ListenRule::Accept).unwrap();
        let t = rules.inner.table.read().unwrap();
        assert!(matches!(
            decide(&t, Some(long.as_bytes())),
            ListenRule::Accept
        ));
    }

    #[test]
    fn closed_table_refuses_edits_and_reports_them() {
        let rules = ListenRules::detached();
        rules.inner.closed.store(true, Ordering::Release);
        assert!(matches!(
            rules.set("a", ListenRule::Accept),
            Err(ListenRuleError::Closed)
        ));
        assert!(matches!(
            rules.set_default(ListenRule::Accept),
            Err(ListenRuleError::Closed)
        ));
        assert!(!rules.remove("a"));
    }

    #[test]
    fn clear_keeps_the_default() {
        let rules = ListenRules::detached();
        rules.set("a", ListenRule::Accept).unwrap();
        rules
            .set_default(ListenRule::Reject(RejectReason::NotFound))
            .unwrap();
        rules.clear();
        assert_eq!(rules.len(), 0);
        let t = rules.inner.table.read().unwrap();
        assert!(matches!(
            decide(&t, Some(b"a")),
            ListenRule::Reject(RejectReason::NotFound)
        ));
    }

    #[test]
    fn debug_never_prints_a_passphrase() {
        let rules = ListenRules::detached();
        let pw = Passphrase::new("secretsecretsecret").unwrap();
        rules
            .set(
                "a",
                ListenRule::AcceptWith {
                    passphrase: pw.clone(),
                },
            )
            .unwrap();
        let rule = ListenRule::AcceptWith { passphrase: pw };
        for s in [format!("{rules:?}"), format!("{rule:?}")] {
            assert!(!s.contains("secretsecretsecret"), "{s}");
        }
    }

    #[test]
    fn apply_on_an_invalid_socket_rejects() {
        crate::init::ensure_initialized();
        // SRT_INVALID_SOCK: the option set fails → -1 (the Fallback path), never a panic.
        let pw = Passphrase::new("secretsecretsecret").unwrap();
        assert_eq!(
            apply(
                srt_sys::SRT_INVALID_SOCK,
                &ListenRule::AcceptWith { passphrase: pw }
            ),
            -1
        );
        assert_eq!(apply(srt_sys::SRT_INVALID_SOCK, &ListenRule::Accept), 0);
    }
}
