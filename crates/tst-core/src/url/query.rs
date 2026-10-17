//! The key table a transport's URL parser is built on.
//!
//! Internal tier: the types exist for the transport crates and the doc
//! generator, not for consumers.
//!
//! A scheme declares its query keys once, as a `const` slice of
//! [`QueryKey`]. [`apply_query`] walks the URL's pairs through the table
//! and [`docs`] renders the same rows for the documentation, so the parser
//! and the documented key list cannot drift.

use super::common::UrlError;
use alloc::string::ToString;

/// One query key of a scheme: its names, its documentation and the
/// function that applies a value to the scheme's config `C`. A scheme
/// that reports its own error type names it as `E`; the default is the
/// shared [`UrlError`].
#[derive(Debug)]
#[non_exhaustive]
pub struct QueryKey<C, E = UrlError> {
    /// The key's canonical spelling.
    pub canonical: &'static str,
    /// Other spellings that mean the same key.
    pub aliases: &'static [&'static str],
    /// What the key does, in the words of the conventions table.
    pub meaning: &'static str,
    /// The unit or accepted values for this scheme.
    pub unit: &'static str,
    /// Applies `value` (under the spelling the URL used) to the config.
    pub apply: fn(&mut C, key: &str, value: &str) -> Result<(), E>,
}

impl<C, E> QueryKey<C, E> {
    /// Builds a row; `const` so a table can be a `const` slice.
    pub const fn new(
        canonical: &'static str,
        aliases: &'static [&'static str],
        meaning: &'static str,
        unit: &'static str,
        apply: fn(&mut C, &str, &str) -> Result<(), E>,
    ) -> Self {
        Self {
            canonical,
            aliases,
            meaning,
            unit,
            apply,
        }
    }
}

/// What to do with a key the table does not list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownKeys {
    /// Skip the pair.
    Ignore,
    /// Fail with [`UrlError::UnknownQueryKey`].
    Reject,
}

/// Finds the row whose canonical name or alias is `key`.
pub fn lookup<'t, C, E>(table: &'t [QueryKey<C, E>], key: &str) -> Option<&'t QueryKey<C, E>> {
    table
        .iter()
        .find(|row| row.canonical == key || row.aliases.contains(&key))
}

/// Applies every pair in URL order through `table`; an unknown key is
/// skipped or rejected (`UnknownQueryKey`) per `unknown`. Returns the
/// first error unchanged (the caller may wrap it). A later duplicate
/// overwrites an earlier one because each pair is applied in turn.
pub fn apply_query<C>(
    table: &[QueryKey<C>],
    cfg: &mut C,
    pairs: &[(impl AsRef<str>, impl AsRef<str>)],
    unknown: UnknownKeys,
) -> Result<(), UrlError> {
    for (k, v) in pairs {
        let (k, v) = (k.as_ref(), v.as_ref());
        match lookup(table, k) {
            Some(row) => (row.apply)(cfg, k, v)?,
            None if unknown == UnknownKeys::Reject => {
                return Err(UrlError::UnknownQueryKey { key: k.to_string() });
            }
            None => {}
        }
    }
    Ok(())
}

/// The scheme-erased view the doc generator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDoc {
    /// The URL scheme the row belongs to.
    pub scheme: &'static str,
    /// The key's canonical spelling.
    pub canonical: &'static str,
    /// Other spellings of the key.
    pub aliases: &'static [&'static str],
    /// What the key does.
    pub meaning: &'static str,
    /// The unit or accepted values.
    pub unit: &'static str,
}

/// The documentation rows of `table`, in table order, tagged with `scheme`.
pub fn docs<'t, C, E>(
    scheme: &'static str,
    table: &'t [QueryKey<C, E>],
) -> impl Iterator<Item = KeyDoc> + 't {
    table.iter().map(move |r| KeyDoc {
        scheme,
        canonical: r.canonical,
        aliases: r.aliases,
        meaning: r.meaning,
        unit: r.unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::common::{parse_bool, parse_int};
    use alloc::vec::Vec;

    #[derive(Default, Debug, PartialEq)]
    struct Cfg {
        n: u32,
        on: bool,
    }

    fn set_n(c: &mut Cfg, k: &str, v: &str) -> Result<(), UrlError> {
        c.n = parse_int(k, v)?;
        Ok(())
    }
    fn set_on(c: &mut Cfg, k: &str, v: &str) -> Result<(), UrlError> {
        c.on = parse_bool(k, v)?;
        Ok(())
    }

    const TABLE: &[QueryKey<Cfg>] = &[
        QueryKey::new("count", &["n", "cnt"], "how many", "integer", set_n),
        QueryKey::new("flag", &[], "switch", "1/0", set_on),
    ];

    fn run(pairs: &[(&str, &str)], u: UnknownKeys) -> (Cfg, Result<(), UrlError>) {
        let mut c = Cfg::default();
        let r = apply_query(TABLE, &mut c, pairs, u);
        (c, r)
    }

    #[test]
    fn canonical_and_alias_resolve_to_one_row() {
        let a = lookup(TABLE, "count").unwrap();
        assert!(core::ptr::eq(a, lookup(TABLE, "n").unwrap()));
        assert!(core::ptr::eq(a, lookup(TABLE, "cnt").unwrap()));
        assert!(lookup(TABLE, "nope").is_none());
    }

    #[test]
    fn unknown_keys_follow_the_policy() {
        let (c, r) = run(&[("zzz", "1"), ("n", "3")], UnknownKeys::Ignore);
        assert_eq!((r, c.n), (Ok(()), 3));
        let (_, r) = run(&[("zzz", "1")], UnknownKeys::Reject);
        assert_eq!(r, Err(UrlError::UnknownQueryKey { key: "zzz".into() }));
    }

    #[test]
    fn apply_errors_propagate_unchanged_and_stop() {
        let (c, r) = run(&[("flag", "maybe"), ("n", "9")], UnknownKeys::Ignore);
        assert!(matches!(r, Err(UrlError::BadQueryValue { ref key, .. }) if key == "flag"));
        assert_eq!(c.n, 0);
    }

    #[test]
    fn later_duplicate_wins_in_url_order() {
        let (c, _) = run(
            &[("n", "1"), ("count", "2"), ("cnt", "3")],
            UnknownKeys::Ignore,
        );
        assert_eq!(c.n, 3);
    }

    #[test]
    fn docs_lists_rows_in_order() {
        let d: Vec<KeyDoc> = docs("x", TABLE).collect();
        assert_eq!(d.len(), 2);
        assert_eq!(
            (d[0].scheme, d[0].canonical, d[0].aliases),
            ("x", "count", &["n", "cnt"][..])
        );
        assert_eq!(d[1].unit, "1/0");
    }
}
