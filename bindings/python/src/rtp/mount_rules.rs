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
    AuthScheme, MountCredential, MountPolicy, MountRule, MountRuleError, MountRules, RefuseStatus,
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
    /// configured, else open. A path rule's `inherit()` does not fall back to
    /// the table's default rule: under a default of `refuse(...)`, a path rule
    /// with one direction left `inherit()` leaves that direction as open as
    /// the server itself is.
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
        let status = match status {
            403 => RefuseStatus::Forbidden,
            404 => RefuseStatus::NotFound,
            453 => RefuseStatus::NotEnoughBandwidth,
            503 => RefuseStatus::ServiceUnavailable,
            other => {
                return Err(PyValueError::new_err(format!(
                    "refuse status {other} is not one of 403, 404, 453, 503"
                )));
            }
        };
        Ok(Self {
            inner: MountPolicy::Refuse(status),
        })
    }

    fn __repr__(&self) -> String {
        format!("MountPolicy::{:?}", self.inner)
    }
}

/// The publish and read policies for one mount path (or the default). A
/// direction left out inherits: it follows the server-wide `auth` setting
/// (open when there is none), not the table's default rule.
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

/// One server's per-mount-path rules, from `RtspServer.mount_rules()`. An
/// edit applies to the next request and is safe while the server runs. Once
/// the server is stopped every edit raises `RtspError(SERVER)`; `len()` and
/// `is_closed()` keep answering.
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
    /// Set the rule for `path`, replacing any earlier one. The path is keyed
    /// the way the server resolves a request URI, so `/amber/` names
    /// `/amber`. Raises
    /// `RtspError(MOUNT)` for an invalid path or a credential with an empty
    /// username, `RtspError(SERVER)` once the server is stopped.
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

    /// Set the scheme (an `AuthScheme` member) and realm a `require` policy
    /// challenges with. Without a server credential the challenge is Digest
    /// (MD5) with realm `tstrans`.
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

    /// Whether the server has stopped, after which edits raise.
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
    Ok(())
}
