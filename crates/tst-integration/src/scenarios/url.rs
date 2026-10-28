//! The canonical URL corpus (`tests/fixtures/scenarios/url-corpus/corpus.toml`): the
//! normalised record every binding's URL handling is compared as, and the
//! loader each Rust-side leg (the Rust parsers, the C ABI) reads it through.
//!
//! The record is binding-neutral: the Python leg reads the same TOML with
//! `tomllib` and compares the same fields, and a JVM leg reads it the same way.
//! See the file's header for the record's rules and the per-leg entry points.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use tst_pipeline::binding::BindingErrorKind;

/// The whole corpus file.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema_version: u32,
    /// Documented-key counts per scheme family.
    pub schemes: BTreeMap<String, SchemeKeys>,
    /// Entry point per leg, per scheme family, per role (plus `probe`).
    pub legs: BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>,
    /// The rows, in file order.
    pub url: Vec<Row>,
}

/// The header's key accounting for one scheme family.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SchemeKeys {
    /// Documented keys, as the parser's table lists them.
    pub keys: usize,
    /// Alias spellings across those keys.
    pub aliases: usize,
    /// Keys whose value no parser refuses, so they have no `invalid` row.
    pub no_invalid: Vec<String>,
}

/// One corpus row.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// The URL's scheme as written (`tcps`, `rtsps`, ...).
    pub scheme: String,
    /// Which entry point opens it (`caller`, `send`, `recv`, `listener`, ...).
    pub role: String,
    /// `default`, `explicit`, `alias`, `credentials`, `invalid` or `conflict`.
    pub case: String,
    /// The canonical key the row exercises, when it exercises one.
    pub key: Option<String>,
    /// `open` when the parser accepts the URL and the role's opener refuses it.
    pub stage: Option<String>,
    /// Record fields the wire legs must see absent from the requests, though
    /// the parser holds them (space-separated, the row's comment says why).
    pub wire_absent: Option<String>,
    pub url: String,
    pub expect: Record,
}

/// What a leg observed for one URL: the parsed record, or an error kind.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Set for a URL the parser or the opener refuses; every other field is
    /// then unset.
    pub error_kind: Option<String>,
    pub scheme: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: Option<String>,
    pub bind: Option<bool>,
    pub username: Option<String>,
    pub password_set: Option<bool>,
    pub keys: Option<BTreeMap<String, String>>,
}

impl Record {
    /// The record of a refused URL.
    pub fn error(kind: BindingErrorKind) -> Self {
        Self {
            error_kind: Some(kind.name().to_string()),
            ..Self::default()
        }
    }

    /// Whether this is the record of a refused URL.
    pub fn is_error(&self) -> bool {
        self.error_kind.is_some()
    }
}

impl Row {
    /// The scheme family the header and the leg tables key on: `tcps` is
    /// `tcp`, `rtsps` is `rtsp`, `hlss` is `hls`, `wheps` is `whep`.
    pub fn family(&self) -> &'static str {
        family(&self.scheme)
    }

    /// A short label for failure messages.
    pub fn label(&self) -> String {
        format!("[{} {} {}] {}", self.scheme, self.role, self.case, self.url)
    }
}

/// The family of a URL scheme. Panics on a scheme the corpus does not cover.
pub fn family(scheme: &str) -> &'static str {
    match scheme {
        "srt" => "srt",
        "rtp" => "rtp",
        "rtsp" | "rtsps" => "rtsp",
        "udp" => "udp",
        "tcp" | "tcps" => "tcp",
        "hls" | "hlss" => "hls",
        "rist" => "rist",
        "whep" | "wheps" => "whep",
        other => panic!("url corpus: unknown scheme {other:?}"),
    }
}

/// The error domain's variant prefix for a family (`UDP_` for `udp`).
fn domain_prefix(family: &str) -> &'static str {
    match family {
        "srt" => "SRT_",
        "rtp" => "RTP_",
        "rtsp" => "RTSP_",
        "udp" => "UDP_",
        "tcp" => "TCP_",
        "hls" => "HLS_",
        "rist" => "RIST_",
        "whep" => "WEBRTC_",
        other => panic!("url corpus: unknown family {other:?}"),
    }
}

/// The kind a corpus `error_kind` names for `family`: the domain's kind of
/// that name, or the cross-domain kind (`CONFIG_INVALID`) of that name.
pub fn kind_for(family: &str, name: &str) -> BindingErrorKind {
    let domain = format!("{}{name}", domain_prefix(family));
    BindingErrorKind::ALL
        .iter()
        .copied()
        .find(|k| k.name() == name && (k.variant_name() == domain || k.variant_name() == name))
        .unwrap_or_else(|| panic!("url corpus: no {family} kind named {name:?}"))
}

/// The kinds a refused URL of `family` fails with in this corpus. A valid row
/// may fail at open for another reason (a refused connect); failing with one
/// of these is a mismatch.
pub fn url_kinds(corpus: &Corpus, family: &str) -> Vec<BindingErrorKind> {
    let mut kinds: Vec<BindingErrorKind> = Vec::new();
    for row in corpus.url.iter().filter(|r| r.family() == family) {
        if let Some(name) = &row.expect.error_kind {
            let k = kind_for(family, name);
            if !kinds.contains(&k) {
                kinds.push(k);
            }
        }
    }
    kinds
}

