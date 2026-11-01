//! Per-mount-path publish and read policies for an
//! [`RtspServer`](crate::RtspServer), editable while the server runs.
//!
//! Every server owns one [`MountRules`] table, reached through
//! [`RtspServer::mount_rules`](crate::RtspServer::mount_rules). A path with
//! an entry uses it; every other path uses the table's default rule. A table
//! nobody edits holds `Inherit` for both directions, which leaves the
//! server-wide credential from the builder in charge, as before. A path
//! rule's `Inherit` means that same server-wide setting, never the default
//! rule.
//!
//! **Stability: Experimental.**

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use secrecy::SecretString;

use crate::rtsp::server::auth::Credential;
use crate::rtsp::server::builder::AuthScheme;
use crate::rtsp::server::handlers::extract_mount_path;
use crate::rtsp::server::validate_mount_path;

/// One username and password a client may present.
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
    /// `404 Not Found`.
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
    /// configured, else open.
    ///
    /// A path rule's `Inherit` does not fall back to the table's default
    /// rule. Under a default of `Refuse`, a path rule with one direction left
    /// `Inherit` leaves that direction as open as the server itself is.
    Inherit,
    /// Allow without credentials.
    Open,
    /// Allow a client that presents one of these credentials.
    Require(Vec<MountCredential>),
    /// Refuse with this status.
    Refuse(RefuseStatus),
}

/// The publish and read policies for one mount path (or the default).
#[derive(Clone, Debug)]
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
    /// The server was stopped.
    #[error("the server was stopped")]
    Closed,
}

/// Which side of a mount a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Publish,
    Read,
}

struct Inner {
    rules: RwLock<HashMap<String, MountRule>>,
    default: RwLock<MountRule>,
    challenge: RwLock<(AuthScheme, String)>,
    closed: AtomicBool,
}

/// Handle to one server's per-path rules, from
/// [`RtspServer::mount_rules`](crate::RtspServer::mount_rules).
///
/// A path is keyed the way the server resolves a request URI to a mount: a
/// trailing `/`, and a trailing per-media control segment (`trackID=`,
/// `streamid=` or `stream=`), are dropped. A rule set at `/amber/` therefore
/// governs `/amber`, and is the same entry as one set at `/amber`.
///
/// A clone is the same table. Usable from any thread, including while the
/// server runs; an edit applies from the next request on. After the server
/// stops, edits return [`MountRuleError::Closed`] while `len`, `is_empty`
/// and `is_closed` keep answering.
#[derive(Clone)]
pub struct MountRules {
    inner: Arc<Inner>,
}

impl fmt::Debug for MountRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountRules")
            .field("rules", &*read(&self.inner.rules))
            .field("default", &*read(&self.inner.default))
            .field("challenge", &*read(&self.inner.challenge))
            .field("closed", &self.is_closed())
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

/// Refuse a `Require` that no client could satisfy.
fn check_policy(policy: &MountPolicy) -> Result<(), MountRuleError> {
    if let MountPolicy::Require(creds) = policy
        && (creds.is_empty() || creds.iter().any(|c| c.username.is_empty()))
    {
        return Err(MountRuleError::EmptyCredentials);
    }
    Ok(())
}

pub(crate) fn check_rule(rule: &MountRule) -> Result<(), MountRuleError> {
    check_policy(&rule.publish)?;
    check_policy(&rule.read)
}

impl MountRules {
    pub(crate) fn new(default: MountRule, challenge: (AuthScheme, String)) -> Self {
        Self {
            inner: Arc::new(Inner {
                rules: RwLock::new(HashMap::new()),
                default: RwLock::new(default),
                challenge: RwLock::new(challenge),
                closed: AtomicBool::new(false),
            }),
        }
    }

    fn ensure_open(&self) -> Result<(), MountRuleError> {
        if self.is_closed() {
            Err(MountRuleError::Closed)
        } else {
            Ok(())
        }
    }

    /// Set the rule for `path`, replacing any earlier one. `path` is keyed
    /// as the type-level docs describe (`/amber/` names `/amber`).
    ///
    /// # Errors
    /// - [`MountRuleError::InvalidMountPath`] if `path` is not a valid mount
    ///   path.
    /// - [`MountRuleError::EmptyCredentials`] if a `Require` policy holds no
    ///   credential or a credential with an empty username.
    /// - [`MountRuleError::Closed`] after the server stopped.
    pub fn set(&self, path: &str, rule: MountRule) -> Result<(), MountRuleError> {
        self.ensure_open()?;
        validate_mount_path(path).map_err(|e| MountRuleError::InvalidMountPath {
            path: path.to_string(),
            detail: match e {
                crate::error::RtspServerError::InvalidMountPath { detail } => detail,
                other => other.to_string(),
            },
        })?;
        check_rule(&rule)?;
        write(&self.inner.rules).insert(extract_mount_path(path), rule);
        Ok(())
    }

