//! The canonical URL corpus (`tests/fixtures/scenarios/url-corpus/corpus.toml`): the
//! normalised record every binding's URL handling is compared as, and the
//! loader each Rust-side leg (the Rust parsers, the C ABI) reads it through.
//!
//! The record is binding-neutral: the Python leg reads the same TOML with
//! `tomllib` and compares the same fields; the JVM leg, which has no TOML
//! reader, reads the JSON twin `gen-scenarios` writes into the scenario's
//! golden. See the file's header for the record's rules and the per-leg entry
//! points.

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
    /// The family of each written scheme, for the legs outside Rust; the
    /// coverage check holds it to [`family`].
    pub families: BTreeMap<String, String>,
    /// The schemes the bindings' shells open, with the error domain of each
    /// (`tst_pipeline::binding::SHELL_SCHEMES`, by variant name).
    pub shell_schemes: BTreeMap<String, String>,
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
    /// Per probing leg, the outcomes the leg may see when they differ from
    /// the record's (space-separated, each `ok` or a kind name); see
    /// [`Row::outcomes`].
    pub open: Option<BTreeMap<String, String>>,
    /// Why `open` departs from the record; required with it.
    pub open_why: Option<String>,
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
    pub port: Option<Port>,
    pub path: Option<String>,
    pub bind: Option<bool>,
    pub username: Option<String>,
    pub password_set: Option<bool>,
    pub keys: Option<BTreeMap<String, String>>,
}

/// A record's port: a number, or [`PORT`] in a row whose URL holds it.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(try_from = "PortText")]
pub enum Port {
    Number(u16),
    Placeholder,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PortText {
    Number(u16),
    Text(String),
}

impl TryFrom<PortText> for Port {
    type Error = String;
    fn try_from(p: PortText) -> Result<Self, String> {
        match p {
            PortText::Number(n) => Ok(Port::Number(n)),
            PortText::Text(t) if t == PORT => Ok(Port::Placeholder),
            PortText::Text(t) => Err(format!("a port is a number or {PORT:?}, not {t:?}")),
        }
    }
}

impl std::fmt::Display for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Port::Number(n) => write!(f, "{n}"),
            Port::Placeholder => f.write_str(PORT),
        }
    }
}

/// In a row's URL and record: a loopback port the leg chooses when it runs
/// the row, so no row binds a fixed port.
pub const PORT: &str = "{port}";

/// In a row's URL and record: a directory the leg creates holding
/// [`TLS_FILES`].
pub const DIR: &str = "{dir}";

/// The files a leg writes into [`DIR`]: a self-signed certificate for
/// `127.0.0.1` and `localhost`, and its private key, both PEM.
pub const TLS_FILES: [&str; 2] = ["server.crt", "server.key"];

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

    /// Whether the row's URL holds [`PORT`].
    pub fn needs_port(&self) -> bool {
        self.url.contains(PORT)
    }

    /// Whether the row's URL holds [`DIR`].
    pub fn needs_dir(&self) -> bool {
        self.url.contains(DIR)
    }

    /// The row with [`PORT`] replaced by `port` and [`DIR`] by `dir`, in its
    /// URL and in its record.
    pub fn resolve(&self, port: u16, dir: &str) -> Row {
        let text = |s: &str| s.replace(PORT, &port.to_string()).replace(DIR, dir);
        let opt = |s: &Option<String>| s.as_deref().map(text);
        let e = &self.expect;
        Row {
            url: text(&self.url),
            expect: Record {
                error_kind: e.error_kind.clone(),
                scheme: opt(&e.scheme),
                host: opt(&e.host),
                port: e.port.map(|p| match p {
                    Port::Placeholder => Port::Number(port),
                    n => n,
                }),
                path: opt(&e.path),
                bind: e.bind,
                username: opt(&e.username),
                password_set: e.password_set,
                keys: e
                    .keys
                    .as_ref()
                    .map(|k| k.iter().map(|(k, v)| (k.clone(), text(v))).collect()),
            },
            ..self.clone()
        }
    }

    /// The outcomes probing leg `leg` may see when it opens the row: the
    /// row's `open` entry for the leg when it has one, else the record's
    /// error kind for a refused row, else [`Outcome::Opened`].
    pub fn outcomes(&self, leg: &str) -> Vec<Outcome> {
        if let Some(text) = self.open.as_ref().and_then(|o| o.get(leg)) {
            return text
                .split_whitespace()
                .map(|w| {
                    Outcome::parse(self.family(), w)
                        .unwrap_or_else(|e| panic!("{}: open.{leg}: {e}", self.label()))
                })
                .collect();
        }
        match &self.expect.error_kind {
            Some(name) => vec![Outcome::Refused(kind_for(self.family(), name))],
            None => vec![Outcome::Opened],
        }
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
    try_kind_for(family, name)
        .unwrap_or_else(|| panic!("url corpus: no {family} kind named {name:?}"))
}

