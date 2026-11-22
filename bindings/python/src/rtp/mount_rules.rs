//! `MountCredential`, `MountPolicy`, `MountRule` and `MountRules`.
//!
//! `MountRules` holds the `tst_rtp::MountRules` handle of one server; it is a
//! clone of the server's own table, so an edit applies to the next request.
//! None of these methods blocks (a table write waits at most on a request's
//! read), so none releases the GIL. Passwords go in and never come out: there
//! is no getter and `repr` prints `<redacted>`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use secrecy::SecretString;

use tst_pipeline::binding::BindingError;
use tst_rtp::{
    AuthScheme, MountCredential, MountPolicy, MountRule, MountRuleAuthFailures, MountRuleError,
    MountRules, RefuseStatus,
};

use crate::raise::{RTSP, raise};

fn rule_err(py: Python<'_>, e: MountRuleError) -> PyErr {
    raise(py, &RTSP, BindingError::from(e))
}

/// One username and password a client may present. The password cannot be
/// read back and `repr` redacts it.
#[pyclass(name = "MountCredential", module = "tstrans.rtp", frozen)]
pub(crate) struct PyMountCredential {
    inner: MountCredential,
}

#[pymethods]
impl PyMountCredential {
    #[new]
    fn new(username: &str, password: String) -> Self {
        Self {
            inner: MountCredential::new(username, SecretString::new(password.into())),
        }
    }

    /// The username.
    #[getter]
    fn username(&self) -> String {
        self.inner.username().to_owned()
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.inner)
    }
}

/// What the server does with one direction of traffic on a mount path:
/// `inherit()`, `open()`, `require(*credentials)` or `refuse(status)`.
#[pyclass(name = "MountPolicy", module = "tstrans.rtp", frozen)]
pub(crate) struct PyMountPolicy {
    inner: MountPolicy,
}

#[pymethods]
impl PyMountPolicy {
    /// The server-wide setting: the config's `auth` credential if one was
    /// configured, else open. A path rule's `inherit()` takes the table's
    /// default rule for that direction; the default rule's `inherit()` is the
    /// server-wide setting.
    #[staticmethod]
    fn inherit() -> Self {
        Self {
            inner: MountPolicy::Inherit,
        }
    }

    /// Allow without credentials.
    #[staticmethod]
    fn open() -> Self {
        Self {
            inner: MountPolicy::Open,
        }
    }

    /// Allow a client that presents one of `credentials`. Raises
    /// `ValueError` when none is given.
    #[staticmethod]
    #[pyo3(signature = (*credentials))]
    fn require(credentials: Vec<PyRef<'_, PyMountCredential>>) -> PyResult<Self> {
        if credentials.is_empty() {
            return Err(PyValueError::new_err(
                "MountPolicy.require needs at least one MountCredential",
            ));
        }
        Ok(Self {
            inner: MountPolicy::Require(credentials.iter().map(|c| c.inner.clone()).collect()),
        })
    }

    /// Refuse with `status` (a `RefuseStatus` member or its code: 403, 404,
    /// 453 or 503). Raises `ValueError` for any other code.
    #[staticmethod]
    fn refuse(status: u16) -> PyResult<Self> {
        let status = RefuseStatus::from_code(status).ok_or_else(|| {
            PyValueError::new_err(format!(
                "refuse status {status} is not one of 403, 404, 453, 503"
            ))
        })?;
        Ok(Self {
            inner: MountPolicy::Refuse(status),
        })
    }

    fn __repr__(&self) -> String {
        format!("MountPolicy::{:?}", self.inner)
    }
}

/// The publish and read policies for one mount path (or the default). A
/// direction left out inherits: a path rule's `inherit()` takes the table's
/// default rule for that direction; the default rule's `inherit()` is the
/// server-wide `auth` setting (open when there is none).
#[pyclass(name = "MountRule", module = "tstrans.rtp", frozen)]
pub(crate) struct PyMountRule {
    pub(crate) rule: MountRule,
}

#[pymethods]
impl PyMountRule {
    #[new]
    #[pyo3(signature = (publish=None, read=None))]
    fn new(
        publish: Option<PyRef<'_, PyMountPolicy>>,
        read: Option<PyRef<'_, PyMountPolicy>>,
    ) -> Self {
        let pick = |p: Option<PyRef<'_, PyMountPolicy>>| {
            p.map_or(MountPolicy::Inherit, |p| p.inner.clone())
        };
        Self {
            rule: MountRule::new(pick(publish), pick(read)),
        }
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.rule)
    }
}

/// How many requests one rule refused for the credential they carried, per
/// direction, from `MountRules.auth_failures(path)` and
/// `MountRules.default_auth_failures()`. Frozen snapshot.
///
/// A request counts when it carried an `Authorization` header and was
/// answered 401 for any reason but a stale nonce: a wrong password, an
/// unknown user, a malformed header, the wrong scheme. The first challenge
/// every client gets (no header yet), a stale nonce and a `refuse` policy's
/// status never count. An honest client's first answer after
/// `set_challenge` changed the scheme or the realm does.
///
/// A rate signal: a peer with no credential can raise it at will, and a
/// peer that holds one valid credential can try others without bound on one
/// connection, so a lock-out built on one name's count can be triggered
/// against that name's owner.
#[pyclass(name = "MountRuleAuthFailures", module = "tstrans.rtp", frozen)]
pub(crate) struct PyMountRuleAuthFailures {
    inner: MountRuleAuthFailures,
}