    /// Remove the rule for `path`; the path then follows the default.
    /// Returns whether a rule was present. `path` is keyed as in
    /// [`Self::set`].
    ///
    /// # Errors
    /// - [`MountRuleError::Closed`] after the server stopped.
    pub fn remove(&self, path: &str) -> Result<bool, MountRuleError> {
        self.ensure_open()?;
        Ok(write(&self.inner.rules)
            .remove(&extract_mount_path(path))
            .is_some())
    }

    /// Set the rule every path without its own entry follows.
    ///
    /// # Errors
    /// - [`MountRuleError::EmptyCredentials`] if a `Require` policy holds no
    ///   credential or a credential with an empty username.
    /// - [`MountRuleError::Closed`] after the server stopped.
    pub fn set_default(&self, rule: MountRule) -> Result<(), MountRuleError> {
        self.ensure_open()?;
        check_rule(&rule)?;
        *write(&self.inner.default) = rule;
        Ok(())
    }

    /// Remove every path rule. The default rule and the challenge stay.
    ///
    /// # Errors
    /// - [`MountRuleError::Closed`] after the server stopped.
    pub fn clear(&self) -> Result<(), MountRuleError> {
        self.ensure_open()?;
        write(&self.inner.rules).clear();
        Ok(())
    }

    /// Set the scheme and realm a [`MountPolicy::Require`] challenge offers.
    /// Without a builder credential the challenge is Digest (MD5) with realm
    /// `tstrans`; with one it is the builder's scheme and realm.
    ///
    /// # Errors
    /// - [`MountRuleError::Closed`] after the server stopped.
    pub fn set_challenge(&self, scheme: AuthScheme, realm: &str) -> Result<(), MountRuleError> {
        self.ensure_open()?;
        *write(&self.inner.challenge) = (scheme, realm.to_string());
        Ok(())
    }

    /// The number of path rules (the default rule is not counted).
    pub fn len(&self) -> usize {
        read(&self.inner.rules).len()
    }

    /// Whether no path has a rule.
    pub fn is_empty(&self) -> bool {
        read(&self.inner.rules).is_empty()
    }

    /// Whether the server has stopped, after which edits are refused.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// The policy for `dir` on `path`: the path's own rule, else the default.
    pub(crate) fn policy_for(&self, path: &str, dir: Direction) -> MountPolicy {
        let pick = |rule: &MountRule| match dir {
            Direction::Publish => rule.publish.clone(),
            Direction::Read => rule.read.clone(),
        };
        match read(&self.inner.rules).get(path) {
            Some(rule) => pick(rule),
            None => pick(&read(&self.inner.default)),
        }
    }

    /// The scheme and realm a `Require` challenge offers when the builder
    /// configured no credential.
    pub(crate) fn challenge(&self) -> (AuthScheme, String) {
        read(&self.inner.challenge).clone()
    }

    /// An unedited table with the builder's defaults, for tests that build
    /// a `ServerState` by hand.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".to_string()),
        )
    }

    pub(crate) fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RtspServerBuilder;

    fn pw(s: &str) -> SecretString {
        SecretString::new(s.into())
    }

    fn table() -> MountRules {
        MountRules::new(
            MountRule::default(),
            (AuthScheme::DigestMd5, "tstrans".into()),
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
    fn a_trailing_slash_keys_the_rule_on_the_path_requests_resolve_to() {
        let t = table();
        t.set(
            "/amber/",
            MountRule::both(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
        assert!(matches!(
            t.policy_for("/amber", Direction::Publish),
            MountPolicy::Refuse(RefuseStatus::Forbidden)
        ));
        t.set("/amber", MountRule::both(MountPolicy::Open)).unwrap();
        assert_eq!(t.len(), 1, "/amber and /amber/ are one entry");
        assert!(matches!(
            t.policy_for("/amber", Direction::Read),
            MountPolicy::Open
        ));
        assert!(t.remove("/amber/").unwrap());
        assert!(t.is_empty());
        assert!(matches!(
            t.policy_for("/amber", Direction::Publish),
            MountPolicy::Inherit
        ));
    }

    #[test]
    fn invalid_paths_match_add_mount_exactly() {
        let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
            .unwrap()
            .build()
            .unwrap();
        let t = table();
        for path in ["", "amber", "/amber/", "/a b", "/../x", "/ok"] {
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
        t.set_challenge(AuthScheme::Basic, "lab").unwrap();
        assert_eq!(t.challenge(), (AuthScheme::Basic, "lab".to_string()));
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
                Err(RtspServerError::InvalidConfig { .. })
            ));
        }
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.default_mount_rule(MountRule::both(MountPolicy::Open));
        assert!(b.build().is_ok());
    }
}