/// [`kind_for`], or `None` when `family` has no kind of that name.
pub fn try_kind_for(family: &str, name: &str) -> Option<BindingErrorKind> {
    let domain = format!("{}{name}", domain_prefix(family));
    BindingErrorKind::ALL
        .iter()
        .copied()
        .find(|k| k.name() == name && (k.variant_name() == domain || k.variant_name() == name))
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

/// What a probing leg (C, Python, the JVM) saw when it opened a row's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The open returned an object (spelled `ok` in the corpus).
    Opened,
    /// The open failed with this kind (spelled by its name).
    Refused(BindingErrorKind),
}

impl Outcome {
    /// The outcome a corpus word names for `family`: `ok`, or a kind name
    /// inside the family's error domain.
    pub fn parse(family: &str, word: &str) -> Result<Self, String> {
        if word == "ok" {
            return Ok(Outcome::Opened);
        }
        try_kind_for(family, word)
            .map(Outcome::Refused)
            .ok_or_else(|| format!("{word:?} is neither ok nor a {family} kind"))
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Opened => f.write_str("ok"),
            Outcome::Refused(k) => write!(f, "{} ({})", k.name(), k.variant_name()),
        }
    }
}

fn one_of(allowed: &[Outcome]) -> String {
    allowed
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Compare what probing leg `leg` saw for `row` with the outcomes the row
/// allows it ([`Row::outcomes`]). `Err` carries the mismatch for the failure
/// report.
pub fn check_outcome(row: &Row, leg: &str, observed: &Outcome) -> Result<(), String> {
    let allowed = row.outcomes(leg);
    if allowed.contains(observed) {
        Ok(())
    } else {
        Err(format!(
            "expected {}, observed {observed}",
            one_of(&allowed)
        ))
    }
}

/// [`check_outcome`] for a leg that reports `TST_E_*` codes (C): `None` is an
/// open that returned, `Some(code)` a failed one. Codes are compared through
/// each kind's `c_projection()`, so two kinds C folds into one code match.
pub fn check_outcome_code(row: &Row, leg: &str, observed: Option<i32>) -> Result<(), String> {
    let allowed = row.outcomes(leg);
    let matches = allowed.iter().any(|o| match (o, observed) {
        (Outcome::Opened, None) => true,
        (Outcome::Refused(k), Some(code)) => k.c_projection() == code,
        _ => false,
    });
    if matches {
        Ok(())
    } else {
        let codes: Vec<String> = allowed
            .iter()
            .map(|o| match o {
                Outcome::Opened => "ok".to_string(),
                Outcome::Refused(k) => format!("{} (code {})", k.name(), k.c_projection()),
            })
            .collect();
        Err(format!(
            "expected {}, observed {}",
            codes.join(" or "),
            observed.map_or("ok".to_string(), |c| format!("code {c}"))
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_udp_send() -> Row {
        Row {
            scheme: "udp".into(),
            role: "send".into(),
            case: "explicit".into(),
            key: Some("pkt_size".into()),
            stage: None,
            wire_absent: None,
            open: None,
            open_why: None,
            url: "udp://127.0.0.1:31301?pkt_size=1316".into(),
            expect: Record {
                scheme: Some("udp".into()),
                host: Some("127.0.0.1".into()),
                port: Some(Port::Number(31301)),
                ..Record::default()
            },
        }
    }

    fn invalid_udp_send() -> Row {
        Row {
            url: "udp://127.0.0.1:31301?pkt_size=x".into(),
            expect: Record::error(BindingErrorKind::UdpUrl),
            case: "invalid".into(),
            ..valid_udp_send()
        }
    }

    #[test]
    fn a_valid_row_reporting_invalid_config_fails() {
        let observed = Outcome::Refused(BindingErrorKind::UdpInvalidConfig);
        assert!(check_outcome(&valid_udp_send(), "python", &observed).is_err());
        let code = BindingErrorKind::UdpInvalidConfig.c_projection();
        assert!(check_outcome_code(&valid_udp_send(), "c", Some(code)).is_err());
        assert!(check_outcome(&valid_udp_send(), "python", &Outcome::Opened).is_ok());
        assert!(check_outcome_code(&valid_udp_send(), "c", None).is_ok());
    }

    #[test]
    fn a_row_expected_ok_that_is_refused_fails() {
        let observed = Outcome::Refused(BindingErrorKind::UdpIo);
        let err = check_outcome(&valid_udp_send(), "jvm", &observed).unwrap_err();
        assert!(err.contains("expected ok"), "{err}");
    }

    #[test]
    fn a_refused_row_that_opens_or_fails_otherwise_fails() {
        let row = invalid_udp_send();
        assert!(check_outcome(&row, "python", &Outcome::Opened).is_err());
        let other = Outcome::Refused(BindingErrorKind::UdpIo);
        assert!(check_outcome(&row, "python", &other).is_err());
        assert!(check_outcome_code(&row, "c", None).is_err());
        let url = Outcome::Refused(BindingErrorKind::UdpUrl);
        assert!(check_outcome(&row, "python", &url).is_ok());
    }

    #[test]
    fn an_open_entry_sets_only_its_legs_outcomes() {
        let mut row = valid_udp_send();
        row.open = Some(BTreeMap::from([("jvm".to_string(), "IO ok".to_string())]));
        let io = Outcome::Refused(BindingErrorKind::UdpIo);
        assert_eq!(row.outcomes("jvm"), vec![io, Outcome::Opened]);
        assert!(check_outcome(&row, "jvm", &io).is_ok());
        assert!(check_outcome(&row, "python", &io).is_err());
        let code = BindingErrorKind::UdpIo.c_projection();
        assert!(check_outcome_code(&row, "c", Some(code)).is_err());
    }

    #[test]
    fn an_outcome_word_is_ok_or_a_kind_of_the_family() {
        assert_eq!(Outcome::parse("tcp", "ok"), Ok(Outcome::Opened));
        assert_eq!(
            Outcome::parse("tcp", "TLS"),
            Ok(Outcome::Refused(BindingErrorKind::TcpTls))
        );
        assert_eq!(
            Outcome::parse("srt", "CONFIG_INVALID"),
            Ok(Outcome::Refused(BindingErrorKind::ConfigInvalid))
        );
        assert!(Outcome::parse("tcp", "refused").is_err());
        assert!(Outcome::parse("udp", "TLS").is_err());
    }

    #[test]
    fn resolve_fills_the_placeholders_in_the_url_and_the_record() {
        let row = Row {
            url: "tcps://127.0.0.1:{port}?cert={dir}/server.crt".into(),
            expect: Record {
                port: Some(Port::Placeholder),
                keys: Some(BTreeMap::from([(
                    "cert".to_string(),
                    "{dir}/server.crt".to_string(),
                )])),
                ..Record::default()
            },
            ..valid_udp_send()
        };
        assert!(row.needs_port() && row.needs_dir());
        let r = row.resolve(40000, "/x");
        assert_eq!(r.url, "tcps://127.0.0.1:40000?cert=/x/server.crt");
        assert_eq!(r.expect.port, Some(Port::Number(40000)));
        assert_eq!(r.expect.keys.unwrap()["cert"], "/x/server.crt");
        assert!(!valid_udp_send().needs_port());
    }

    #[test]
    fn the_shell_schemes_table_is_the_rust_list() {
        let rust: BTreeMap<String, String> = tst_pipeline::binding::SHELL_SCHEMES
            .iter()
            .map(|(s, d)| (s.to_string(), format!("{d:?}")))
            .collect();
        assert_eq!(load().shell_schemes, rust);
    }

    #[test]
    fn a_port_is_a_number_or_the_placeholder() {
        #[derive(Deserialize)]
        struct P {
            port: Port,
        }
        let p = |t: &str| toml::from_str::<P>(t).map(|p| p.port);
        assert_eq!(p("port = 5").unwrap(), Port::Number(5));
        assert_eq!(p("port = \"{port}\"").unwrap(), Port::Placeholder);
        assert!(p("port = \"5\"").is_err());
    }
}
