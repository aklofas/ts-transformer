//! The URL query-key vocabulary, one characterization table per scheme.
//!
//! Every row is `(url, Expect)`: either the effective value of the keys the
//! row names, or the exact `Display` text of the rejection. The rows are the
//! contract. A parser change must leave each of them green; only the
//! per-scheme adapter functions at the bottom of each section may change when
//! the parsers are restructured.
//!
//! No row opens a socket: hosts are documentation-range IP literals
//! (`192.0.2.0/24`, `2001:db8::/32`) and the side-rule rows only reach
//! entry points that reject before they bind.

use std::fmt::Display;
use std::time::Duration;

use tst_hls::HlsUrl;
use tst_rist::{RistRecvTransportBuilder, RistTransportBuilder, RistUrl};
use tst_rtp::{RtpRecvTransport, RtpTransport, RtpUrl, RtspUrl};
use tst_srt::url::SrtUrl;
use tst_tcp::url::TcpUrl;
use tst_udp::{
    builder::{UdpRecvTransportBuilder, UdpTransportBuilder},
    url::UdpUrl,
};

/// What a row expects.
enum Expect {
    /// The URL parses; each `(key, rendered value)` must appear in the
    /// adapter's output. Keys the row does not name are not checked.
    Accept(&'static [(&'static str, &'static str)]),
    /// The URL is refused with exactly this `Display` text.
    Reject(&'static str),
}
use Expect::{Accept as A, Reject as R};

type Row = (&'static str, Expect);
type Effective = Result<Vec<(&'static str, String)>, String>;

/// Run every row of `table` through `adapter`; panic naming every row that
/// does not match.
fn run(scheme: &str, table: &[Row], adapter: fn(&str) -> Effective) {
    let mut bad = Vec::new();
    for (url, expect) in table {
        let got = adapter(url);
        let ok = match (expect, &got) {
            (A(want), Ok(have)) => want
                .iter()
                .all(|(k, v)| have.iter().any(|(hk, hv)| hk == k && hv == v)),
            (R(want), Err(have)) => want == have,
            _ => false,
        };
        if !ok {
            bad.push(format!("  {scheme} row {url}\n    got {got:?}"));
        }
        if std::env::var_os("URL_VOCABULARY_FILL").is_some() {
            if let (R("?"), Err(have)) = (expect, &got) {
                println!("FILL\t{url}\t{have}");
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} {scheme} rows differ:\n{}",
        bad.len(),
        table.len(),
        bad.join("\n")
    );
}

fn show<T: Display>(v: &Option<T>) -> String {
    v.as_ref().map_or_else(|| "none".into(), |x| x.to_string())
}
fn ms(v: Option<Duration>) -> String {
    v.map_or_else(|| "none".into(), |d| d.as_millis().to_string())
}
fn secs(v: Option<Duration>) -> String {
    v.map_or_else(|| "none".into(), |d| d.as_secs().to_string())
}
fn side<T, E: Display>(r: Result<T, E>) -> Effective {
    r.map(|_| Vec::new()).map_err(|e| e.to_string())
}

// ───────────────────────────── udp ─────────────────────────────

const UDP_PARSE: &[Row] = &[
    (
        "udp://192.0.2.10:5000",
        A(&[
            ("addr", "192.0.2.10"),
            ("port", "5000"),
            ("recv_bind", "false"),
            ("iface", "none"),
            ("ttl", "none"),
            ("tos", "none"),
            ("rcvbuf", "none"),
            ("sndbuf", "none"),
            ("pkt_size", "none"),
            ("localaddr", "none"),
        ]),
    ),
    ("udp://@192.0.2.10:5000", A(&[("recv_bind", "true")])),
    ("udp://[2001:db8::1]:5000", A(&[("addr", "2001:db8::1")])),
    ("udp://192.0.2.10:5000?iface=eth0", A(&[("iface", "eth0")])),
    (
        "udp://192.0.2.10:5000?iface=192.0.2.77",
        A(&[("iface", "192.0.2.77")]),
    ),
    ("udp://192.0.2.10:5000?ttl=7", A(&[("ttl", "7")])),
    ("udp://192.0.2.10:5000?ttl=0", A(&[("ttl", "0")])),
    ("udp://192.0.2.10:5000?ttl=255", A(&[("ttl", "255")])),
    (
        "udp://192.0.2.10:5000?ttl=256",
        R("query param 'ttl' has invalid value '256': number too large to fit in target type"),
    ),
    (
        "udp://192.0.2.10:5000?ttl=abc",
        R("query param 'ttl' has invalid value 'abc': invalid digit found in string"),
    ),
    (
        "udp://192.0.2.10:5000?ttl=0x10",
        R("query param 'ttl' has invalid value '0x10': invalid digit found in string"),
    ),
    ("udp://192.0.2.10:5000?ttl=2&ttl=3", A(&[("ttl", "3")])),
    ("udp://192.0.2.10:5000?tos=46", A(&[("tos", "46")])),
    ("udp://192.0.2.10:5000?tos=0x2e", A(&[("tos", "46")])),
    ("udp://192.0.2.10:5000?tos=0X10", A(&[("tos", "16")])),
    (
        "udp://192.0.2.10:5000?tos=256",
        R("query param 'tos' has invalid value '256': number too large to fit in target type"),
    ),
    (
        "udp://192.0.2.10:5000?tos=0xzz",
        R("query param 'tos' has invalid value '0xzz': invalid digit found in string"),
    ),
    ("udp://192.0.2.10:5000?rcvbuf=512", A(&[("rcvbuf", "512")])),
    (
        "udp://192.0.2.10:5000?rcvbuf=64K",
        A(&[("rcvbuf", "65536")]),
    ),
    (
        "udp://192.0.2.10:5000?rcvbuf=64k",
        A(&[("rcvbuf", "65536")]),
    ),
    (
        "udp://192.0.2.10:5000?rcvbuf=8M",
        A(&[("rcvbuf", "8388608")]),
    ),
    (
        "udp://192.0.2.10:5000?rcvbuf=256M",
        A(&[("rcvbuf", "268435456")]),
    ),
    (
        "udp://192.0.2.10:5000?rcvbuf=257M",
        R(
            "query param 'rcvbuf' has invalid value '257M': byte size 269484032 exceeds maximum 268435456",
        ),
    ),
    (
        "udp://192.0.2.10:5000?rcvbuf=abc",
        R("query param 'rcvbuf' has invalid value 'abc': invalid digit found in string"),
    ),
    (
        "udp://192.0.2.10:5000?sndbuf=2M",
        A(&[("sndbuf", "2097152")]),
    ),
    (
        "udp://192.0.2.10:5000?sndbuf=257M",
        R(
            "query param 'sndbuf' has invalid value '257M': byte size 269484032 exceeds maximum 268435456",
        ),
    ),
    (
        "udp://192.0.2.10:5000?sndbuf=-1",
        R("query param 'sndbuf' has invalid value '-1': invalid digit found in string"),
    ),
    (
        "udp://192.0.2.10:5000?pkt_size=1316",
        A(&[("pkt_size", "1316")]),
    ),
    (
        "udp://192.0.2.10:5000?pkt_size=1K",
        A(&[("pkt_size", "1024")]),
    ),
    (
        "udp://192.0.2.10:5000?pkt_size=big",
        R("query param 'pkt_size' has invalid value 'big': invalid digit found in string"),
    ),
    (
        "udp://192.0.2.10:5000?localaddr=192.0.2.99",
        A(&[("localaddr", "192.0.2.99")]),
    ),
    (
        "udp://[2001:db8::1]:5000?localaddr=2001:db8::2",
        A(&[("localaddr", "2001:db8::2")]),
    ),
    (
        "udp://192.0.2.10:5000?localaddr=nothost",
        R("query param 'localaddr' has invalid value 'nothost': invalid IP address syntax"),
    ),
    (
        "udp://192.0.2.10:5000?localaddr=2001:db8::2",
        R("peer address 192.0.2.10 and ?localaddr=2001:db8::2 are different IP families"),
    ),
    (
        "udp://[2001:db8::1]:5000?localaddr=192.0.2.99",
        R("peer address 2001:db8::1 and ?localaddr=192.0.2.99 are different IP families"),
    ),
    (
        "udp://192.0.2.10:5000?unknown=1",
        A(&[("addr", "192.0.2.10")]),
    ),
    ("udp://192.0.2.10:5000?bogus=1&ttl=9", A(&[("ttl", "9")])),
    ("udp://192.0.2.10", R("URL must include a port")),
    (
        "tcp://192.0.2.10:5000",
        R("URL scheme must be 'udp', got 'tcp'"),
    ),
    (
        "srt://192.0.2.10:5000",
        R("URL scheme must be 'udp', got 'srt'"),
    ),
    (
        "not a url",
        R("URL parse failed: URL missing '://' separator"),
    ),
];

/// Entry-point rules between the parsed URL and the builders; these refuse
/// before any socket is opened.
const UDP_SEND: &[Row] = &[
    (
        "udp://@192.0.2.10:5000",
        R("URL has '@' prefix → use UdpRecvTransportBuilder instead"),
    ),
    ("udp://192.0.2.10:5000?pkt_size=1316", A(&[])),
];
const UDP_RECV: &[Row] = &[
    (
        "udp://@192.0.2.10:5000?pkt_size=1316",
        R(
            "pkt_size is a send-side knob; receive buffers size to the transport's deliverable ceiling automatically — remove ?pkt_size= from receiver URLs",
        ),
    ),
    ("udp://@192.0.2.10:5000?rcvbuf=1M", A(&[])),
];

fn effective_udp(url: &str) -> Effective {
    let u = UdpUrl::parse(url).map_err(|e| e.to_string())?;
    Ok(vec![
        ("addr", u.addr.to_string()),
        ("port", u.port.to_string()),
        ("recv_bind", u.recv_bind.to_string()),
        ("iface", show(&u.config.iface)),
        ("ttl", show(&u.config.ttl)),
        ("tos", show(&u.config.tos)),
        ("rcvbuf", show(&u.config.rcvbuf)),
        ("sndbuf", show(&u.config.sndbuf)),
        ("pkt_size", show(&u.config.pkt_size)),
        ("localaddr", show(&u.config.localaddr)),
    ])
}
fn effective_udp_send(url: &str) -> Effective {
    side(UdpTransportBuilder::from_url(url).map(|_| ()))
}
fn effective_udp_recv(url: &str) -> Effective {
    side(UdpRecvTransportBuilder::from_url(url).map(|_| ()))
}

#[test]
fn udp_keys() {
    run("udp", UDP_PARSE, effective_udp);
}
#[test]
fn udp_send_entry_rules() {
    run("udp-send", UDP_SEND, effective_udp_send);
}
#[test]
fn udp_recv_entry_rules() {
    run("udp-recv", UDP_RECV, effective_udp_recv);
}

// ───────────────────────────── tcp / tcps ─────────────────────────────

const TCP_PARSE: &[Row] = &[
    (
        "tcp://192.0.2.10:9000",
        A(&[
            ("host", "192.0.2.10"),
            ("port", "9000"),
            ("tls", "false"),
            ("listen", "false"),
            ("nodelay", "none"),
            ("keepalive", "none"),
            ("rcvbuf", "none"),
            ("sndbuf", "none"),
            ("connect_timeout", "none"),
            ("cert", "none"),
            ("key", "none"),
            ("ca", "none"),
        ]),
    ),
    ("tcps://192.0.2.10:9000", A(&[("tls", "true")])),
    ("tcp://relay.invalid:9000", A(&[("host", "relay.invalid")])),
    ("tcp://[2001:db8::1]:9000", A(&[("host", "2001:db8::1")])),
    ("tcp://192.0.2.10:9000?listen=1", A(&[("listen", "true")])),
    (
        "tcp://192.0.2.10:9000?listen=true",
        A(&[("listen", "true")]),
    ),
    ("tcp://192.0.2.10:9000?listen=yes", A(&[("listen", "true")])),
    ("tcp://192.0.2.10:9000?listen=on", A(&[("listen", "true")])),
    ("tcp://192.0.2.10:9000?listen=0", A(&[("listen", "false")])),
    (
        "tcp://192.0.2.10:9000?listen=off",
        A(&[("listen", "false")]),
    ),
    ("tcp://192.0.2.10:9000?listen=no", A(&[("listen", "false")])),
    (
        "tcp://192.0.2.10:9000?listen=false",
        A(&[("listen", "false")]),
    ),
    (
        "tcp://192.0.2.10:9000?listen=maybe",
        R(
            "query param 'listen' has invalid value 'maybe': expected one of: 1/0/true/false/yes/no/on/off",
        ),
    ),
    (
        "tcp://192.0.2.10:9000?listen=1&listen=0",
        A(&[("listen", "false")]),
    ),
    (
        "tcp://relay.invalid:9000?listen=1",
        R("listener host 'relay.invalid' must be a literal IPv4/IPv6 address"),
    ),
    (
        "tcp://[::]:9000?listen=1",
        A(&[("host", "::"), ("listen", "true")]),
    ),
    (
        "tcp://192.0.2.10:9000?nodelay=0",
        A(&[("nodelay", "false")]),
    ),
    ("tcp://192.0.2.10:9000?nodelay=1", A(&[("nodelay", "true")])),
    (
        "tcp://192.0.2.10:9000?nodelay=2",
        R(
            "query param 'nodelay' has invalid value '2': expected one of: 1/0/true/false/yes/no/on/off",
        ),
    ),
    (
        "tcp://192.0.2.10:9000?keepalive=30",
        A(&[("keepalive", "30")]),
    ),
    (
        "tcp://192.0.2.10:9000?keepalive=-1",
        R("query param 'keepalive' has invalid value '-1': invalid digit found in string"),
    ),
    (
        "tcp://192.0.2.10:9000?keepalive=1.5",
        R("query param 'keepalive' has invalid value '1.5': invalid digit found in string"),
    ),
    (
        "tcp://192.0.2.10:9000?rcvbuf=4M",
        A(&[("rcvbuf", "4194304")]),
    ),
    (
        "tcp://192.0.2.10:9000?rcvbuf=300M",
        R(
            "query param 'rcvbuf' has invalid value '300M': byte size 314572800 exceeds maximum 268435456",
        ),
    ),
    (
        "tcp://192.0.2.10:9000?sndbuf=64K",
        A(&[("sndbuf", "65536")]),
    ),
    (
        "tcp://192.0.2.10:9000?sndbuf=zz",
        R("query param 'sndbuf' has invalid value 'zz': invalid digit found in string"),
    ),
    (
        "tcp://192.0.2.10:9000?connect_timeout=5",
        A(&[("connect_timeout", "5")]),
    ),
    (
        "tcp://192.0.2.10:9000?connect_timeout=5&connect_timeout=8",
        A(&[("connect_timeout", "8")]),
    ),
    (
        "tcp://192.0.2.10:9000?connect_timeout=soon",
        R("query param 'connect_timeout' has invalid value 'soon': invalid digit found in string"),
    ),
    (
        "tcps://192.0.2.10:9000?listen=1&cert=/tmp/s.crt&key=/tmp/s.key",
        A(&[("cert", "/tmp/s.crt"), ("key", "/tmp/s.key")]),
    ),
    (
        "tcps://192.0.2.10:9000?ca=/tmp/ca.pem",
        A(&[("ca", "/tmp/ca.pem")]),
    ),
    (
        "tcp://192.0.2.10:9000?unknown=1",
        A(&[("host", "192.0.2.10")]),
    ),
    (
        "tcp://192.0.2.10:9000?pkt_size=8K",
        A(&[("pkt_size", "8192")]),
    ),
    (
        "tcp://192.0.2.10:9000?pkt_size=big",
        R("query param 'pkt_size' has invalid value 'big': invalid digit found in string"),
    ),
    ("tcp://192.0.2.10", R("URL must include a port")),
    (
        "udp://192.0.2.10:9000",
        R("URL scheme must be 'tcp' or 'tcps', got 'udp'"),
    ),
];

/// Whether the URL's query names `key`; the parsed config alone cannot say
/// for the keys whose config default is not "none".
fn query_names(url: &str, key: &str) -> bool {
    url.split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|p| p.split('=').next() == Some(key)))
}

fn show_path(v: &Option<std::path::PathBuf>) -> String {
    v.as_ref()
        .map_or_else(|| "none".into(), |p| p.display().to_string())
}

fn effective_tcp(url: &str) -> Effective {
    let u = TcpUrl::parse(url).map_err(|e| e.to_string())?;
    let c = &u.config;
    Ok(vec![
        ("host", u.host.clone()),
        ("port", u.port.to_string()),
        ("tls", u.tls.to_string()),
        ("listen", u.listen.to_string()),
        (
            "nodelay",
            if query_names(url, "nodelay") {
                show(&c.nodelay)
            } else {
                "none".into()
            },
        ),
        ("keepalive", secs(c.keepalive)),
        ("rcvbuf", show(&c.rcvbuf)),
        ("sndbuf", show(&c.sndbuf)),
        ("pkt_size", show(&c.pkt_size)),
        ("connect_timeout", secs(c.connect_timeout)),
        ("cert", show_path(&c.cert)),
        ("key", show_path(&c.key)),
        ("ca", show_path(&c.ca)),
    ])
}

#[test]
fn tcp_keys() {
    run("tcp", TCP_PARSE, effective_tcp);
}

// ───────────────────────────── rist ─────────────────────────────

const RIST_PARSE: &[Row] = &[
    (
        "rist://192.0.2.10:8000",
        A(&[
            ("addr", "192.0.2.10"),
            ("port", "8000"),
            ("recv_bind", "false"),
            ("profile", "none"),
            ("bandwidth", "none"),
            ("buffer", "none"),
            ("aes-type", "none"),
            ("secret", "none"),
            ("cname", "none"),
            ("recovery_maxbitrate", "none"),
            ("session_timeout", "none"),
            ("compression", "none"),
        ]),
    ),
    ("rist://@192.0.2.10:8000", A(&[("recv_bind", "true")])),
    ("rist://[2001:db8::1]:8000", A(&[("addr", "2001:db8::1")])),
    (
        "rist://relay.invalid:8000",
        R("host 'relay.invalid' is not a literal IPv4/IPv6 address"),
    ),
    (
        "rist://192.0.2.10:8000?profile=simple",
        A(&[("profile", "Simple")]),
    ),
    (
        "rist://192.0.2.10:8000?profile=main",
        A(&[("profile", "Main")]),
    ),
    (
        "rist://192.0.2.10:8000?profile=advanced",
        R("query param 'profile' has invalid value 'advanced': expected one of: simple, main"),
    ),
    (
        "rist://192.0.2.10:8000?profile=main&profile=simple",
        A(&[("profile", "Simple")]),
    ),
    (
        "rist://192.0.2.10:8000?bandwidth=1000",
        A(&[("bandwidth", "1000")]),
    ),
    (
        "rist://192.0.2.10:8000?bandwidth=fast",
        R("query param 'bandwidth' has invalid value 'fast': invalid digit found in string"),
    ),
    (
        "rist://192.0.2.10:8000?recovery_maxbitrate=2000",
        A(&[("recovery_maxbitrate", "2000")]),
    ),
    (
        "rist://192.0.2.10:8000?recovery-maxbitrate=2500",
        A(&[("recovery_maxbitrate", "2500")]),
    ),
    (
        "rist://192.0.2.10:8000?recovery_maxbitrate=-5",
        R(
            "query param 'recovery_maxbitrate' has invalid value '-5': invalid digit found in string",
        ),
    ),
    (
        "rist://192.0.2.10:8000?bandwidth=3000&recovery_maxbitrate=3000",
        A(&[("bandwidth", "3000"), ("recovery_maxbitrate", "3000")]),
    ),
    (
        "rist://192.0.2.10:8000?bandwidth=3000&recovery_maxbitrate=4000",
        R(
            "query param 'bandwidth' has invalid value '3000': conflicts with recovery_maxbitrate=4000; `bandwidth` is an alias of `recovery_maxbitrate` (librist recovery_maxbitrate) — give one, or equal values",
        ),
    ),
    (
        "rist://192.0.2.10:8000?recovery-maxbitrate=4000&bandwidth=3000",
        R(
            "query param 'bandwidth' has invalid value '3000': conflicts with recovery_maxbitrate=4000; `bandwidth` is an alias of `recovery_maxbitrate` (librist recovery_maxbitrate) — give one, or equal values",
        ),
    ),
    ("rist://192.0.2.10:8000?buffer=200", A(&[("buffer", "200")])),
    (
        "rist://192.0.2.10:8000?buffer=x",
        R("query param 'buffer' has invalid value 'x': invalid digit found in string"),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=128&secret=hunter2hunter2",
        A(&[("aes-type", "128"), ("secret", "hunter2hunter2")]),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=192&secret=hunter2hunter2",
        A(&[("aes-type", "192")]),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=256&secret=hunter2hunter2",
        A(&[("aes-type", "256")]),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=100&secret=hunter2hunter2",
        R("aes-type must be 128, 192, or 256; got 100"),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=bits&secret=hunter2hunter2",
        R("query param 'aes-type' has invalid value 'bits': invalid digit found in string"),
    ),
    (
        "rist://192.0.2.10:8000?aes-type=128",
        R(
            "aes-type given without secret; aes-type only selects the key size for a secret (a secret without aes-type defaults to AES-256)",
        ),
    ),
    (
        "rist://192.0.2.10:8000?secret=hunter2hunter2",
        A(&[("aes-type", "256"), ("secret", "hunter2hunter2")]),
    ),
    (
        "rist://192.0.2.10:8000?secret=",
        R("query param 'secret' has invalid value '': must be non-empty"),
    ),
    (
        "rist://192.0.2.10:8000?secret=a&secret=b",
        A(&[("secret", "b")]),
    ),
    (
        "rist://192.0.2.10:8000?cname=uav-12",
        A(&[("cname", "uav-12")]),
    ),
    (
        "rist://192.0.2.10:8000?session_timeout=5000",
        A(&[("session_timeout", "5000")]),
    ),
    (
        "rist://192.0.2.10:8000?session-timeout=6000",
        A(&[("session_timeout", "6000")]),
    ),
    (
        "rist://192.0.2.10:8000?session_timeout=never",
        R("query param 'session_timeout' has invalid value 'never': invalid digit found in string"),
    ),
    (
        "rist://192.0.2.10:8000?compression=1",
        A(&[("compression", "true")]),
    ),
    (
        "rist://192.0.2.10:8000?compression=on",
        A(&[("compression", "true")]),
    ),
    (
        "rist://192.0.2.10:8000?compression=off",
        A(&[("compression", "false")]),
    ),
    (
        "rist://192.0.2.10:8000?compression=sometimes",
        R(
            "query param 'compression' has invalid value 'sometimes': expected one of: 1/0/true/false/yes/no/on/off",
        ),
    ),
    (
        "rist://192.0.2.10:8000?unknown=1",
        A(&[("addr", "192.0.2.10")]),
    ),
    (
        "rist://192.0.2.10:8000?pkt_size=1K",
        A(&[("pkt_size", "1024")]),
    ),
    (
        "rist://192.0.2.10:8000?pkt_size=big",
        R("query param 'pkt_size' has invalid value 'big': invalid digit found in string"),
    ),
    ("rist://192.0.2.10", R("URL must include a port")),
    (
        "udp://192.0.2.10:8000",
        R("URL scheme must be 'rist', got 'udp'"),
    ),
    (
        "rist://192.0.2.10:8000?recovery-maxbitrate=-5",
        R(
            "query param 'recovery-maxbitrate' has invalid value '-5': invalid digit found in string",
        ),
    ),
    (
        "rist://192.0.2.10:8000?session-timeout=never",
        R("query param 'session-timeout' has invalid value 'never': invalid digit found in string"),
    ),
];

const RIST_SEND: &[Row] = &[
    (
        "rist://@192.0.2.10:8000",
        R("invalid configuration: URL has '@' prefix — use RistRecvTransportBuilder"),
    ),
    ("rist://192.0.2.10:8000", A(&[])),
];
const RIST_RECV: &[Row] = &[
    (
        "rist://192.0.2.10:8000",
        R("invalid configuration: URL missing '@' prefix — use rist://@host:port for receivers"),
    ),
    ("rist://@192.0.2.10:8000", A(&[])),
];

fn effective_rist(url: &str) -> Effective {
    let u = RistUrl::parse(url).map_err(|e| e.to_string())?;
    let c = &u.config;
    let given = |key: &str, v: String| {
        if query_names(url, key) {
            v
        } else {
            "none".into()
        }
    };
    Ok(vec![
        ("addr", u.addr.to_string()),
        ("port", u.port.to_string()),
        ("recv_bind", u.is_recv_bind.to_string()),
        ("profile", given("profile", format!("{:?}", c.profile))),
        ("bandwidth", show(&c.bandwidth_kbps)),
        ("buffer", given("buffer", c.buffer.as_millis().to_string())),
        (
            "aes-type",
            c.encryption
                .as_ref()
                .map_or("none".into(), |k| k.size_bits.to_string()),
        ),
        (
            "secret",
            c.encryption
                .as_ref()
                .map_or("none".into(), |k| k.secret.expose().to_string()),
        ),
        ("cname", show(&c.cname)),
        ("recovery_maxbitrate", show(&c.recovery_maxbitrate_kbps)),
        ("session_timeout", ms(c.session_timeout)),
        (
            "compression",
            given("compression", c.compression.to_string()),
        ),
        ("pkt_size", given("pkt_size", c.pkt_size.to_string())),
    ])
}
fn effective_rist_send(url: &str) -> Effective {
    side(RistTransportBuilder::new(url).map(|_| ()))
}
fn effective_rist_recv(url: &str) -> Effective {
    side(RistRecvTransportBuilder::new(url).map(|_| ()))
}

#[test]
fn rist_keys() {
    run("rist", RIST_PARSE, effective_rist);
}
#[test]
fn rist_send_entry_rule() {
    run("rist-send", RIST_SEND, effective_rist_send);
}
#[test]
fn rist_recv_entry_rule() {
    run("rist-recv", RIST_RECV, effective_rist_recv);
}

// ───────────────────────────── hls / hlss ─────────────────────────────

const HLS_PARSE: &[Row] = &[
    (
        "hls://192.0.2.10:8080",
        A(&[
            ("addr", "192.0.2.10"),
            ("port", "8080"),
            ("tls", "false"),
            ("output_dir", "none"),
            ("segment_duration", "none"),
            ("playlist_window", "none"),
            ("mode", "none"),
            ("auth_user", "none"),
            ("auth_pass", "none"),
            ("cert", "none"),
            ("key", "none"),
        ]),
    ),
    ("hlss://192.0.2.10:8443", A(&[("tls", "true")])),
    ("hls://[2001:db8::1]:8080", A(&[("addr", "2001:db8::1")])),
    (
        "hls://relay.invalid:8080",
        R("host 'relay.invalid' is not a literal IPv4/IPv6 address"),
    ),
    (
        "hls://192.0.2.10:8080?output_dir=/tmp/seg",
        A(&[("output_dir", "/tmp/seg")]),
    ),
    (
        "hls://192.0.2.10:8080?output_dir=/a&output_dir=/b",
        A(&[("output_dir", "/b")]),
    ),
    (
        "hls://192.0.2.10:8080?segment_duration=4",
        A(&[("segment_duration", "4")]),
    ),
    (
        "hls://192.0.2.10:8080?segment_duration=2.5",
        R("query param 'segment_duration' has invalid value '2.5': invalid digit found in string"),
    ),
    (
        "hls://192.0.2.10:8080?playlist_window=6",
        A(&[("playlist_window", "6")]),
    ),
    (
        "hls://192.0.2.10:8080?playlist_window=-1",
        R("query param 'playlist_window' has invalid value '-1': invalid digit found in string"),
    ),
    ("hls://192.0.2.10:8080?mode=live", A(&[("mode", "Live")])),
    ("hls://192.0.2.10:8080?mode=event", A(&[("mode", "Event")])),
    ("hls://192.0.2.10:8080?mode=vod", A(&[("mode", "Vod")])),
    (
        "hls://192.0.2.10:8080?mode=garbage",
        R("query param 'mode' has invalid value 'garbage': expected one of: live, event, vod"),
    ),
    (
        "hls://192.0.2.10:8080?mode=LIVE",
        R("query param 'mode' has invalid value 'LIVE': expected one of: live, event, vod"),
    ),
    (
        "hls://192.0.2.10:8080?auth_user=alice",
        A(&[("auth_user", "alice")]),
    ),
    (
        "hls://192.0.2.10:8080?auth_pass=s3cret",
        A(&[("auth_pass", "s3cret")]),
    ),
    (
        "hlss://192.0.2.10:8443?cert=/tmp/s.crt&key=/tmp/s.key",
        A(&[("cert", "/tmp/s.crt"), ("key", "/tmp/s.key")]),
    ),
    (
        "hls://192.0.2.10:8080?unknown=1",
        A(&[("addr", "192.0.2.10")]),
    ),
    ("hls://192.0.2.10", R("URL must include a port")),
    (
        "http://192.0.2.10:8080",
        R("URL scheme must be 'hls' or 'hlss', got 'http'"),
    ),
];

/// Whether the URL's query names `key`; the parsed config alone cannot say,
/// because the HLS config holds a default for every key.
fn hls_named(url: &str, key: &str) -> bool {
    url.split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|p| p.split('=').next() == Some(key)))
}

fn effective_hls(url: &str) -> Effective {
    let u = HlsUrl::parse(url).map_err(|e| e.to_string())?;
    let c = &u.config;
    let given = |key: &str, v: String| {
        if hls_named(url, key) {
            v
        } else {
            "none".into()
        }
    };
    // A credential given alone is dropped by the config (no auth is
    // configured), so a lone one is read back from the query text.
    let lone = |key: &str| {
        url.split_once('?')
            .and_then(|(_, q)| {
                q.split('&')
                    .filter_map(|p| p.split_once('='))
                    .rfind(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            })
            .unwrap_or_else(|| "none".into())
    };
    let (auth_user, auth_pass) = match &c.basic_auth {
        Some((user, pass)) => (user.clone(), pass.clone()),
        None => (lone("auth_user"), lone("auth_pass")),
    };
    Ok(vec![
        ("addr", c.bind.ip().to_string()),
        ("port", c.bind.port().to_string()),
        ("tls", c.tls.to_string()),
        (
            "output_dir",
            given("output_dir", c.output_dir.display().to_string()),
        ),
        (
            "segment_duration",
            given("segment_duration", c.segment_duration.as_secs().to_string()),
        ),
        (
            "playlist_window",
            given("playlist_window", c.playlist_window.to_string()),
        ),
        ("mode", given("mode", format!("{:?}", c.mode))),
        ("auth_user", auth_user),
        ("auth_pass", auth_pass),
        (
            "cert",
            given(
                "cert",
                c.tls_cert
                    .as_ref()
                    .map_or_else(String::new, |p| p.display().to_string()),
            ),
        ),
        (
            "key",
            given(
                "key",
                c.tls_key
                    .as_ref()
                    .map_or_else(String::new, |p| p.display().to_string()),
            ),
        ),
    ])
}

#[test]
fn hls_keys() {
    run("hls", HLS_PARSE, effective_hls);
}

// ───────────────────────────── srt ─────────────────────────────

const SRT_PARSE: &[Row] = &[
    (
        "srt://192.0.2.10:9000",
        A(&[
            ("host", "192.0.2.10"),
            ("port", "9000"),
            ("mode", "Caller"),
            ("passphrase", "none"),
            ("pbkeylen", "Aes128"),
            ("latency", "none"),
            ("rcvlatency", "none"),
            ("peerlatency", "none"),
            ("mss", "none"),
            ("payloadsize", "none"),
            ("maxbw", "None"),
            ("inputbw", "none"),
            ("oheadbw", "none"),
            ("streamid", "none"),
            ("lossmaxttl", "none"),
            ("tlpktdrop", "None"),
            ("fc", "none"),
            ("packetfilter", "none"),
            ("congestion", "None"),
            ("x-recvtimeout", "none"),
            ("x-sendtimeout", "none"),
            ("conntimeo", "none"),
            ("linger", "none"),
            ("udprcvbuf", "none"),
            ("udpsndbuf", "none"),
        ]),
    ),
    ("srt://[2001:db8::1]:9000", A(&[("host", "2001:db8::1")])),
    (
        "srt://camera.invalid:9000",
        A(&[("host", "camera.invalid")]),
    ),
    (
        "srt://:7000?mode=listener",
        A(&[("host", ""), ("mode", "Listener")]),
    ),
    (
        "srt://192.0.2.10:9000?mode=listener",
        A(&[("mode", "Listener")]),
    ),
    (
        "srt://192.0.2.10:9000?mode=caller",
        A(&[("mode", "Caller")]),
    ),
    (
        "srt://192.0.2.10:9000?mode=rendezvous",
        R("unsupported mode 'rendezvous'; only mode=caller and mode=listener are accepted"),
    ),
    ("srt://:7000", R("URL must include host")),
    (
        "srt://192.0.2.10",
        R("URL must include port (e.g. srt://host:9000), got no port"),
    ),
    (
        "srt://user:pw@192.0.2.10:9000",
        R("userinfo (user:pass@) is not supported in SRT URLs; use ?passphrase=... instead"),
    ),
    (
        "srt://user@192.0.2.10:9000",
        R("userinfo (user:pass@) is not supported in SRT URLs; use ?passphrase=... instead"),
    ),
    (
        "udp://192.0.2.10:9000",
        R("scheme must be 'srt', got 'udp'"),
    ),
    (
        "srt://192.0.2.10:9000?congestion=live",
        A(&[("congestion", "Some(Live)")]),
    ),
    (
        "srt://192.0.2.10:9000?congestion=file",
        A(&[("congestion", "Some(File)")]),
    ),
    (
        "srt://192.0.2.10:9000?smoother=file",
        A(&[("congestion", "Some(File)")]),
    ),
    (
        "srt://192.0.2.10:9000?congestion=bbr",
        R(
            "option validation failed for 'congestion': option value out of range: congestion must be 'live' or 'file' (lowercase), got 'bbr'",
        ),
    ),
    (
        "srt://192.0.2.10:9000?conntimeo=1500",
        A(&[("conntimeo", "1500")]),
    ),
    (
        "srt://192.0.2.10:9000?connect_timeout=2500",
        A(&[("conntimeo", "2500")]),
    ),
    (
        "srt://192.0.2.10:9000?conntimeo=-1",
        R("invalid value for 'conntimeo': must be non-negative, got -1"),
    ),
    (
        "srt://192.0.2.10:9000?conntimeo=soon",
        R(
            "invalid value for 'conntimeo': expected i32 decimal, got 'soon': invalid digit found in string",
        ),
    ),
    ("srt://192.0.2.10:9000?fc=25600", A(&[("fc", "25600")])),
    ("srt://192.0.2.10:9000?ffs=12800", A(&[("fc", "12800")])),
    (
        "srt://192.0.2.10:9000?fc=-1",
        R(
            "invalid value for 'fc': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?inputbw=1000000",
        A(&[("inputbw", "1000000")]),
    ),
    (
        "srt://192.0.2.10:9000?inputbw=x",
        R(
            "invalid value for 'inputbw': expected non-negative decimal integer in range, got 'x': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?latency=200",
        A(&[("latency", "200")]),
    ),
    (
        "srt://192.0.2.10:9000?tsbpddelay=300",
        A(&[("latency", "300")]),
    ),
    (
        "srt://192.0.2.10:9000?latency=100&latency=200",
        A(&[("latency", "200")]),
    ),
    (
        "srt://192.0.2.10:9000?latency=-1",
        R("invalid value for 'latency': must be non-negative, got -1"),
    ),
    (
        "srt://192.0.2.10:9000?latency=2147483648",
        R(
            "invalid value for 'latency': expected i32 decimal, got '2147483648': number too large to fit in target type",
        ),
    ),
    (
        "srt://192.0.2.10:9000?latency=12000",
        A(&[("latency", "12000")]),
    ),
    (
        "srt://192.0.2.10:9000?peerlatency=120",
        A(&[("peerlatency", "120")]),
    ),
    (
        "srt://192.0.2.10:9000?peerlatency=x",
        R(
            "invalid value for 'peerlatency': expected i32 decimal, got 'x': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?rcvlatency=130",
        A(&[("rcvlatency", "130")]),
    ),
    (
        "srt://192.0.2.10:9000?rcvlatency=-3",
        R("invalid value for 'rcvlatency': must be non-negative, got -3"),
    ),
    ("srt://192.0.2.10:9000?linger=5", A(&[("linger", "5")])),
    (
        "srt://192.0.2.10:9000?linger=-1",
        R("invalid value for 'linger': must be non-negative, got -1"),
    ),
    (
        "srt://192.0.2.10:9000?lossmaxttl=50",
        A(&[("lossmaxttl", "50")]),
    ),
    (
        "srt://192.0.2.10:9000?lossmaxttl=-1",
        R(
            "invalid value for 'lossmaxttl': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?maxbw=0",
        A(&[("maxbw", "Some(Limited(0))")]),
    ),
    (
        "srt://192.0.2.10:9000?maxbw=125000000",
        A(&[("maxbw", "Some(Limited(125000000))")]),
    ),
    (
        "srt://192.0.2.10:9000?maxbw=-1",
        R(
            "invalid value for 'maxbw': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
    ("srt://192.0.2.10:9000?mss=1500", A(&[("mss", "1500")])),
    (
        "srt://192.0.2.10:9000?mss=70000",
        R(
            "invalid value for 'mss': expected non-negative decimal integer in range, got '70000': number too large to fit in target type",
        ),
    ),
    ("srt://192.0.2.10:9000?oheadbw=5", A(&[("oheadbw", "5")])),
    (
        "srt://192.0.2.10:9000?oheadbw=100",
        A(&[("oheadbw", "100")]),
    ),
    (
        "srt://192.0.2.10:9000?oheadbw=4",
        R("invalid value for 'oheadbw': overhead_bandwidth_pct must be 5..=100, got 4"),
    ),
    (
        "srt://192.0.2.10:9000?oheadbw=101",
        R("invalid value for 'oheadbw': overhead_bandwidth_pct must be 5..=100, got 101"),
    ),
    (
        "srt://192.0.2.10:9000?oheadbw=-5",
        R(
            "invalid value for 'oheadbw': expected non-negative decimal integer in range, got '-5': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?packetfilter=fec,cols:10,rows:5",
        A(&[("packetfilter", "fec,cols:10,rows:5")]),
    ),
    (
        "srt://192.0.2.10:9000?packetfilter=bad%20spec",
        R(
            "option validation failed for 'packetfilter': invalid packet filter: filter spec contains invalid characters",
        ),
    ),
    (
        "srt://192.0.2.10:9000?passphrase=0123456789abc",
        A(&[("passphrase", "0123456789abc")]),
    ),
    (
        "srt://192.0.2.10:9000?passphrase=short",
        R(
            "option validation failed for 'passphrase': invalid passphrase: passphrase length must be 10-79 chars (got 5)",
        ),
    ),
    (
        "srt://192.0.2.10:9000?pbkeylen=16",
        A(&[("pbkeylen", "Aes128")]),
    ),
    (
        "srt://192.0.2.10:9000?pbkeylen=24",
        A(&[("pbkeylen", "Aes192")]),
    ),
    (
        "srt://192.0.2.10:9000?pbkeylen=32",
        A(&[("pbkeylen", "Aes256")]),
    ),
    (
        "srt://192.0.2.10:9000?pbkeylen=20",
        R(
            "option validation failed for 'pbkeylen': option value out of range: pbkeylen must be 16, 24, or 32, got 20",
        ),
    ),
    (
        "srt://192.0.2.10:9000?payloadsize=1316",
        A(&[("payloadsize", "1316")]),
    ),
    (
        "srt://192.0.2.10:9000?pkt_size=1000",
        A(&[("payloadsize", "1000")]),
    ),
    (
        "srt://192.0.2.10:9000?payload_size=900",
        A(&[("payloadsize", "900")]),
    ),
    (
        "srt://192.0.2.10:9000?payloadsize=1456",
        A(&[("payloadsize", "1456")]),
    ),
    (
        "srt://192.0.2.10:9000?payloadsize=1457",
        R(
            "invalid value for 'payloadsize': must be <= 1456 (libsrt SRT_LIVE_MAX_PLSIZE, live-mode cap), got 1457",
        ),
    ),
    (
        "srt://192.0.2.10:9000?payloadsize=70000",
        R(
            "invalid value for 'payloadsize': expected non-negative decimal integer in range, got '70000': number too large to fit in target type",
        ),
    ),
    (
        "srt://192.0.2.10:9000?streamid=front",
        A(&[("streamid", "front")]),
    ),
    (
        "srt://192.0.2.10:9000?srt_streamid=rear",
        A(&[("streamid", "rear")]),
    ),
    (
        "srt://192.0.2.10:9000?streamid=%E2%82%AC",
        R(
            "option validation failed for 'streamid': invalid stream id: stream ID contains non-ASCII characters",
        ),
    ),
    (
        "srt://192.0.2.10:9000?tlpktdrop=0",
        A(&[("tlpktdrop", "Some(false)")]),
    ),
    (
        "srt://192.0.2.10:9000?tlpktdrop=1",
        A(&[("tlpktdrop", "Some(true)")]),
    ),
    (
        "srt://192.0.2.10:9000?tlpktdrop=true",
        R("invalid value for 'tlpktdrop': expected '0' or '1', got 'true'"),
    ),
    (
        "srt://192.0.2.10:9000?tlpktdrop=2",
        R("invalid value for 'tlpktdrop': expected '0' or '1', got '2'"),
    ),
    (
        "srt://192.0.2.10:9000?udprcvbuf=8388608",
        A(&[("udprcvbuf", "8388608")]),
    ),
    (
        "srt://192.0.2.10:9000?recv_buffer_size=4194304",
        A(&[("udprcvbuf", "4194304")]),
    ),
    (
        "srt://192.0.2.10:9000?udprcvbuf=8M",
        R(
            "invalid value for 'udprcvbuf': expected non-negative decimal integer in range, got '8M': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?udpsndbuf=2097152",
        A(&[("udpsndbuf", "2097152")]),
    ),
    (
        "srt://192.0.2.10:9000?send_buffer_size=1048576",
        A(&[("udpsndbuf", "1048576")]),
    ),
    (
        "srt://192.0.2.10:9000?udpsndbuf=-1",
        R(
            "invalid value for 'udpsndbuf': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?x-recvtimeout=1000",
        A(&[("x-recvtimeout", "1000")]),
    ),
    (
        "srt://192.0.2.10:9000?x-recvtimeout=-1",
        R("invalid value for 'x-recvtimeout': must be non-negative, got -1"),
    ),
    (
        "srt://192.0.2.10:9000?x-sendtimeout=2000",
        A(&[("x-sendtimeout", "2000")]),
    ),
    (
        "srt://192.0.2.10:9000?x-sendtimeout=x",
        R(
            "invalid value for 'x-sendtimeout': expected i32 decimal, got 'x': invalid digit found in string",
        ),
    ),
    // libsrt keys this library recognises but does not expose.
    (
        "srt://192.0.2.10:9000?bindtodevice=eth0",
        R(
            "option 'bindtodevice' (libsrt SRTO_BINDTODEVICE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?cryptomode=1",
        R(
            "option 'cryptomode' (libsrt SRTO_CRYPTOMODE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?drifttracer=1",
        R(
            "option 'drifttracer' (libsrt SRTO_DRIFTTRACER) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?enforcedencryption=1",
        R(
            "option 'enforcedencryption' (libsrt SRTO_ENFORCEDENCRYPTION) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?groupconnect=1",
        R(
            "option 'groupconnect' (libsrt SRTO_GROUPCONNECT) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?groupminstabletimeo=1",
        R(
            "option 'groupminstabletimeo' (libsrt SRTO_GROUPMINSTABLETIMEO) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?iptos=1",
        R(
            "option 'iptos' (libsrt SRTO_IPTOS) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?ipttl=1",
        R(
            "option 'ipttl' (libsrt SRTO_IPTTL) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?ipv6only=1",
        R(
            "option 'ipv6only' (libsrt SRTO_IPV6ONLY) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?kmpreannounce=1",
        R(
            "option 'kmpreannounce' (libsrt SRTO_KMPREANNOUNCE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?kmrefreshrate=1",
        R(
            "option 'kmrefreshrate' (libsrt SRTO_KMREFRESHRATE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?maxrexmitbw=1",
        R(
            "option 'maxrexmitbw' (libsrt SRTO_MAXREXMITBW) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?messageapi=1",
        R(
            "option 'messageapi' (libsrt SRTO_MESSAGEAPI) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?mininputbw=1",
        R(
            "option 'mininputbw' (libsrt SRTO_MININPUTBW) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?minversion=1",
        R(
            "option 'minversion' (libsrt SRTO_MINVERSION) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?nakreport=1",
        R(
            "option 'nakreport' (libsrt SRTO_NAKREPORT) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?peeridletimeo=1",
        R(
            "option 'peeridletimeo' (libsrt SRTO_PEERIDLETIMEO) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?rcvbuf=1",
        R(
            "option 'rcvbuf' (libsrt SRTO_RCVBUF) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?retransmitalgo=1",
        R(
            "option 'retransmitalgo' (libsrt SRTO_RETRANSMITALGO) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?sndbuf=1",
        R(
            "option 'sndbuf' (libsrt SRTO_SNDBUF) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?snddropdelay=1",
        R(
            "option 'snddropdelay' (libsrt SRTO_SNDDROPDELAY) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?transtype=live",
        R(
            "option 'transtype' (libsrt SRTO_TRANSTYPE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?tsbpdmode=1",
        R(
            "option 'tsbpdmode' (libsrt SRTO_TSBPDMODE) is recognized but not yet exposed by this library; see deferred-features.md",
        ),
    ),
    // ffmpeg-only aliases.
    (
        "srt://192.0.2.10:9000?timeout=1000",
        R(
            "ffmpeg URL alias 'timeout' (ffmpeg's read/write timeout, libsrt rw_timeout, microseconds) is not exposed by this library; use x-recvtimeout / x-sendtimeout (milliseconds) for the closest equivalent",
        ),
    ),
    (
        "srt://192.0.2.10:9000?listen_timeout=1000",
        R(
            "ffmpeg URL alias 'listen_timeout' (ffmpeg's listen_timeout (connection-awaiting timeout, microseconds)) is not exposed by this library; not exposed by this library; see deferred-features.md",
        ),
    ),
    (
        "srt://192.0.2.10:9000?tsbpd=1",
        R(
            "ffmpeg URL alias 'tsbpd' (ffmpeg short alias for SRTO_TSBPDMODE (also reachable via the longer 'tsbpdmode' libsrt URL key)) is not exposed by this library; not exposed by this library; see deferred-features.md",
        ),
    ),
    // Unknown keys are refused.
    (
        "srt://192.0.2.10:9000?bogus=1",
        R("unknown URL key 'bogus'"),
    ),
    ("srt://192.0.2.10:9000?ttl=5", R("unknown URL key 'ttl'")),
    (
        "srt://192.0.2.10:9000?smoother=bbr",
        R(
            "option validation failed for 'congestion': option value out of range: congestion must be 'live' or 'file' (lowercase), got 'bbr'",
        ),
    ),
    (
        "srt://192.0.2.10:9000?connect_timeout=soon",
        R(
            "invalid value for 'conntimeo': expected i32 decimal, got 'soon': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?ffs=-1",
        R(
            "invalid value for 'fc': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?tsbpddelay=-1",
        R("invalid value for 'latency': must be non-negative, got -1"),
    ),
    (
        "srt://192.0.2.10:9000?pkt_size=1457",
        R(
            "invalid value for 'payloadsize': must be <= 1456 (libsrt SRT_LIVE_MAX_PLSIZE, live-mode cap), got 1457",
        ),
    ),
    (
        "srt://192.0.2.10:9000?payload_size=1457",
        R(
            "invalid value for 'payloadsize': must be <= 1456 (libsrt SRT_LIVE_MAX_PLSIZE, live-mode cap), got 1457",
        ),
    ),
    (
        "srt://192.0.2.10:9000?srt_streamid=%E2%82%AC",
        R(
            "option validation failed for 'streamid': invalid stream id: stream ID contains non-ASCII characters",
        ),
    ),
    (
        "srt://192.0.2.10:9000?recv_buffer_size=8M",
        R(
            "invalid value for 'udprcvbuf': expected non-negative decimal integer in range, got '8M': invalid digit found in string",
        ),
    ),
    (
        "srt://192.0.2.10:9000?send_buffer_size=-1",
        R(
            "invalid value for 'udpsndbuf': expected non-negative decimal integer in range, got '-1': invalid digit found in string",
        ),
    ),
];

fn effective_srt(url: &str) -> Effective {
    let u = SrtUrl::parse(url).map_err(|e| e.to_string())?;
    let mut cfg = tst_srt::config::SrtSocketConfig::default();
    cfg.merge_from(&u.config);
    Ok(vec![
        ("host", u.host.clone()),
        ("port", u.port.to_string()),
        ("mode", format!("{:?}", u.mode)),
        (
            "passphrase",
            cfg.passphrase
                .as_ref()
                .map_or("none".into(), |p| p.as_str().to_string()),
        ),
        (
            "pbkeylen",
            format!("{:?}", cfg.key_length.unwrap_or_default()),
        ),
        ("latency", ms(cfg.latency)),
        ("rcvlatency", ms(cfg.recv_latency)),
        ("peerlatency", ms(cfg.peer_latency)),
        ("mss", show(&cfg.mss)),
        ("payloadsize", show(&cfg.payload_size)),
        ("maxbw", format!("{:?}", cfg.max_bandwidth)),
        ("inputbw", show(&cfg.input_bandwidth)),
        ("oheadbw", show(&cfg.overhead_bandwidth_pct)),
        (
            "streamid",
            cfg.stream_id
                .as_ref()
                .map_or("none".into(), |s| s.as_str().to_string()),
        ),
        ("lossmaxttl", show(&cfg.loss_max_ttl)),
        ("tlpktdrop", format!("{:?}", cfg.too_late_packet_drop)),
        ("fc", show(&cfg.flow_window_packets)),
        (
            "packetfilter",
            cfg.packet_filter
                .as_ref()
                .map_or("none".into(), |p| p.as_str().to_string()),
        ),
        ("congestion", format!("{:?}", cfg.congestion)),
        ("x-recvtimeout", ms(cfg.recv_timeout)),
        ("x-sendtimeout", ms(cfg.send_timeout)),
        ("conntimeo", ms(cfg.connect_timeout)),
        ("linger", secs(cfg.linger)),
        ("udprcvbuf", show(&cfg.udp_recv_buffer_bytes)),
        ("udpsndbuf", show(&cfg.udp_send_buffer_bytes)),
    ])
}

#[test]
fn srt_keys() {
    run("srt", SRT_PARSE, effective_srt);
}

// ───────────────────────────── rtp ─────────────────────────────

const RTP_PARSE: &[Row] = &[
    (
        "rtp://192.0.2.10:5004",
        A(&[
            ("host", "192.0.2.10"),
            ("port", "5004"),
            ("ttl", "none"),
            ("iface", "none"),
            ("pkt_size", "none"),
            ("ssrc", "none"),
            ("pt", "none"),
            ("recv_timeout", "none"),
            ("rcvbuf", "none"),
            ("sndbuf", "none"),
            ("source", "none"),
        ]),
    ),
    ("rtp://[2001:db8::1]:5004", A(&[("host", "2001:db8::1")])),
    (
        "rtp://camera.invalid:5004",
        A(&[("host", "camera.invalid")]),
    ),
    ("rtp://192.0.2.10:5004?ttl=1", A(&[("ttl", "1")])),
    ("rtp://192.0.2.10:5004?ttl=255", A(&[("ttl", "255")])),
    (
        "rtp://192.0.2.10:5004?ttl=0",
        R("bad value for query parameter ttl: 0 (must be in 1..=255)"),
    ),
    (
        "rtp://192.0.2.10:5004?ttl=256",
        R("bad value for query parameter ttl: 256 (must be in 1..=255)"),
    ),
    (
        "rtp://192.0.2.10:5004?ttl=x",
        R("bad value for query parameter ttl: x (invalid digit found in string)"),
    ),
    ("rtp://192.0.2.10:5004?ttl=2&ttl=3", A(&[("ttl", "3")])),
    ("rtp://192.0.2.10:5004?iface=eth0", A(&[("iface", "eth0")])),
    (
        "rtp://192.0.2.10:5004?iface=192.0.2.50",
        A(&[("iface", "192.0.2.50")]),
    ),
    (
        "rtp://192.0.2.10:5004?iface=",
        R("bad value for query parameter iface: "),
    ),
    (
        "rtp://192.0.2.10:5004?iface=a%20b",
        R("bad value for query parameter iface: a b"),
    ),
    (
        "rtp://192.0.2.10:5004?pkt_size=1316",
        A(&[("pkt_size", "1316")]),
    ),
    (
        "rtp://192.0.2.10:5004?pkt_size=188",
        A(&[("pkt_size", "188")]),
    ),
    (
        "rtp://192.0.2.10:5004?pkt_size=0",
        R("bad value for query parameter pkt_size: 0 (must be a positive multiple of 188)"),
    ),
    (
        "rtp://192.0.2.10:5004?pkt_size=1000",
        R("bad value for query parameter pkt_size: 1000 (must be a positive multiple of 188)"),
    ),
    (
        "rtp://192.0.2.10:5004?pkt_size=1K",
        R("bad value for query parameter pkt_size: 1K (invalid digit found in string)"),
    ),
    ("rtp://192.0.2.10:5004?ssrc=12345", A(&[("ssrc", "12345")])),
    (
        "rtp://192.0.2.10:5004?ssrc=0xdeadbeef",
        A(&[("ssrc", "3735928559")]),
    ),
    ("rtp://192.0.2.10:5004?ssrc=0XFF", A(&[("ssrc", "255")])),
    (
        "rtp://192.0.2.10:5004?ssrc=4294967296",
        R(
            "bad value for query parameter ssrc: 4294967296 (number too large to fit in target type)",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?ssrc=zz",
        R("bad value for query parameter ssrc: zz (invalid digit found in string)"),
    ),
    ("rtp://192.0.2.10:5004?pt=96", A(&[("pt", "96")])),
    ("rtp://192.0.2.10:5004?pt=127", A(&[("pt", "127")])),
    (
        "rtp://192.0.2.10:5004?pt=33",
        R(
            "bad value for query parameter pt: 33 (payload type 33 is MPEG-TS — use the MP2T receiver (no ?pt=))",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?pt=0",
        R(
            "bad value for query parameter pt: 0 (must be in 1..=127 (excluding 33, which is MPEG-TS))",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?pt=128",
        R(
            "bad value for query parameter pt: 128 (must be in 1..=127 (excluding 33, which is MPEG-TS))",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?pt=x",
        R("bad value for query parameter pt: x (invalid digit found in string)"),
    ),
    (
        "rtp://192.0.2.10:5004?recv_timeout=500",
        A(&[("recv_timeout", "500")]),
    ),
    (
        "rtp://192.0.2.10:5004?recv_timeout=0",
        R("bad value for query parameter recv_timeout: 0 (must be nonzero)"),
    ),
    (
        "rtp://192.0.2.10:5004?recv_timeout=soon",
        R("bad value for query parameter recv_timeout: soon (invalid digit found in string)"),
    ),
    (
        "rtp://192.0.2.10:5004?rcvbuf=8M",
        A(&[("rcvbuf", "8388608")]),
    ),
    (
        "rtp://192.0.2.10:5004?rcvbuf=512K",
        A(&[("rcvbuf", "524288")]),
    ),
    (
        "rtp://192.0.2.10:5004?rcvbuf=300M",
        R("bad value for query parameter rcvbuf: 300M"),
    ),
    (
        "rtp://192.0.2.10:5004?rcvbuf=zz",
        R("bad value for query parameter rcvbuf: zz"),
    ),
    (
        "rtp://192.0.2.10:5004?sndbuf=2M",
        A(&[("sndbuf", "2097152")]),
    ),
    (
        "rtp://192.0.2.10:5004?sndbuf=zz",
        R("bad value for query parameter sndbuf: zz"),
    ),
    (
        "rtp://192.0.2.10:5004?source=192.0.2.77",
        A(&[("source", "192.0.2.77")]),
    ),
    (
        "rtp://192.0.2.10:5004?source=2001:db8::7",
        A(&[("source", "2001:db8::7")]),
    ),
    (
        "rtp://192.0.2.10:5004?source=sender.invalid",
        R("bad value for query parameter source: sender.invalid"),
    ),
    (
        "rtp://192.0.2.10:5004?bogus=1",
        R("unknown query parameter: bogus"),
    ),
    (
        "rtp://192.0.2.10:5004?localaddr=192.0.2.1",
        R("unknown query parameter: localaddr"),
    ),
    ("rtp://192.0.2.10", R("rtp:// URL requires :port")),
    ("rtsp://192.0.2.10:5004", R("unsupported scheme: rtsp")),
    ("udp://192.0.2.10:5004", R("unsupported scheme: udp")),
];

/// Send-side rules, refused before the socket is bound.
const RTP_SEND: &[Row] = &[
    (
        "rtp://192.0.2.10:5004?pt=96",
        R("URL has ?pt= (elementary RTP): use H264Receiver, not the MP2T transport"),
    ),
    (
        "rtp://192.0.2.10:5004?rcvbuf=1M",
        R(
            "URL parse failed: rcvbuf is a receive-side knob — remove ?rcvbuf= from sender URLs (use ?sndbuf=)",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?source=192.0.2.77",
        R(
            "URL parse failed: source is a receive-side filter (accept datagrams only from that IP) — remove ?source= from sender URLs",
        ),
    ),
];
/// Receive-side rules, refused before the socket is bound.
const RTP_RECV: &[Row] = &[
    (
        "rtp://192.0.2.10:5004?pkt_size=1316",
        R(
            "URL parse failed: pkt_size is a send-side knob; receive buffers size to the transport's deliverable ceiling automatically — remove ?pkt_size= from receiver URLs",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?sndbuf=1M",
        R(
            "URL parse failed: sndbuf is a send-side knob — remove ?sndbuf= from receiver URLs (use ?rcvbuf=)",
        ),
    ),
    (
        "rtp://192.0.2.10:5004?pt=96",
        R("URL has ?pt= (elementary RTP): use H264Receiver, not the MP2T transport"),
    ),
];

fn effective_rtp(url: &str) -> Effective {
    let u = RtpUrl::parse(url).map_err(|e| e.to_string())?;
    Ok(vec![
        ("host", u.host.clone()),
        ("port", u.port.to_string()),
        ("ttl", show(&u.ttl)),
        ("iface", show(&u.iface)),
        ("pkt_size", show(&u.pkt_size)),
        ("ssrc", show(&u.ssrc)),
        ("pt", show(&u.pt)),
        ("recv_timeout", ms(u.recv_timeout)),
        ("rcvbuf", show(&u.rcvbuf)),
        ("sndbuf", show(&u.sndbuf)),
        ("source", show(&u.source)),
    ])
}
fn effective_rtp_send(url: &str) -> Effective {
    let u = RtpUrl::parse(url).map_err(|e| e.to_string())?;
    side(RtpTransport::connect_with(&u).map(|_| ()))
}
fn effective_rtp_recv(url: &str) -> Effective {
    let u = RtpUrl::parse(url).map_err(|e| e.to_string())?;
    side(RtpRecvTransport::listen_with(&u).map(|_| ()))
}

#[test]
fn rtp_keys() {
    run("rtp", RTP_PARSE, effective_rtp);
}
#[test]
fn rtp_send_entry_rules() {
    run("rtp-send", RTP_SEND, effective_rtp_send);
}
#[test]
fn rtp_recv_entry_rules() {
    run("rtp-recv", RTP_RECV, effective_rtp_recv);
}

// ───────────────────────────── rtsp / rtsps ─────────────────────────────

const RTSP_PARSE: &[Row] = &[
    (
        "rtsp://192.0.2.10/live",
        A(&[
            ("scheme", "Rtsp"),
            ("host", "192.0.2.10"),
            ("port", "554"),
            ("path", "/live"),
            ("username", "none"),
            ("password", "none"),
            ("transport", "PreferUdp"),
            ("rtsp_version", "V1_0"),
            ("tcp_keepalive", "none"),
            ("recv_timeout", "none"),
        ]),
    ),
    (
        "rtsps://192.0.2.10/live",
        A(&[("scheme", "Rtsps"), ("port", "322")]),
    ),
    (
        "rtsp://192.0.2.10:8554/a/b",
        A(&[("port", "8554"), ("path", "/a/b")]),
    ),
    ("rtsp://192.0.2.10", A(&[("path", "")])),
    (
        "rtsp://[2001:db8::1]:554/live",
        A(&[("host", "2001:db8::1")]),
    ),
    ("rtsp://cam.invalid/live", A(&[("host", "cam.invalid")])),
    (
        "rtsp://user:p%40ss@192.0.2.10/live",
        A(&[("username", "user"), ("password", "p@ss")]),
    ),
    (
        "rtsp://user@192.0.2.10/live",
        A(&[("username", "user"), ("password", "none")]),
    ),
    (
        "rtsp://192.0.2.10/live?transport=tcp",
        A(&[("transport", "ForceTcp")]),
    ),
    (
        "rtsp://192.0.2.10/live?transport=udp",
        A(&[("transport", "ForceUdp")]),
    ),
    (
        "rtsp://192.0.2.10/live?transport=quic",
        R("bad value for query parameter transport: quic"),
    ),
    (
        "rtsp://192.0.2.10/live?transport=TCP",
        R("bad value for query parameter transport: TCP"),
    ),
    (
        "rtsp://192.0.2.10/live?transport=udp&transport=tcp",
        A(&[("transport", "ForceTcp")]),
    ),
    (
        "rtsp://192.0.2.10/live?rtsp_version=1.0",
        A(&[("rtsp_version", "V1_0")]),
    ),
    (
        "rtsp://192.0.2.10/live?rtsp_version=2.0",
        A(&[("rtsp_version", "V2_0")]),
    ),
    (
        "rtsp://192.0.2.10/live?rtsp_version=3.0",
        R("bad value for query parameter rtsp_version: 3.0"),
    ),
    (
        "rtsp://192.0.2.10/live?tcp_keepalive=30",
        A(&[("tcp_keepalive", "30")]),
    ),
    (
        "rtsp://192.0.2.10/live?tcp_keepalive=-1",
        R("bad value for query parameter tcp_keepalive: -1"),
    ),
    (
        "rtsp://192.0.2.10/live?tcp_keepalive=often",
        R("bad value for query parameter tcp_keepalive: often"),
    ),
    (
        "rtsp://192.0.2.10/live?recv_timeout=500",
        A(&[("recv_timeout", "500")]),
    ),
    (
        "rtsp://192.0.2.10/live?recv_timeout=0",
        R("bad value for query parameter recv_timeout: 0 (must be nonzero)"),
    ),
    (
        "rtsp://192.0.2.10/live?recv_timeout=soon",
        R("bad value for query parameter recv_timeout: soon (invalid digit found in string)"),
    ),
    (
        "rtsp://192.0.2.10/live?recv_timeout=100&recv_timeout=200",
        A(&[("recv_timeout", "200")]),
    ),
    (
        "rtsp://192.0.2.10/live?ttl=5",
        R("unknown query parameter: ttl"),
    ),
    (
        "rtsp://192.0.2.10/live?bogus=1",
        R("unknown query parameter: bogus"),
    ),
    ("rtp://192.0.2.10:554/live", R("unsupported scheme: rtp")),
    ("http://192.0.2.10/live", R("unsupported scheme: http")),
];

fn effective_rtsp(url: &str) -> Effective {
    use secrecy::ExposeSecret;
    let u = RtspUrl::parse(url).map_err(|e| e.to_string())?;
    Ok(vec![
        ("scheme", format!("{:?}", u.scheme())),
        ("host", u.host.clone()),
        ("port", u.port.to_string()),
        ("path", u.path.clone()),
        ("username", show(&u.username)),
        (
            "password",
            u.password
                .as_ref()
                .map_or("none".into(), |p| p.expose_secret().to_string()),
        ),
        ("transport", format!("{:?}", u.transport_preference)),
        ("rtsp_version", format!("{:?}", u.rtsp_version)),
        ("tcp_keepalive", secs(u.tcp_keepalive)),
        ("recv_timeout", ms(u.recv_timeout)),
    ])
}

#[test]
fn rtsp_keys() {
    run("rtsp", RTSP_PARSE, effective_rtsp);
}
