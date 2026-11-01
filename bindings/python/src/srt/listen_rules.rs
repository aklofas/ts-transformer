//! `ListenRule`, `ListenRules` and the native `RejectReason` table.
//!
//! `ListenRules` holds the listener's `tst_srt::ListenRules` handle, taken
//! from the `Listener` snapshot, so an edit never waits behind a parked
//! `accept()`. None of these methods blocks (a table write waits at most on
//! the handshake callback's read), so none releases the GIL.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_srt::error::RejectReason;
use tst_srt::{AccessControl, AccessKind, AccessMode, ListenRuleError, ListenRules, Passphrase};

use crate::raise::{SRT, raise};
use crate::util::CancelSource;

/// The lowest code a rule may reject with; libsrt owns the codes below.
const FIRST_APPLICATION_REASON: i32 = 1000;

/// The typed predefined reasons and their `tstrans.srt.RejectReason` member
/// names; `test_reject_reason_table_matches_native` holds the Python enum to
/// this list.
const PREDEFINED: [(RejectReason, &str); 21] = [
    (RejectReason::Fallback, "FALLBACK"),
    (RejectReason::KeyNotSupported, "KEY_NOT_SUPPORTED"),
    (RejectReason::Filepath, "FILEPATH"),
    (RejectReason::HostNotFound, "HOST_NOT_FOUND"),
    (RejectReason::BadRequest, "BAD_REQUEST"),
    (RejectReason::Unauthorized, "UNAUTHORIZED"),
    (RejectReason::Overload, "OVERLOAD"),
    (RejectReason::Forbidden, "FORBIDDEN"),
    (RejectReason::NotFound, "NOT_FOUND"),
    (RejectReason::BadMode, "BAD_MODE"),
    (RejectReason::Unacceptable, "UNACCEPTABLE"),
    (RejectReason::Conflict, "CONFLICT"),
    (RejectReason::NotSupportedMedia, "NOT_SUPPORTED_MEDIA"),
    (RejectReason::Locked, "LOCKED"),
    (RejectReason::FailedDependency, "FAILED_DEPENDENCY"),
    (RejectReason::InternalServerError, "INTERNAL_SERVER_ERROR"),
    (RejectReason::Unimplemented, "UNIMPLEMENTED"),
    (RejectReason::Gateway, "GATEWAY"),
    (RejectReason::Down, "DOWN"),
    (RejectReason::VersionUnsupported, "VERSION_UNSUPPORTED"),
    (RejectReason::NoRoom, "NO_ROOM"),
];

/// The typed variant for a predefined code, `Other(code)` for any other.
fn reason_from_code(code: i32) -> RejectReason {
    PREDEFINED
        .iter()
        .map(|(r, _)| *r)
        .find(|r| r.raw() == code)
        .unwrap_or(RejectReason::Other(code))
}

/// `(code, name)` for each predefined reject reason, in code order.
#[pyfunction]
fn _reject_reason_table() -> Vec<(i32, &'static str)> {
    PREDEFINED.iter().map(|(r, n)| (r.raw(), *n)).collect()
}

/// The rule as written; the passphrase is validated when the rule is
/// installed, so `set` reports a bad one the way `Builder` does.
#[derive(Clone)]
enum Rule {
    Accept,
    AcceptWith(String),
    Reject(RejectReason),
}

/// What a listener does with a caller: accept, accept with a passphrase
/// the caller must match, or reject with a reason the caller reads.
#[pyclass(name = "ListenRule", module = "tstrans.srt", frozen)]
pub(crate) struct PyListenRule {
    rule: Rule,
}

impl PyListenRule {
    fn build(&self, py: Python<'_>) -> PyResult<tst_srt::ListenRule> {
        Ok(match &self.rule {
            Rule::Accept => tst_srt::ListenRule::Accept,
            Rule::AcceptWith(p) => tst_srt::ListenRule::AcceptWith {
                passphrase: Passphrase::new(p.clone()).map_err(|e| {
                    raise(
                        py,
                        &SRT,
                        BindingError::new(BindingErrorKind::ConfigInvalid, e.to_string()),
                    )
                })?,
            },
            Rule::Reject(r) => tst_srt::ListenRule::Reject(*r),
        })
    }
}

