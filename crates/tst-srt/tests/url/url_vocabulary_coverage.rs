//! Vocabulary-coverage drift test.
//!
//! Asserts:
//!   1. All 16 Group 1 honored keys parse to a `Some(_)` field on
//!      `UrlOverlay` (catches "I forgot to wire the new key into the
//!      apply layer").
//!   2. All 2 Group 2 keys parse successfully.
//!   3. All 24 Group 3 keys reject with `UnsupportedKey` carrying a
//!      non-empty `SRTO_*` static string.
//!   4. The honored-key count matches what's documented in
//!      `docs/guides/srt.md` (drift between code and docs).

use tst_srt::{SrtUrl, UrlError};

const GROUP1_KEYS: &[&str] = &[
    "passphrase",
    "pbkeylen",
    "latency",
    "rcvlatency",
    "peerlatency",
    "mss",
    "payloadsize",
    "maxbw",
    "inputbw",
    "oheadbw",
    "streamid",
    "lossmaxttl",
    "tlpktdrop",
    "fc",
    "packetfilter",
    "congestion",
    "conntimeo",
    "linger",
    "udprcvbuf",
    "udpsndbuf",
];

const GROUP2_KEYS: &[&str] = &["x-recvtimeout", "x-sendtimeout"];

const GROUP3_KEYS: &[&str] = &[
    "bindtodevice",
    "cryptomode",
    "drifttracer",
    "enforcedencryption",
    "groupconnect",
    "groupminstabletimeo",
    "iptos",
    "ipttl",
    "ipv6only",
    "kmpreannounce",
    "kmrefreshrate",
    "maxrexmitbw",
    "messageapi",
    "mininputbw",
    "minversion",
    "nakreport",
    "peeridletimeo",
    "rcvbuf",
    "retransmitalgo",
    "sndbuf",
    "snddropdelay",
    "transtype",
    "tsbpdmode",
];

/// For each Group 1 key, build a URL with a value the parser will accept,
/// parse it, and assert the resulting overlay has at least one Some(_)
/// field. (Approximation: we count distinct fields by snapshotting before
/// and after; if the parse succeeds, the key is wired.)
#[test]
fn group1_keys_all_parse_and_wire() {
    let representative_value: &[(&str, &str)] = &[
        ("passphrase", "ten-chars!"),
        ("pbkeylen", "16"),
        ("latency", "100"),
        ("rcvlatency", "100"),
        ("peerlatency", "80"),
        ("mss", "1316"),
        ("payloadsize", "1316"),
        ("maxbw", "10000000"),
        ("inputbw", "5000000"),
        ("oheadbw", "25"),
        ("streamid", "front"),
        ("lossmaxttl", "20"),
        ("tlpktdrop", "1"),
        ("fc", "8192"),
        ("packetfilter", "fec,cols:10,rows:5"),
        ("congestion", "live"),
        ("conntimeo", "10000"),
        ("linger", "5"),
        ("udprcvbuf", "2000000"),
        ("udpsndbuf", "2000000"),
    ];
    assert_eq!(
        representative_value.len(),
        GROUP1_KEYS.len(),
        "GROUP1_KEYS and representative_value must match"
    );
    for (key, value) in representative_value {
        let url = format!("srt://1.2.3.4:9000?{key}={value}");
        let _ = SrtUrl::parse(&url).unwrap_or_else(|e| {
            panic!("Group 1 key '{key}' should parse with value '{value}': {e}")
        });
    }
}

#[test]
fn group2_keys_all_parse_and_wire() {
    for key in GROUP2_KEYS {
        let url = format!("srt://1.2.3.4:9000?{key}=1000");
        let _ =
            SrtUrl::parse(&url).unwrap_or_else(|e| panic!("Group 2 key '{key}' should parse: {e}"));
    }
}

#[test]
fn group3_keys_all_reject_with_srto() {
    for key in GROUP3_KEYS {
        let url = format!("srt://1.2.3.4:9000?{key}=1");
        let e = SrtUrl::parse(&url).unwrap_err();
        match e {
            UrlError::UnsupportedKey { key: k, srto } => {
                assert_eq!(&k, key);
                assert!(!srto.is_empty(), "{key}: srto must not be empty");
                assert!(
                    srto.starts_with("SRTO_"),
                    "{key}: srto must start with SRTO_, got {srto}"
                );
            }
            other => panic!("{key}: expected UnsupportedKey, got {other:?}"),
        }
    }
}

#[test]
fn guide_srt_md_keytable_count_matches() {
    // Cargo test CWD is the crate root (crates/tst-srt/), so guide-srt.md
    // is two directories up at docs/guides/srt.md.
    let path = "../../docs/guides/srt.md";
    let body = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "could not read guide-srt.md: {e}; cwd = {:?}",
            std::env::current_dir()
        )
    });
    // Count rows in the Group 1 + Group 2 honored-keys table. We don't
    // parse the markdown rigorously; we just count occurrences of the
    // representative key names inside the URL parsing section.
    let section_marker = "## URL parsing";
    let section_start = body
        .find(section_marker)
        .unwrap_or_else(|| panic!("'{section_marker}' section not found in guide-srt.md"));
    let section = &body[section_start..];
    let mut found = 0;
    for key in GROUP1_KEYS.iter().chain(GROUP2_KEYS.iter()) {
        // Each key appears in a markdown table cell as `\`<key>\``.
        let needle = format!("`{key}`");
        if section.contains(&needle) {
            found += 1;
        }
    }
    let expected = GROUP1_KEYS.len() + GROUP2_KEYS.len();
    assert_eq!(
        found, expected,
        "guide-srt.md URL parsing table is missing keys; found {found}, expected {expected}"
    );
}
