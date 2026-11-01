//! Parser for libsrt's recommended `#!::key=value,...` stream-ID form.
//!
//! libsrt's access-control convention puts `key=value` pairs after a `#!::`
//! prefix, for example `#!::u=admin,r=stream1`. Values are taken as given:
//! the form has no escaping, so a value cannot contain a comma.

/// The pairs of a `#!::` stream ID, in the order they appeared.
///
/// `Debug` prints the keys and `<redacted>` for every value: stream-ID
/// values carry user names and tokens.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccessControl {
    pairs: Vec<(String, String)>,
}

/// The `t` key: what the caller wants to transfer.
///
/// `Debug` redacts the value of [`AccessKind::Other`].
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessKind {
    /// `t=stream` or no `t` key.
    Stream,
    /// `t=file`.
    File,
    /// `t=auth`.
    Auth,
    /// Any other value, as given.
    Other(String),
}

/// The `m` key: the direction of the transfer.
///
/// `Debug` redacts the value of [`AccessMode::Other`].
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessMode {
    /// `m=request` or no `m` key.
    Request,
    /// `m=publish`.
    Publish,
    /// `m=bidirectional`.
    Bidirectional,
    /// Any other value, as given.
    Other(String),
}

impl core::fmt::Debug for AccessControl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        struct Pairs<'a>(&'a [(String, String)]);
        impl core::fmt::Debug for Pairs<'_> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_list()
                    .entries(self.0.iter().map(|(k, _)| (k, "<redacted>")))
                    .finish()
            }
        }
        f.debug_struct("AccessControl")
            .field("pairs", &Pairs(&self.pairs))
            .finish()
    }
}

impl core::fmt::Debug for AccessKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Stream => f.write_str("Stream"),
            Self::File => f.write_str("File"),
            Self::Auth => f.write_str("Auth"),
            Self::Other(_) => f.debug_tuple("Other").field(&"<redacted>").finish(),
        }
    }
}

impl core::fmt::Debug for AccessMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Request => f.write_str("Request"),
            Self::Publish => f.write_str("Publish"),
            Self::Bidirectional => f.write_str("Bidirectional"),
            Self::Other(_) => f.debug_tuple("Other").field(&"<redacted>").finish(),
        }
    }
}

/// Why a stream ID did not parse as an access-control string.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessControlError {
    /// The stream ID does not start with `#!`.
    #[error("not an access-control stream id (no `#!` prefix)")]
    NotAccessControl,
    /// The nested `#!{...}` form.
    #[error("nested `#!{{` form is not supported")]
    Unsupported,
    /// A malformed pair; the number is the byte offset of the offending
    /// segment (2 for a `#!` not followed by `::`, right after the prefix).
    #[error("syntax error at byte {0}")]
    Syntax(usize),
    /// The same key appeared twice.
    #[error("duplicate key `{0}`")]
    DuplicateKey(String),
}

impl AccessControl {
    /// Parses a `#!::key=value,...` stream ID.
    ///
    /// # Errors
    /// See [`AccessControlError`].
    pub fn parse(stream_id: &str) -> Result<Self, AccessControlError> {
        let Some(rest) = stream_id.strip_prefix("#!") else {
            return Err(AccessControlError::NotAccessControl);
        };
        if rest.starts_with('{') {
            return Err(AccessControlError::Unsupported);
        }
        let Some(content) = rest.strip_prefix("::") else {
            return Err(AccessControlError::Syntax(2));
        };
        let mut pairs: Vec<(String, String)> = Vec::new();
        if content.is_empty() {
            return Ok(Self { pairs });
        }
        let mut offset = 4;
        for segment in content.split(',') {
            let (key, value) = match segment.split_once('=') {
                Some((k, v)) if !k.is_empty() => (k, v),
                _ => return Err(AccessControlError::Syntax(offset)),
            };
            if pairs.iter().any(|(k, _)| k == key) {
                return Err(AccessControlError::DuplicateKey(key.to_owned()));
            }
            pairs.push((key.to_owned(), value.to_owned()));
            offset += segment.len() + 1;
        }
        Ok(Self { pairs })
    }

    /// Every `(key, value)` pair, in the order the stream ID gave them.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The `u` key: user name.
    pub fn user(&self) -> Option<&str> {
        self.get("u")
    }

    /// The `r` key: resource name.
    pub fn resource(&self) -> Option<&str> {
        self.get("r")
    }

    /// The `h` key: host name the caller used.
    pub fn host(&self) -> Option<&str> {
        self.get("h")
    }