#[pymethods]
impl PyListenRule {
    /// Accept with the listener's own settings.
    #[staticmethod]
    fn accept() -> Self {
        Self { rule: Rule::Accept }
    }

    /// Accept, applying `passphrase` to the connection; libsrt rejects a
    /// caller whose passphrase differs with `BadSecret`. The passphrase is
    /// checked when the rule is installed: `ListenRules.set` raises
    /// `SrtError(CONFIG_INVALID)` for one outside 10..=80 bytes.
    #[staticmethod]
    fn accept_with(passphrase: String) -> Self {
        Self {
            rule: Rule::AcceptWith(passphrase),
        }
    }

    /// Reject during the handshake with `reason`: a `RejectReason` member or
    /// any code >= 1000 (`user_defined` for 2000 and up). Raises
    /// `ValueError` below 1000 (codes reserved for libsrt).
    #[staticmethod]
    fn reject(reason: i32) -> PyResult<Self> {
        if reason < FIRST_APPLICATION_REASON {
            return Err(PyValueError::new_err(format!(
                "reject reason {reason} is reserved for libsrt (codes below 1000)"
            )));
        }
        Ok(Self {
            rule: Rule::Reject(reason_from_code(reason)),
        })
    }

    fn __repr__(&self) -> String {
        match &self.rule {
            Rule::Accept => "ListenRule.accept()".to_string(),
            Rule::AcceptWith(_) => "ListenRule.accept_with(<redacted>)".to_string(),
            Rule::Reject(r) => format!("ListenRule.reject({r:?})"),
        }
    }
}

/// One listener's per-stream-ID rules, from `Listener.rules()`. Matching is
/// exact on the stream ID; a caller with no, an empty or a non-UTF-8 stream
/// ID gets the default rule. An edit applies to the next handshake and is
/// safe while another thread is parked in `accept()`. Once the listener is
/// closed or cancelled every edit raises `SrtError(CLOSED)`.
#[pyclass(name = "ListenRules", module = "tstrans.srt", frozen)]
pub(crate) struct PyListenRules {
    rules: ListenRules,
    /// The owning `Listener` sets this on close.
    closed: Arc<AtomicBool>,
    /// The owning `Listener`'s cancel state: `cancel_handle().cancel()`
    /// closes the socket without closing the `Listener`.
    cancel: Arc<CancelSource>,
}

impl PyListenRules {
    pub(crate) fn new(
        rules: ListenRules,
        closed: Arc<AtomicBool>,
        cancel: Arc<CancelSource>,
    ) -> Self {
        Self {
            rules,
            closed,
            cancel,
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.cancel.is_cancelled()
    }

    /// `SrtError(CLOSED)` once the listener is closed or cancelled: the Rust
    /// handle reports only a close, and only from `set` / `set_default`.
    fn check_open(&self, py: Python<'_>) -> PyResult<()> {
        if self.is_closed() {
            return Err(rule_err(py, ListenRuleError::Closed));
        }
        Ok(())
    }
}

fn rule_err(py: Python<'_>, e: ListenRuleError) -> PyErr {
    let kind = match e {
        ListenRuleError::Closed => BindingErrorKind::Closed,
        // `ListenRule.reject` refuses codes below 1000 before this point.
        _ => BindingErrorKind::ConfigInvalid,
    };
    raise(py, &SRT, BindingError::new(kind, e.to_string()))
}

#[pymethods]
impl PyListenRules {
    /// Install `rule` for callers whose stream ID is exactly `stream_id`,
    /// replacing any previous rule for it. Raises `SrtError(CONFIG_INVALID)`
    /// for a bad passphrase, `SrtError(CLOSED)` once the listener is closed.
    fn set(&self, py: Python<'_>, stream_id: &str, rule: &PyListenRule) -> PyResult<()> {
        self.check_open(py)?;
        let rule = rule.build(py)?;
        self.rules.set(stream_id, rule).map_err(|e| rule_err(py, e))
    }

