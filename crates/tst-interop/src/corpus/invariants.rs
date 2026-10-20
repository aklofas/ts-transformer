//! Invariant checks over a digest. This module holds the failure record the
//! sweep reports; the checks that produce it are added alongside.

use serde::{Deserialize, Serialize};

/// One broken invariant. `rule` is `"feed-error"`, `"klv-unknown"`,
/// `"accounting"` or `"pts-order"`; it is a `String` so a report can be read
/// back from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantFailure {
    pub rule: String,
    pub pid: Option<u16>,
    pub detail: String,
}