    /// The `s` key: session id.
    pub fn session(&self) -> Option<&str> {
        self.get("s")
    }

    /// The `t` key; [`AccessKind::Stream`] when absent.
    pub fn kind(&self) -> AccessKind {
        match self.get("t") {
            None | Some("stream") => AccessKind::Stream,
            Some("file") => AccessKind::File,
            Some("auth") => AccessKind::Auth,
            Some(other) => AccessKind::Other(other.to_owned()),
        }
    }

    /// The `m` key; [`AccessMode::Request`] when absent.
    pub fn mode(&self) -> AccessMode {
        match self.get("m") {
            None | Some("request") => AccessMode::Request,
            Some("publish") => AccessMode::Publish,
            Some("bidirectional") => AccessMode::Bidirectional,
            Some(other) => AccessMode::Other(other.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::StreamId;

    #[test]
    fn debug_prints_keys_and_never_values() {
        let ac = AccessControl::parse("#!::u=admin,s=token1,t=weird,m=odd").unwrap();
        let shown = format!("{ac:?} {:?} {:?}", ac.kind(), ac.mode());
        for secret in ["admin", "token1", "weird", "odd"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(
            shown.contains("\"u\"") && shown.contains("\"s\""),
            "{shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn doc_example_user_and_resource() {
        let ac = AccessControl::parse("#!::u=admin,r=bluesbrothers1_hi").unwrap();
        assert_eq!(ac.user(), Some("admin"));
        assert_eq!(ac.resource(), Some("bluesbrothers1_hi"));
        assert_eq!(ac.kind(), AccessKind::Stream);
        assert_eq!(ac.mode(), AccessMode::Request);
    }
    #[test]
    fn doc_example_file_publish() {
        let ac = AccessControl::parse("#!::u=johnny,t=file,m=publish,r=results.csv").unwrap();
        assert_eq!(ac.kind(), AccessKind::File);
        assert_eq!(ac.mode(), AccessMode::Publish);
        assert_eq!(ac.resource(), Some("results.csv"));
    }
    #[test]
    fn pairs_iterate_in_input_order() {
        let ac = AccessControl::parse("#!::u=a,user_x=1,r=b").unwrap();
        let got: Vec<_> = ac.pairs().collect();
        assert_eq!(got, [("u", "a"), ("user_x", "1"), ("r", "b")]);
    }
    #[test]
    fn custom_keys_and_other_values() {
        let ac =
            AccessControl::parse("#!::user_tier=gold,t=telemetry,m=bidirectional,h=cam.example")
                .unwrap();
        assert_eq!(ac.get("user_tier"), Some("gold"));
        assert_eq!(ac.kind(), AccessKind::Other("telemetry".into()));
        assert_eq!(ac.mode(), AccessMode::Bidirectional);
        assert_eq!(ac.host(), Some("cam.example"));
        assert_eq!(ac.get("u"), None);
    }
    #[test]
    fn empty_content_and_empty_value() {
        assert_eq!(AccessControl::parse("#!::").unwrap().user(), None);
        assert_eq!(AccessControl::parse("#!::s=").unwrap().session(), Some(""));
    }
    #[test]
    fn free_form_is_not_access_control() {
        assert_eq!(
            AccessControl::parse("publish:amber"),
            Err(AccessControlError::NotAccessControl)
        );
        assert_eq!(
            AccessControl::parse(""),
            Err(AccessControlError::NotAccessControl)
        );
    }
    #[test]
    fn nested_is_unsupported() {
        assert_eq!(
            AccessControl::parse("#!{u=admin}"),
            Err(AccessControlError::Unsupported)
        );
    }
    #[test]
    fn syntax_errors() {
        assert_eq!(
            AccessControl::parse("#!::novalue"),
            Err(AccessControlError::Syntax(4))
        );
        assert_eq!(
            AccessControl::parse("#!::=x"),
            Err(AccessControlError::Syntax(4))
        );
        assert_eq!(
            AccessControl::parse("#!?u=a"),
            Err(AccessControlError::Syntax(2))
        );
    }
    #[test]
    fn duplicate_key_is_refused() {
        assert_eq!(
            AccessControl::parse("#!::u=a,u=b"),
            Err(AccessControlError::DuplicateKey("u".into()))
        );
    }
    #[test]
    fn stream_id_convenience() {
        let id = StreamId::new("#!::r=front").unwrap();
        assert_eq!(id.access_control().unwrap().resource(), Some("front"));
    }
}