#[pymethods]
impl PyMountRuleAuthFailures {
    /// Refused requests that publish (`ANNOUNCE`, and the requests of a
    /// session that announced).
    #[getter]
    fn publish(&self) -> u64 {
        self.inner.publish
    }

    /// Refused requests that read (`DESCRIBE`, and the requests of a reader
    /// or an idle session).
    #[getter]
    fn read(&self) -> u64 {
        self.inner.read
    }

    fn __repr__(&self) -> String {
        format!(
            "MountRuleAuthFailures(publish={}, read={})",
            self.inner.publish, self.inner.read
        )
    }
}

/// One server's per-mount-path rules, from `RtspServer.mount_rules()`. An
/// edit applies to the next request and is safe while the server runs. Once
/// the server is stopped, cancelled or garbage-collected every edit raises
/// `RtspError(CLOSED)`; `len()` and `is_closed()` keep answering.
#[pyclass(name = "MountRules", module = "tstrans.rtp", frozen)]
pub(crate) struct PyMountRules {
    rules: MountRules,
}

impl PyMountRules {
    pub(crate) fn new(rules: MountRules) -> Self {
        Self { rules }
    }
}

#[pymethods]
impl PyMountRules {
    /// Set the rule for `path`, replacing any earlier one. A rule's path
    /// must be canonical (`/amber`, never `/amber/`); `set` refuses anything
    /// else. Raises
    /// `RtspError(MOUNT)` for an invalid path, a credential with an empty
    /// username or a username listed twice, `RtspError(CLOSED)` once the server is stopped.
    fn set(&self, py: Python<'_>, path: &str, rule: &PyMountRule) -> PyResult<()> {
        self.rules
            .set(path, rule.rule.clone())
            .map_err(|e| rule_err(py, e))
    }

    /// Remove the rule for `path`; `True` if there was one.
    fn remove(&self, py: Python<'_>, path: &str) -> PyResult<bool> {
        self.rules.remove(path).map_err(|e| rule_err(py, e))
    }

    /// Set the rule every path without its own entry follows. Raises like
    /// `set`.
    fn set_default(&self, py: Python<'_>, rule: &PyMountRule) -> PyResult<()> {
        self.rules
            .set_default(rule.rule.clone())
            .map_err(|e| rule_err(py, e))
    }

    /// Remove every path rule; the default rule and the challenge stay.
    fn clear(&self, py: Python<'_>) -> PyResult<()> {
        self.rules.clear().map_err(|e| rule_err(py, e))
    }

    /// Set the scheme (an `AuthScheme` member) and realm of every 401 the
    /// server sends, and so the scheme a credential is checked in: a
    /// `require` policy's and, on a server whose config carries `auth`, that
    /// credential's too. The table starts with `auth`'s scheme and realm, or
    /// Digest (MD5) with realm `tstrans` without one. A change applies from
    /// the next request on; a Digest answer computed for the old realm is
    /// then refused. A user name that contains `:` cannot authenticate
    /// while the challenge is Basic, the config's own credential included.
    fn set_challenge(&self, py: Python<'_>, scheme: u8, realm: &str) -> PyResult<()> {
        let scheme = match scheme {
            0 => AuthScheme::Basic,
            1 => AuthScheme::DigestMd5,
            2 => AuthScheme::DigestSha256,
            other => {
                return Err(PyValueError::new_err(format!(
                    "auth scheme {other} is not one of 0 (BASIC), 1 (DIGEST_MD5), 2 (DIGEST_SHA256)"
                )));
            }
        };
        self.rules
            .set_challenge(scheme, realm)
            .map_err(|e| rule_err(py, e))
    }

    /// The requests refused for their credential on `path`'s own rule
    /// (see `MountRuleAuthFailures`); `None` when `path` has no rule of its
    /// own, as a request for such a path counts on the default rule. A
    /// rule's counts survive `set` on the same path; `remove` and `clear`
    /// drop them. Answers after the server shut down.
    fn auth_failures(&self, path: &str) -> Option<PyMountRuleAuthFailures> {
        self.rules
            .auth_failures(path)
            .map(|inner| PyMountRuleAuthFailures { inner })
    }

    /// The requests refused for their credential on the default rule: those
    /// for every path without a rule of its own. Answers after the server
    /// shut down.
    fn default_auth_failures(&self) -> PyMountRuleAuthFailures {
        PyMountRuleAuthFailures {
            inner: self.rules.default_auth_failures(),
        }
    }

    /// Whether the server has shut down, after which edits raise.
    fn is_closed(&self) -> bool {
        self.rules.is_closed()
    }

    /// The number of path rules (the default rule is not counted).
    fn __len__(&self) -> usize {
        self.rules.len()
    }

    fn __repr__(&self) -> String {
        if self.rules.is_closed() {
            format!("MountRules(len={}, closed)", self.rules.len())
        } else {
            format!("MountRules(len={})", self.rules.len())
        }
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMountCredential>()?;
    m.add_class::<PyMountPolicy>()?;
    m.add_class::<PyMountRule>()?;
    m.add_class::<PyMountRules>()?;
    m.add_class::<PyMountRuleAuthFailures>()?;
    Ok(())
}