/// The corpus file's path.
pub fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scenarios")
        .join("url-corpus")
        .join("corpus.toml")
}

/// Read and parse the corpus.
pub fn load() -> Corpus {
    let path = corpus_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let corpus: Corpus =
        toml::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()));
    assert!(!corpus.url.is_empty(), "url corpus has no rows");
    corpus
}

impl Corpus {
    /// The entry point `leg` declares for `row`'s role, and the leg's probe.
    pub fn entry(&self, leg: &str, row: &Row) -> (&str, &str) {
        let table = self
            .legs
            .get(leg)
            .and_then(|l| l.get(row.family()))
            .unwrap_or_else(|| panic!("url corpus: leg {leg} declares no {}", row.family()));
        let entry = table.get(&row.role).unwrap_or_else(|| {
            panic!(
                "url corpus: leg {leg} declares no {} role {:?}",
                row.family(),
                row.role
            )
        });
        let probe = table
            .get("probe")
            .unwrap_or_else(|| panic!("url corpus: leg {leg} {} has no probe", row.family()));
        (entry.as_str(), probe.as_str())
    }
}

/// The outcome a probing leg (C, Python) observed when it opened a row's URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The open returned, or failed with a kind outside the scheme's URL kinds.
    Accepted,
    /// The open failed with this kind.
    Refused(BindingErrorKind),
}

/// Compare a probing leg's outcome with the row's record. `Err` carries the
/// mismatch for the failure report.
pub fn check_probe(corpus: &Corpus, row: &Row, observed: &Probe) -> Result<(), String> {
    let family = row.family();
    match (&row.expect.error_kind, observed) {
        (Some(name), Probe::Refused(k)) if kind_for(family, name) == *k => Ok(()),
        (Some(name), other) => Err(format!("expected {name}, observed {other:?}")),
        (None, Probe::Accepted) => Ok(()),
        (None, Probe::Refused(k)) if !url_kinds(corpus, family).contains(k) => Ok(()),
        (None, Probe::Refused(k)) => Err(format!(
            "expected the URL to be accepted, observed {} ({})",
            k.name(),
            k.variant_name()
        )),
    }
}

/// [`check_probe`] for a leg that reports `TST_E_*` codes (C): `None` is an
/// open that returned, `Some(code)` a failed one. Codes are compared through
/// each kind's `c_projection()`, so two kinds C folds into one code match.
pub fn check_probe_code(corpus: &Corpus, row: &Row, observed: Option<i32>) -> Result<(), String> {
    let family = row.family();
    match (&row.expect.error_kind, observed) {
        (Some(name), Some(code)) if kind_for(family, name).c_projection() == code => Ok(()),
        (Some(name), other) => Err(format!(
            "expected {name} (code {}), observed {other:?}",
            kind_for(family, name).c_projection()
        )),
        (None, None) => Ok(()),
        (None, Some(code)) => {
            match url_kinds(corpus, family)
                .iter()
                .find(|k| k.c_projection() == code)
            {
                Some(k) => Err(format!(
                    "expected the URL to be accepted, observed code {code} (the code of {})",
                    k.variant_name()
                )),
                None => Ok(()),
            }
        }
    }
}

/// One field of a record by its corpus name: a top-level field (`host`,
/// `path`, `username`, ...) or `keys.<key>`. The wire legs compare the fields
/// their header lists through this one accessor.
pub fn field(rec: &Record, name: &str) -> Option<String> {
    match name {
        "error_kind" => rec.error_kind.clone(),
        "scheme" => rec.scheme.clone(),
        "host" => rec.host.clone(),
        "port" => rec.port.map(|p| p.to_string()),
        "path" => rec.path.clone(),
        "bind" => rec.bind.map(|b| b.to_string()),
        "username" => rec.username.clone(),
        "password_set" => rec.password_set.map(|b| b.to_string()),
        other => match other.strip_prefix("keys.") {
            Some(k) => rec.keys.as_ref().and_then(|m| m.get(k).cloned()),
            None => panic!("url corpus: unknown record field {other:?}"),
        },
    }
}

impl Corpus {
    /// The record fields `leg` observes on the wire for `row`, when the leg
    /// declares a wire probe for the row's scheme (`wire_schemes`).
    pub fn wire_fields(&self, leg: &str, row: &Row) -> Option<Vec<String>> {
        let table = self.legs.get(leg)?.get(row.family())?;
        let schemes = table.get("wire_schemes")?;
        if !schemes.split_whitespace().any(|s| s == row.scheme) {
            return None;
        }
        let wire = table
            .get("wire")
            .unwrap_or_else(|| panic!("url corpus: leg {leg} has wire_schemes but no wire"));
        Some(wire.split_whitespace().map(str::to_string).collect())
    }
}

impl Row {
    /// What a wire leg must observe for `name`: the record's value, or
    /// nothing for a field the row lists in `wire_absent`.
    pub fn wire_expect(&self, name: &str) -> Option<String> {
        let absent = self
            .wire_absent
            .as_deref()
            .is_some_and(|a| a.split_whitespace().any(|f| f == name));
        if absent {
            None
        } else {
            field(&self.expect, name)
        }
    }
}
