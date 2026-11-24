//! Per-mount-path publish and read policies for an
//! [`RtspServer`](crate::RtspServer), editable while the server runs.
//!
//! Every server owns one [`MountRules`] table, reached through
//! [`RtspServer::mount_rules`](crate::RtspServer::mount_rules). A path with
//! an entry uses it; every other path uses the table's default rule. A table
//! nobody edits holds `Inherit` for both directions, which leaves the
//! server-wide credential from the builder in charge, as before. A path
//! rule's `Inherit` takes the table's default rule for that direction; the
//! default rule's `Inherit` is the server-wide setting.
//!
//! The table's challenge ([`MountRules::set_challenge`]) is the scheme and
//! realm of every 401 the server sends, the builder credential's included.
//!
//! **Stability: Experimental.**

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use secrecy::SecretString;

use crate::rtsp::server::RtspServerCancelHandle;
use crate::rtsp::server::auth::Credential;
use crate::rtsp::server::builder::AuthScheme;
use crate::rtsp::server::validate_mount_path;

/// One username and password a client may present.
///
/// A user name that contains `:` cannot authenticate while the challenge is
/// Basic.
///
/// The password never leaves the table: there is no getter, and `Debug`
/// prints `<redacted>`.
#[derive(Clone)]
pub struct MountCredential {
    username: String,
    password: SecretString,
}

impl MountCredential {
    /// A credential for `username` with `password`.
    pub fn new(username: &str, password: SecretString) -> Self {
        Self {
            username: username.to_string(),
            password,
        }
    }

    /// The username.
    pub fn username(&self) -> &str {
        &self.username
    }
}

impl<'a> From<&'a MountCredential> for Credential<'a> {
    fn from(c: &'a MountCredential) -> Self {
        Credential {
            username: &c.username,
            password: &c.password,
        }
    }
}

impl fmt::Debug for MountCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountCredential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The status line a [`MountPolicy::Refuse`] answers with.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefuseStatus {
    /// `403 Forbidden`.
    Forbidden,
    /// `404 Not Found`. Not every request of such a path reads like one of
    /// a missing path: see [`MountPolicy::Refuse`].
    NotFound,
    /// `453 Not Enough Bandwidth`.
    NotEnoughBandwidth,
    /// `503 Service Unavailable`.
    ServiceUnavailable,
}

impl RefuseStatus {
    /// The numeric RTSP status code.
    pub fn code(self) -> u16 {
        match self {
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::NotEnoughBandwidth => 453,
            Self::ServiceUnavailable => 503,
        }
    }

    /// The status for a numeric RTSP code; `None` unless it is 403, 404, 453
    /// or 503. Inverse of [`RefuseStatus::code`].
    pub fn from_code(code: u16) -> Option<Self> {
        match code {
            403 => Some(Self::Forbidden),
            404 => Some(Self::NotFound),
            453 => Some(Self::NotEnoughBandwidth),
            503 => Some(Self::ServiceUnavailable),
            _ => None,
        }
    }

    /// The reason phrase for the status line.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Forbidden => "Forbidden",
            Self::NotFound => "Not Found",
            Self::NotEnoughBandwidth => "Not Enough Bandwidth",
            Self::ServiceUnavailable => "Service Unavailable",
        }
    }
}

/// What the server does with one direction of traffic on a mount path.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum MountPolicy {
    /// The server-wide setting: the builder's credential if one was
    /// configured, checked under the table's challenge
    /// ([`MountRules::set_challenge`]), else open. No rule edit removes the
    /// builder's credential.
    ///
    /// A path rule's `Inherit` takes the table's default rule for that
    /// direction; the default rule's `Inherit` is the server-wide setting.
    Inherit,
    /// Allow without credentials.
    Open,
    /// Allow a client that presents one of these credentials.
    Require(Vec<MountCredential>),
    /// Refuse with this status.
    ///
    /// The rule answers before the request's handler runs, so a refused
    /// path does not always read like a missing one. On a server whose
    /// default rule is open, a DESCRIBE or SETUP of a `Refuse(NotFound)`
    /// path reads like one of a missing path, and so does an ANNOUNCE with
    /// a usable SDP body on a server that creates no mount on demand. A
    /// request the handler would have answered otherwise shows that the
    /// path has a rule: an ANNOUNCE without a body reads `404` where a
    /// missing path reads `400` (`455` from a reader), and an `Idle`
    /// connection's PLAY, PAUSE, RECORD or TEARDOWN reads the rule's status
    /// where a missing path reads `454`, `454`, `455` or `200`. On a server
    /// whose default rule asks for a credential, a
    /// request without one reads the rule's status where every other path
    /// reads `401`.
    Refuse(RefuseStatus),
}

/// The publish and read policies for one mount path (or the default).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MountRule {
    /// Policy for a client that publishes (`ANNOUNCE`/`RECORD`) to the path.
    pub publish: MountPolicy,
    /// Policy for a client that reads (`DESCRIBE`/`SETUP`/`PLAY`) the path.
    pub read: MountPolicy,
}

impl MountRule {
    /// A rule with separate publish and read policies.
    pub fn new(publish: MountPolicy, read: MountPolicy) -> Self {
        Self { publish, read }
    }

    /// A rule that applies `policy` to both directions.
    pub fn both(policy: MountPolicy) -> Self {
        Self {
            publish: policy.clone(),
            read: policy,
        }
    }
}

impl Default for MountRule {
    /// `Inherit` for both directions.
    fn default() -> Self {
        Self::both(MountPolicy::Inherit)
    }
}