    /// Remove the rule for `stream_id`; `True` if there was one. Raises
    /// `SrtError(CLOSED)` once the listener is closed, as do `clear` and
    /// `len`.
    fn remove(&self, py: Python<'_>, stream_id: &str) -> PyResult<bool> {
        self.check_open(py)?;
        Ok(self.rules.remove(stream_id))
    }

    /// The rule for callers no stream-ID rule matches (initially accept).
    /// Raises like `set`.
    fn set_default(&self, py: Python<'_>, rule: &PyListenRule) -> PyResult<()> {
        self.check_open(py)?;
        let rule = rule.build(py)?;
        self.rules.set_default(rule).map_err(|e| rule_err(py, e))
    }

    /// Remove every stream-ID rule; the default rule stays.
    fn clear(&self, py: Python<'_>) -> PyResult<()> {
        self.check_open(py)?;
        self.rules.clear();
        Ok(())
    }

    /// The number of stream-ID rules (the default rule is not counted).
    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        self.check_open(py)?;
        Ok(self.rules.len())
    }

    fn __repr__(&self) -> String {
        if self.is_closed() {
            "ListenRules(closed)".to_string()
        } else {
            format!("ListenRules(len={})", self.rules.len())
        }
    }
}

/// A parsed `#!::k=v` access-control stream ID (libsrt's structured form).
#[pyclass(name = "AccessControl", module = "tstrans.srt", frozen)]
pub(crate) struct PyAccessControl {
    inner: AccessControl,
}

#[pymethods]
impl PyAccessControl {
    /// Parse a `#!::k=v,...` stream ID. Raises `ValueError` for a string
    /// without the `#!` prefix, the nested `#!{` form, a syntax error or a
    /// duplicate key.
    #[staticmethod]
    fn parse(stream_id: &str) -> PyResult<Self> {
        AccessControl::parse(stream_id)
            .map(|inner| Self { inner })
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    /// The `u` key, if present.
    #[getter]
    fn user(&self) -> Option<String> {
        self.inner.user().map(str::to_owned)
    }

    /// The `r` key, if present.
    #[getter]
    fn resource(&self) -> Option<String> {
        self.inner.resource().map(str::to_owned)
    }

    /// The `h` key, if present.
    #[getter]
    fn host(&self) -> Option<String> {
        self.inner.host().map(str::to_owned)
    }

    /// The `s` key, if present.
    #[getter]
    fn session(&self) -> Option<String> {
        self.inner.session().map(str::to_owned)
    }

    /// The `t` key: `"stream"`, `"file"`, `"auth"` or the raw text.
    #[getter]
    fn kind(&self) -> String {
        match self.inner.kind() {
            AccessKind::Stream => "stream".to_owned(),
            AccessKind::File => "file".to_owned(),
            AccessKind::Auth => "auth".to_owned(),
            AccessKind::Other(s) => s,
            _ => String::new(),
        }
    }

    /// The `m` key: `"request"`, `"publish"`, `"bidirectional"` or the raw
    /// text.
    #[getter]
    fn mode(&self) -> String {
        match self.inner.mode() {
            AccessMode::Request => "request".to_owned(),
            AccessMode::Publish => "publish".to_owned(),
            AccessMode::Bidirectional => "bidirectional".to_owned(),
            AccessMode::Other(s) => s,
            _ => String::new(),
        }
    }

    /// The value for `key`, or `None`.
    fn get(&self, key: &str) -> Option<String> {
        self.inner.get(key).map(str::to_owned)
    }

    /// Every `(key, value)` pair in input order.
    fn items(&self) -> Vec<(String, String)> {
        self.inner
            .pairs()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn __repr__(&self) -> String {
        let redacted: Vec<(String, &str)> = self
            .items()
            .into_iter()
            .map(|(k, _)| (k, "<redacted>"))
            .collect();
        format!("AccessControl({redacted:?})")
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyListenRule>()?;
    m.add_class::<PyListenRules>()?;
    m.add_class::<PyAccessControl>()?;
    m.add_function(wrap_pyfunction!(_reject_reason_table, m)?)?;
    Ok(())
}