/// Why a [`MountRules`] edit was refused.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum MountRuleError {
    /// The path is not a valid mount path (the rule `add_mount` applies).
    #[error("invalid mount path {path:?}: {detail}")]
    InvalidMountPath {
        /// The path as given.
        path: String,
        /// Why it was refused.
        detail: String,
    },
    /// A [`MountPolicy::Require`] with no credential, or one with an empty
    /// username.
    #[error("a Require policy needs at least one credential with a non-empty username")]
    EmptyCredentials,
    /// A [`MountPolicy::Require`] that lists the same username twice. Both
    /// authentication schemes check a username's first entry only, so the
    /// later password could never be accepted.
    #[error("a Require policy lists username {username:?} more than once")]
    DuplicateUsername {
        /// The repeated username.
        username: String,
    },
    /// A challenge realm containing a control byte (CR, LF, NUL, DEL, ...),
    /// which would corrupt the `WWW-Authenticate` header.
    #[error("invalid challenge realm {realm:?}: control bytes are not allowed")]
    InvalidRealm {
        /// The realm as given.
        realm: String,
    },
    /// The server shut down: stopped, hard-cancelled or dropped.
    #[error("the server was shut down")]
    Closed,
}

/// How many requests one rule refused for the credential they carried, per
/// direction, from [`MountRules::auth_failures`] and
/// [`MountRules::default_auth_failures`].
///
/// A request counts when it carried an `Authorization` header and was
/// answered 401 for any reason but a stale nonce: a wrong password, an
/// unknown user, a malformed header, the wrong scheme. The first challenge
/// every client gets (no header yet), a stale nonce (a Digest header that
/// does not carry the connection's current nonce, malformed or not) and a
/// `Refuse` rule's status never count. An honest client's first answer after
/// [`MountRules::set_challenge`] changed the scheme or the realm does.
///
/// A stale nonce is answered 401 with a new nonce and `stale=true`, which
/// tells the client to answer again with the credential it has (RFC 7616
/// §3.3). That 401 is not one of the three that close a connection either,
/// and it clears none of them.
///
/// A rate signal: an unauthenticated peer can raise it at will (three
/// refused answers per connection under Basic or Digest: a Digest peer can
/// take its nonce from a stale challenge, which is no strike; any number of
/// connections), and a peer that holds
/// one valid credential resets its strikes with it and can try other
/// credentials without bound on one connection. A lock-out built on one
/// name's count can therefore be triggered against that name's owner.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MountRuleAuthFailures {
    /// Refused requests that publish (`ANNOUNCE`, and the requests of a
    /// session that announced).
    pub publish: u64,
    /// Refused requests that read (`DESCRIBE`, and the requests of a reader
    /// or an idle session).
    pub read: u64,
}

/// The refused-credential counts of one rule (or of the default rule).
#[derive(Debug, Default)]
struct Counts {
    publish: AtomicU64,
    read: AtomicU64,
}

impl Counts {
    fn snapshot(&self) -> MountRuleAuthFailures {
        MountRuleAuthFailures {
            publish: self.publish.load(Ordering::Relaxed),
            read: self.read.load(Ordering::Relaxed),
        }
    }
}

/// Which side of a mount a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Publish,
    Read,
}

/// Everything a request reads and an edit writes, under one lock: a request
/// sees the table before an edit or after it, never a mix, and the closed
/// flag is checked under the same guard the edit then writes through.
#[derive(Clone)]
struct Table {
    rules: HashMap<String, MountRule>,
    /// The refused-credential counts of each path in `rules`, same keys.
    counts: HashMap<String, Arc<Counts>>,
    default: MountRule,
    default_counts: Arc<Counts>,
    challenge: (AuthScheme, String),
    closed: bool,
}

struct Inner {
    table: RwLock<Table>,
    /// The owning server's hard cancel: the table is closed once it fires,
    /// even on a server whose listener never ran to close it. Kept outside
    /// the lock so that firing it stays one atomic store.
    hard_cancel: RtspServerCancelHandle,
    #[cfg(test)]
    pauses: PausePoints,
}

/// The policy and the challenge a request is decided by, read together.
#[derive(Debug)]
pub(crate) struct Decision {
    pub policy: MountPolicy,
    pub scheme: AuthScheme,
    pub realm: String,
    /// The counts of the rule the decision came from: the path's own when
    /// it has one (whatever its policy, `Inherit` included), else the
    /// default rule's.
    counts: Arc<Counts>,
}

impl Decision {
    /// Count one request refused for the credential it carried, in `dir`.
    /// No lock: the counts are atomics shared with the table.
    pub(crate) fn count_refused_credential(&self, dir: Direction) {
        match dir {
            Direction::Publish => &self.counts.publish,
            Direction::Read => &self.counts.read,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

/// Test-only stops inside a table's read and edit paths. They belong to one
/// table, so tests that run as threads of one process never meet each
/// other's, and the thread that reaches one need not be the one that armed it.
#[cfg(test)]
#[derive(Default)]
struct PausePoints {
    /// In `decision`, between the pick of the path rule and of the default.
    decision: std::sync::Mutex<Option<Pause>>,
    /// In every edit, right after its closed check.
    edit: std::sync::Mutex<Option<Pause>>,
    /// In the request path, between a failed credential check and the
    /// building of its 401.
    challenge: std::sync::Mutex<Option<Pause>>,
}

/// One armed stop: the thread that reaches it says so on `reached`, then
/// waits on `go_on`. It fires once.
#[cfg(test)]
struct Pause {
    reached: std::sync::mpsc::Sender<()>,
    go_on: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl Pause {
    /// Arm `slot`; the test keeps the returned ends.
    fn arm(
        slot: &std::sync::Mutex<Option<Pause>>,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_rx) = std::sync::mpsc::channel();
        let (go_on_tx, go_on) = std::sync::mpsc::channel();
        *slot.lock().unwrap() = Some(Pause { reached, go_on });
        (reached_rx, go_on_tx)
    }

    fn hit(slot: &std::sync::Mutex<Option<Pause>>) {
        let armed = slot.lock().unwrap().take();
        if let Some(p) = armed {
            let _ = p.reached.send(());
            let _ = p.go_on.recv();
        }
    }
}

/// Handle to one server's per-path rules, from
/// [`RtspServer::mount_rules`](crate::RtspServer::mount_rules).
///
/// A rule's `path` must be canonical: the exact key a request resolves to
/// (no trailing slash, no empty segment, no trailing `trackID=`/`streamid=`/
/// `stream=` segment), the same rule `add_mount` applies. `set("/amber/")`
/// is refused with [`MountRuleError::InvalidMountPath`]; use `/amber`. A
/// request for `/amber/` or `/amber/trackID=0` resolves to `/amber` and so
/// meets that rule.
///
/// A clone is the same table. Usable from any thread, including while the
/// server runs; an edit applies from the next request on. A session already
/// admitted meets an edit only at its next gated request (DESCRIBE,
/// ANNOUNCE, SETUP, PLAY, PAUSE, RECORD, TEARDOWN; the OPTIONS and
/// GET_PARAMETER keepalives are never checked): a reader that is playing
/// and only keeps alive streams on after its mount's rule turns to
/// `Require` or `Refuse`, as a recording publisher does. To end such a
/// session, remove its mount with
/// [`RtspServer::remove_mount`](crate::RtspServer::remove_mount). After the server
/// shuts down (stopped, hard-cancelled or dropped), edits return
/// [`MountRuleError::Closed`] and the table no longer changes, while `len`,
/// `is_empty` and `is_closed` keep answering. An edit that began before a
/// hard cancel may complete; none begins after it.
#[derive(Clone)]
pub struct MountRules {
    inner: Arc<Inner>,
}

impl fmt::Debug for MountRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Copy out first: the caller's formatter may be slow, and an edit
        // must not wait for it.
        let t = read(&self.inner.table).clone();
        let closed = t.closed || self.inner.hard_cancel.is_cancelled();
        f.debug_struct("MountRules")
            .field("rules", &t.rules)
            .field("default", &t.default)
            .field("challenge", &t.challenge)
            .field("closed", &closed)
            .finish()
    }
}

// A panic while a guard is held cannot leave the table half-edited (every
// edit is one insert/remove/assignment), so a poisoned lock is still usable.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// Refuse a `Require` that no client could satisfy, or one with a
/// credential that could never be accepted (a repeated username).
fn check_policy(policy: &MountPolicy) -> Result<(), MountRuleError> {
    let MountPolicy::Require(creds) = policy else {
        return Ok(());
    };
    if creds.is_empty() || creds.iter().any(|c| c.username.is_empty()) {
        return Err(MountRuleError::EmptyCredentials);
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = creds.iter().find(|c| !seen.insert(c.username.as_str())) {
        return Err(MountRuleError::DuplicateUsername {
            username: dup.username.clone(),
        });
    }
    Ok(())
}

/// The mount-path gate `add_mount` applies, mapped to the rules table's
/// error.
fn check_path(path: &str) -> Result<(), MountRuleError> {
    validate_mount_path(path).map_err(|e| MountRuleError::InvalidMountPath {
        path: path.to_string(),
        detail: match e {
            crate::error::RtspServerError::InvalidMountPath { detail } => detail,
            other => other.to_string(),
        },
    })
}

pub(crate) fn check_rule(rule: &MountRule) -> Result<(), MountRuleError> {
    check_policy(&rule.publish)?;
    check_policy(&rule.read)
}

impl MountRules {
    pub(crate) fn new(
        default: MountRule,
        challenge: (AuthScheme, String),
        hard_cancel: RtspServerCancelHandle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                table: RwLock::new(Table {
                    rules: HashMap::new(),
                    counts: HashMap::new(),
                    default,
                    default_counts: Arc::default(),
                    challenge,
                    closed: false,
                }),
                hard_cancel,
                #[cfg(test)]
                pauses: PausePoints::default(),
            }),
        }
    }

    /// Run `apply` under the write guard, unless the table is closed. The
    /// check is made under the same guard, so no edit lands after `close`
    /// returned.
    fn edit<R>(
        &self,
        apply: impl FnOnce(&mut Table) -> Result<R, MountRuleError>,
    ) -> Result<R, MountRuleError> {
        let mut t = write(&self.inner.table);
        if t.closed || self.inner.hard_cancel.is_cancelled() {
            return Err(MountRuleError::Closed);
        }
        #[cfg(test)]
        Pause::hit(&self.inner.pauses.edit);
        apply(&mut t)
    }

    /// Set the rule for `path`, replacing any earlier one. A path that
    /// already had a rule keeps its refused-credential counts.
    ///
    /// # Errors
    /// - [`MountRuleError::InvalidMountPath`] if `path` is not a valid mount
    ///   path. `path` must be canonical: the exact key a request resolves to
    ///   (no trailing slash, no empty segment, no trailing `trackID=`/
    ///   `streamid=`/`stream=` segment). `set("/amber/")` is refused with
    ///   `InvalidMountPath`; use `/amber`.
    /// - [`MountRuleError::EmptyCredentials`] if a `Require` policy holds no
    ///   credential or a credential with an empty username.
    /// - [`MountRuleError::DuplicateUsername`] if a `Require` policy lists a
    ///   username more than once.
    /// - [`MountRuleError::Closed`] after the server shut down.
    pub fn set(&self, path: &str, rule: MountRule) -> Result<(), MountRuleError> {
        self.edit(|t| {
            check_path(path)?;
            check_rule(&rule)?;
            t.rules.insert(path.to_string(), rule);
            t.counts.entry(path.to_string()).or_default();
            Ok(())
        })
    }

    /// Remove the rule for `path`; the path then follows the default. Its
    /// refused-credential counts go with it. Returns whether a rule was
    /// present.
    ///
    /// # Errors
    /// - [`MountRuleError::InvalidMountPath`] if `path` is not a valid mount
    ///   path. `path` must be canonical, as in [`Self::set`]:
    ///   `remove("/amber/")` is refused with `InvalidMountPath`; use `/amber`.
    /// - [`MountRuleError::Closed`] after the server shut down.
    pub fn remove(&self, path: &str) -> Result<bool, MountRuleError> {
        self.edit(|t| {
            check_path(path)?;
            t.counts.remove(path);
            Ok(t.rules.remove(path).is_some())
        })
    }

    /// Set the rule every path without its own entry follows.
    ///
    /// # Errors
    /// - [`MountRuleError::EmptyCredentials`] if a `Require` policy holds no
    ///   credential or a credential with an empty username.
    /// - [`MountRuleError::DuplicateUsername`] if a `Require` policy lists a
    ///   username more than once.
    /// - [`MountRuleError::Closed`] after the server shut down.
    pub fn set_default(&self, rule: MountRule) -> Result<(), MountRuleError> {
        self.edit(|t| {
            check_rule(&rule)?;
            t.default = rule;
            Ok(())
        })
    }

    /// Remove every path rule, and their refused-credential counts. The
    /// default rule, its counts and the challenge stay.
    ///
    /// # Errors
    /// - [`MountRuleError::Closed`] after the server shut down.
    pub fn clear(&self) -> Result<(), MountRuleError> {
        self.edit(|t| {
            t.rules.clear();
            t.counts.clear();
            Ok(())
        })
    }

    /// Set the scheme and realm of every 401 the server sends, and so the
    /// scheme a credential is checked in: a [`MountPolicy::Require`] path's
    /// and, on a server built with `auth_*`, the server's own credential's.
    /// The table starts with the builder credential's scheme and realm, or
    /// Digest (MD5) with realm `tstrans` without one. A change applies from
    /// the next request on; a Digest answer computed for the old realm is
    /// then refused.
    ///
    /// A user name that contains `:` cannot authenticate while the challenge
    /// is Basic, the server's own credential included.
    ///
    /// # Errors
    /// - [`MountRuleError::InvalidRealm`] if `realm` holds a control byte.
    /// - [`MountRuleError::Closed`] after the server shut down.
    pub fn set_challenge(&self, scheme: AuthScheme, realm: &str) -> Result<(), MountRuleError> {
        self.edit(|t| {
            if !crate::rtsp::server::auth::realm_is_valid(realm) {
                return Err(MountRuleError::InvalidRealm {
                    realm: realm.to_string(),
                });
            }
            t.challenge = (scheme, realm.to_string());
            Ok(())
        })
    }

    /// The number of path rules (the default rule is not counted).
    pub fn len(&self) -> usize {
        read(&self.inner.table).rules.len()
    }

    /// Whether no path has a rule.
    pub fn is_empty(&self) -> bool {
        read(&self.inner.table).rules.is_empty()
    }

    /// The requests refused for their credential on `path`'s own rule
    /// (see [`MountRuleAuthFailures`]); `None` when `path` has no rule of
    /// its own. A request for a path without a rule counts on the default
    /// rule. Answers after the server shut down.
    pub fn auth_failures(&self, path: &str) -> Option<MountRuleAuthFailures> {
        read(&self.inner.table)
            .counts
            .get(path)
            .map(|c| c.snapshot())
    }

    /// The requests refused for their credential on the default rule: those
    /// for every path without a rule of its own (see
    /// [`MountRuleAuthFailures`]). Answers after the server shut down.
    pub fn default_auth_failures(&self) -> MountRuleAuthFailures {
        read(&self.inner.table).default_counts.snapshot()
    }

    /// Whether the server has shut down, after which edits are refused.
    pub fn is_closed(&self) -> bool {
        self.inner.hard_cancel.is_cancelled() || read(&self.inner.table).closed
    }

    /// The policy for `dir` on `path` (the path's own rule, else, with no
    /// rule or the rule's `Inherit` for `dir`, the default rule's policy for
    /// `dir`) and the challenge's scheme and realm.
    pub(crate) fn decision(&self, path: &str, dir: Direction) -> Decision {
        let pick = |rule: &MountRule| match dir {
            Direction::Publish => rule.publish.clone(),
            Direction::Read => rule.read.clone(),
        };
        let t = read(&self.inner.table);
        let own = t.rules.get(path).map(pick);
        #[cfg(test)]
        Pause::hit(&self.inner.pauses.decision);
        let policy = match own {
            Some(policy) if !matches!(policy, MountPolicy::Inherit) => policy,
            _ => pick(&t.default),
        };
        let (scheme, realm) = t.challenge.clone();
        let counts = t.counts.get(path).unwrap_or(&t.default_counts).clone();
        Decision {
            policy,
            scheme,
            realm,
            counts,
        }
    }

    /// Close the table. An edit that holds the guard finishes first; every
    /// later one reads `Closed`.
    pub(crate) fn close(&self) {
        write(&self.inner.table).closed = true;
    }

    /// Arm the stop a request reaches between a failed credential check and
    /// the building of its 401.
    #[cfg(test)]
    pub(crate) fn arm_challenge_pause(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        Pause::arm(&self.inner.pauses.challenge)
    }

    #[cfg(test)]
    pub(crate) fn pause_before_challenge(&self) {
        Pause::hit(&self.inner.pauses.challenge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RtspServerBuilder;

    impl MountRules {
        /// The policy a request for `dir` on `path` is decided by.
        fn policy_for(&self, path: &str, dir: Direction) -> MountPolicy {
            self.decision(path, dir).policy
        }

        /// The table's challenge, read without passing a pause point.
        fn challenge(&self) -> (AuthScheme, String) {
            read(&self.inner.table).challenge.clone()
        }
    }

    #[test]
    fn from_code_round_trips_every_refuse_status() {
        for s in [
            RefuseStatus::Forbidden,
            RefuseStatus::NotFound,
            RefuseStatus::NotEnoughBandwidth,
            RefuseStatus::ServiceUnavailable,
        ] {
            assert_eq!(RefuseStatus::from_code(s.code()), Some(s));
        }
        assert_eq!(RefuseStatus::from_code(500), None);
    }

    fn pw(s: &str) -> SecretString {
        SecretString::new(s.into())
    }

    fn table() -> MountRules {
        MountRules::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".into()),
            RtspServerCancelHandle::new(),
        )
    }

    #[test]
    fn default_rule_is_inherit_both_ways() {
        let r = MountRule::default();
        assert!(matches!(r.publish, MountPolicy::Inherit));
        assert!(matches!(r.read, MountPolicy::Inherit));
    }

    #[test]
    fn set_then_policy_for_returns_the_rule_by_direction() {
        let t = table();
        t.set(
            "/amber",
            MountRule::new(
                MountPolicy::Refuse(RefuseStatus::Forbidden),
                MountPolicy::Open,
            ),
        )
        .unwrap();
        assert!(matches!(
            t.policy_for("/amber", Direction::Publish),
            MountPolicy::Refuse(RefuseStatus::Forbidden)
        ));
        assert!(matches!(
            t.policy_for("/amber", Direction::Read),
            MountPolicy::Open
        ));
        assert!(matches!(
            t.policy_for("/other", Direction::Read),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn remove_falls_back_to_the_default_and_reports_presence() {
        let t = table();
        t.set("/amber", MountRule::both(MountPolicy::Open)).unwrap();
        assert!(t.remove("/amber").unwrap());
        assert!(!t.remove("/amber").unwrap());
        assert!(matches!(
            t.policy_for("/amber", Direction::Publish),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn a_path_rules_inherit_falls_back_to_the_table_default() {
        let t = table();
        t.set_default(MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)))
            .unwrap();
        t.set(
            "/amber",
            MountRule::new(MountPolicy::Open, MountPolicy::Inherit),
        )
        .unwrap();
        assert!(matches!(
            t.policy_for("/amber", Direction::Publish),
            MountPolicy::Open
        ));
        assert!(matches!(
            t.policy_for("/amber", Direction::Read),
            MountPolicy::Refuse(RefuseStatus::NotFound)
        ));
    }

    #[test]
    fn inherit_through_an_inherit_default_is_the_server_setting() {
        let t = table();
        t.set(
            "/amber",
            MountRule::new(MountPolicy::Open, MountPolicy::Inherit),
        )
        .unwrap();
        assert!(matches!(
            t.policy_for("/amber", Direction::Read),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn set_default_governs_unknown_paths_and_clear_keeps_it() {
        let t = table();
        t.set_default(MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)))
            .unwrap();
        t.set("/a", MountRule::both(MountPolicy::Open)).unwrap();
        t.clear().unwrap();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        assert!(matches!(
            t.policy_for("/zzz", Direction::Publish),
            MountPolicy::Refuse(RefuseStatus::NotFound)
        ));
    }

    #[test]
    fn require_with_no_credentials_or_an_empty_username_is_refused() {
        let t = table();
        let none = MountRule::both(MountPolicy::Require(vec![]));
        assert!(matches!(
            t.set("/a", none.clone()),
            Err(MountRuleError::EmptyCredentials)
        ));
        let blank = MountRule::both(MountPolicy::Require(vec![MountCredential::new(
            "",
            pw("x"),
        )]));
        assert!(matches!(
            t.set("/a", blank.clone()),
            Err(MountRuleError::EmptyCredentials)
        ));
        assert!(matches!(
            t.set_default(none),
            Err(MountRuleError::EmptyCredentials)
        ));
        assert!(matches!(
            t.set_default(blank),
            Err(MountRuleError::EmptyCredentials)
        ));
        assert!(t.is_empty());
        assert!(matches!(
            t.policy_for("/a", Direction::Read),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn set_and_remove_refuse_a_non_canonical_key() {
        let t = table();
        for path in ["/amber/", "/amber//", "/amber/stream=1", "/amber/trackID=0"] {
            assert!(
                matches!(
                    t.set(path, MountRule::both(MountPolicy::Open)),
                    Err(MountRuleError::InvalidMountPath { .. })
                ),
                "set {path:?}"
            );
            assert!(
                matches!(t.remove(path), Err(MountRuleError::InvalidMountPath { .. })),
                "remove {path:?}"
            );
        }
        assert!(t.is_empty());
        t.set("/amber", MountRule::both(MountPolicy::Open)).unwrap();
        assert!(matches!(
            t.policy_for("/amber", Direction::Read),
            MountPolicy::Open
        ));
        // A child-looking key is its own entry, never the parent's.
        assert!(matches!(
            t.policy_for("/amber/stream=1", Direction::Read),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn require_with_a_repeated_username_is_refused() {
        let t = table();
        let dup = MountRule::new(
            MountPolicy::Inherit,
            MountPolicy::Require(vec![
                MountCredential::new("amber", pw("a")),
                MountCredential::new("other", pw("x")),
                MountCredential::new("amber", pw("b")),
            ]),
        );
        let is_dup = |r: Result<(), MountRuleError>| matches!(r, Err(MountRuleError::DuplicateUsername { username }) if username == "amber");
        assert!(is_dup(t.set("/a", dup.clone())));
        assert!(is_dup(t.set_default(dup.clone())));
        assert!(t.is_empty());
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.default_mount_rule(dup);
        match b.build() {
            Err(crate::error::RtspServerError::InvalidServerConfig { detail }) => {
                assert!(detail.contains("amber"), "{detail}");
            }
            other => panic!("expected InvalidServerConfig, got {other:?}"),
        }
    }

    #[test]
    fn invalid_paths_match_add_mount_exactly() {
        let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
            .unwrap()
            .build()
            .unwrap();
        let t = table();
        for path in [
            "",
            "amber",
            "/amber/",
            "/a b",
            "/../x",
            "/ok",
            "/amber//",
            "/amber/stream=1",
            "/amber/trackID=0",
            "/",
        ] {
            let rule_err = t.set(path, MountRule::default()).is_err();
            let mount_err = server.add_publish_mount(path).is_err();
            assert_eq!(rule_err, mount_err, "path {path:?}");
        }
    }

    #[test]
    fn edits_after_close_return_closed_but_queries_answer() {
        let t = table();
        t.set("/a", MountRule::default()).unwrap();
        t.close();
        assert!(matches!(
            t.set("/b", MountRule::default()),
            Err(MountRuleError::Closed)
        ));
        assert!(matches!(t.remove("/a"), Err(MountRuleError::Closed)));
        assert!(matches!(
            t.set_default(MountRule::default()),
            Err(MountRuleError::Closed)
        ));
        assert!(matches!(t.clear(), Err(MountRuleError::Closed)));
        assert!(matches!(
            t.set_challenge(AuthScheme::Basic, "x"),
            Err(MountRuleError::Closed)
        ));
        assert_eq!(t.len(), 1);
        assert!(t.is_closed());
    }

    #[test]
    fn challenge_defaults_to_digest_md5_tstrans_and_set_challenge_replaces_it() {
        let t = table();
        assert_eq!(
            t.challenge(),
            (AuthScheme::DigestMd5, "tstrans".to_string())
        );
        assert!(matches!(
            t.set_challenge(AuthScheme::Basic, "a\r\nX: y"),
            Err(MountRuleError::InvalidRealm { .. })
        ));
        t.set_challenge(AuthScheme::Basic, "lab").unwrap();
        assert_eq!(t.challenge(), (AuthScheme::Basic, "lab".to_string()));
    }

    #[test]
    fn debug_shows_the_challenge_in_use() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.auth_basic("realm", "admin", pw("secret"));
        let t = crate::rtsp::server::initial_mount_rules(&b, RtspServerCancelHandle::new());
        t.set_challenge(AuthScheme::DigestSha256, "lab").unwrap();
        let d = t.decision("/a", Direction::Read);
        assert_eq!(
            (d.scheme, d.realm.as_str()),
            (AuthScheme::DigestSha256, "lab")
        );
        let s = format!("{t:?}");
        assert!(s.contains(r#"challenge: (DigestSha256, "lab")"#), "{s}");
    }

    fn counts(read: u64, publish: u64) -> Option<MountRuleAuthFailures> {
        Some(MountRuleAuthFailures { publish, read })
    }

    #[test]
    fn a_rules_counts_survive_set_and_are_dropped_by_remove_and_clear() {
        let t = table();
        t.set("/a", MountRule::both(MountPolicy::Open)).unwrap();
        let refuse = |n: usize| {
            for _ in 0..n {
                t.decision("/a", Direction::Read)
                    .count_refused_credential(Direction::Read);
            }
        };
        refuse(2);
        t.set(
            "/a",
            MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)),
        )
        .unwrap();
        assert_eq!(t.auth_failures("/a"), counts(2, 0), "set keeps the counts");
        assert!(t.remove("/a").unwrap());
        assert_eq!(t.auth_failures("/a"), None);
        t.set("/a", MountRule::both(MountPolicy::Open)).unwrap();
        assert_eq!(
            t.auth_failures("/a"),
            counts(0, 0),
            "a removed rule's counts are gone"
        );
        refuse(1);
        t.clear().unwrap();
        assert_eq!(t.auth_failures("/a"), None);
        assert_eq!(t.default_auth_failures(), MountRuleAuthFailures::default());
    }

    #[test]
    fn a_path_rule_that_says_inherit_still_counts_on_itself() {
        let t = table();
        t.set_default(MountRule::both(MountPolicy::Open)).unwrap();
        t.set("/a", MountRule::default()).unwrap();
        t.decision("/a", Direction::Publish)
            .count_refused_credential(Direction::Publish);
        t.decision("/other", Direction::Read)
            .count_refused_credential(Direction::Read);
        assert_eq!(t.auth_failures("/a"), counts(0, 1));
        assert_eq!(t.auth_failures("/other"), None);
        assert_eq!(
            t.default_auth_failures(),
            MountRuleAuthFailures {
                publish: 0,
                read: 1
            }
        );
    }

    #[test]
    fn counts_stay_readable_after_the_table_closed() {
        let t = table();
        t.set("/a", MountRule::default()).unwrap();
        t.decision("/a", Direction::Read)
            .count_refused_credential(Direction::Read);
        t.close();
        assert_eq!(t.auth_failures("/a"), counts(1, 0));
        assert_eq!(t.default_auth_failures(), MountRuleAuthFailures::default());
    }

    #[test]
    fn debug_redacts_passwords() {
        let rule = MountRule::both(MountPolicy::Require(vec![MountCredential::new(
            "amber",
            pw("hunter2"),
        )]));
        let s = format!("{rule:?}");
        assert!(s.contains("amber"));
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("hunter2"));
    }

    #[test]
    fn refuse_status_codes_and_reasons() {
        for (s, code, reason) in [
            (RefuseStatus::Forbidden, 403, "Forbidden"),
            (RefuseStatus::NotFound, 404, "Not Found"),
            (
                RefuseStatus::NotEnoughBandwidth,
                453,
                "Not Enough Bandwidth",
            ),
            (RefuseStatus::ServiceUnavailable, 503, "Service Unavailable"),
        ] {
            assert_eq!(s.code(), code);
            assert_eq!(s.reason(), reason);
        }
    }

    #[test]
    fn build_refuses_an_invalid_default_mount_rule() {
        use crate::error::RtspServerError;
        for rule in [
            MountRule::both(MountPolicy::Require(vec![])),
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Require(vec![MountCredential::new("", pw("x"))]),
            ),
        ] {
            let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            b.default_mount_rule(rule);
            assert!(matches!(
                b.build(),
                Err(RtspServerError::InvalidServerConfig { .. })
            ));
        }
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.default_mount_rule(MountRule::both(MountPolicy::Open));
        assert!(b.build().is_ok());
    }

    use std::sync::mpsc;
    use std::time::Duration;
    use tst_test_helpers::timing::scaled;

    /// How long a thread gets to reach a pause point or to finish once
    /// nothing holds it back.
    fn deadline() -> Duration {
        scaled(Duration::from_secs(10))
    }

    /// How long a call that must stay blocked is watched. On a correct
    /// table no wait can see it return, so this is an input of the test,
    /// not a race with the code under test.
    const STAYS_BLOCKED: Duration = Duration::from_millis(300);

    fn is_refuse(p: &MountPolicy) -> bool {
        matches!(p, MountPolicy::Refuse(_))
    }

    #[test]
    fn a_request_between_two_edits_sees_one_of_the_two_tables() {
        let t = table();
        t.set("/amber", MountRule::default()).unwrap();
        t.set_default(MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)))
            .unwrap();
        // Every state the operator's two edits pass through refuses /amber.
        let (reached, go_on) = Pause::arm(&t.inner.pauses.decision);
        let reader = {
            let t = t.clone();
            std::thread::spawn(move || t.decision("/amber", Direction::Read).policy)
        };
        reached
            .recv_timeout(deadline())
            .expect("the reader reached its pause point");
        let (first_done, first_done_rx) = mpsc::channel();
        let editor = {
            let t = t.clone();
            std::thread::spawn(move || {
                t.set(
                    "/amber",
                    MountRule::both(MountPolicy::Refuse(RefuseStatus::Forbidden)),
                )
                .unwrap();
                let _ = first_done.send(());
                t.set_default(MountRule::both(MountPolicy::Open)).unwrap();
            })
        };
        let first_returned_while_parked = first_done_rx.recv_timeout(STAYS_BLOCKED).is_ok();
        go_on.send(()).unwrap();
        let policy = reader.join().unwrap();
        editor.join().unwrap();
        assert!(
            !first_returned_while_parked,
            "an edit completed while a request was halfway through its read"
        );
        assert!(is_refuse(&policy), "the request was decided by {policy:?}");
    }

    #[test]
    fn the_table_does_not_change_after_its_close_returned() {
        let t = table();
        let (reached, go_on) = Pause::arm(&t.inner.pauses.edit);
        let edit = {
            let t = t.clone();
            std::thread::spawn(move || t.set("/b", MountRule::both(MountPolicy::Open)))
        };
        reached
            .recv_timeout(deadline())
            .expect("the edit reached its pause point");
        let (closed, closed_rx) = mpsc::channel();
        let closer = {
            let t = t.clone();
            std::thread::spawn(move || {
                t.close();
                let _ = closed.send(t.len());
            })
        };
        let close_returned_during_the_edit = closed_rx.recv_timeout(STAYS_BLOCKED).is_ok();
        go_on.send(()).unwrap();
        let edit_result = edit.join().unwrap();
        closer.join().unwrap();
        assert!(
            !close_returned_during_the_edit,
            "close() returned while an edit that passed its check was still to land"
        );
        assert!(edit_result.is_ok(), "{edit_result:?}");
        assert_eq!(t.len(), 1, "the edit that held the table landed");
        assert!(matches!(
            t.policy_for("/b", Direction::Read),
            MountPolicy::Open
        ));
        let len_at_close = t.len();
        assert!(matches!(
            t.set("/c", MountRule::default()),
            Err(MountRuleError::Closed)
        ));
        assert!(matches!(t.remove("/b"), Err(MountRuleError::Closed)));
        assert!(matches!(t.clear(), Err(MountRuleError::Closed)));
        assert_eq!(t.len(), len_at_close);
        assert!(matches!(
            t.policy_for("/b", Direction::Read),
            MountPolicy::Open
        ));
    }

    #[test]
    fn an_edit_begun_after_a_hard_cancel_is_refused() {
        let cancel = RtspServerCancelHandle::new();
        let t = MountRules::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".into()),
            cancel.clone(),
        );
        t.set("/a", MountRule::both(MountPolicy::Open)).unwrap();
        cancel.cancel();
        assert!(matches!(
            t.set("/b", MountRule::default()),
            Err(MountRuleError::Closed)
        ));
        assert!(matches!(t.remove("/a"), Err(MountRuleError::Closed)));
        assert!(matches!(
            t.set_default(MountRule::both(MountPolicy::Open)),
            Err(MountRuleError::Closed)
        ));
        assert!(matches!(t.clear(), Err(MountRuleError::Closed)));
        assert!(matches!(
            t.set_challenge(AuthScheme::Basic, "x"),
            Err(MountRuleError::Closed)
        ));
        assert_eq!(t.len(), 1);
        assert!(matches!(
            t.policy_for("/a", Direction::Read),
            MountPolicy::Open
        ));
        assert!(matches!(
            t.policy_for("/zzz", Direction::Read),
            MountPolicy::Inherit
        ));
        assert_eq!(
            t.challenge(),
            (AuthScheme::DigestMd5, "tstrans".to_string())
        );
    }

    #[test]
    fn an_edit_waiting_for_the_table_when_the_cancel_fires_is_refused() {
        let cancel = RtspServerCancelHandle::new();
        let t = MountRules::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".into()),
            cancel.clone(),
        );
        let (reached, go_on) = Pause::arm(&t.inner.pauses.edit);
        let first = {
            let t = t.clone();
            std::thread::spawn(move || t.set("/a", MountRule::both(MountPolicy::Open)))
        };
        reached
            .recv_timeout(deadline())
            .expect("the first edit reached its pause point");
        // The first edit holds the table; the second waits for it.
        let (second_done, second_done_rx) = mpsc::channel();
        let second = {
            let t = t.clone();
            std::thread::spawn(move || {
                let r = t.set("/b", MountRule::both(MountPolicy::Open));
                let _ = second_done.send(());
                r
            })
        };
        let second_returned_early = second_done_rx.recv_timeout(STAYS_BLOCKED).is_ok();
        cancel.cancel();
        assert!(t.is_closed());
        go_on.send(()).unwrap();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(
            !second_returned_early,
            "an edit passed a table another edit held"
        );
        assert!(first.is_ok(), "{first:?}");
        assert!(
            matches!(second, Err(MountRuleError::Closed)),
            "an edit that got the table after the cancel fired returned {second:?}"
        );
        assert_eq!(t.len(), 1);
        assert!(matches!(
            t.policy_for("/b", Direction::Read),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn is_closed_reads_true_at_once_after_a_hard_cancel() {
        let cancel = RtspServerCancelHandle::new();
        let t = MountRules::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".into()),
            cancel.clone(),
        );
        assert!(!t.is_closed());
        cancel.cancel();
        assert!(t.is_closed());
    }

    /// A `fmt::Write` that parks the first time it is handed `needle`.
    struct ParkingWriter {
        needle: &'static str,
        reached: mpsc::Sender<()>,
        go_on: mpsc::Receiver<()>,
        parked: bool,
    }

    impl fmt::Write for ParkingWriter {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if !self.parked && s == self.needle {
                self.parked = true;
                let _ = self.reached.send(());
                let _ = self.go_on.recv();
            }
            Ok(())
        }
    }

    #[test]
    fn debug_formats_without_holding_the_table() {
        let t = table();
        t.set("/a", MountRule::both(MountPolicy::Open)).unwrap();
        let (reached, reached_rx) = mpsc::channel();
        let (go_on, go_on_rx) = mpsc::channel();
        let formatter = {
            let t = t.clone();
            std::thread::spawn(move || {
                let mut w = ParkingWriter {
                    needle: "rules",
                    reached,
                    go_on: go_on_rx,
                    parked: false,
                };
                fmt::write(&mut w, format_args!("{t:?}")).unwrap();
                w.parked
            })
        };
        reached_rx
            .recv_timeout(deadline())
            .expect("the formatter parked at the field name");
        let (edited, edited_rx) = mpsc::channel();
        let editor = {
            let t = t.clone();
            std::thread::spawn(move || {
                let _ = edited.send(t.set("/b", MountRule::both(MountPolicy::Open)));
            })
        };
        let edit = edited_rx.recv_timeout(deadline());
        go_on.send(()).unwrap();
        assert!(formatter.join().unwrap(), "the formatter never parked");
        editor.join().unwrap();
        assert!(
            matches!(edit, Ok(Ok(()))),
            "an edit waited behind a slow formatter: {edit:?}"
        );
        assert_eq!(t.len(), 2);
    }
}
