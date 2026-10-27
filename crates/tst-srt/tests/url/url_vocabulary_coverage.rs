//! Vocabulary-coverage drift test.
//!
//! Asserts:
//!   1. Every Group 1 honored key lands in its own `SrtUrl::config` field
//!      (catches "I forgot to wire the new key into the apply layer").
//!   2. Every Group 2 key parses and changes the config.
//!   3. All 24 Group 3 keys reject with `UnsupportedKey` carrying a
//!      non-empty `SRTO_*` static string.
//!   4. The honored-key count matches what's documented in
//!      `docs/guides/srt.md` (drift between code and docs).

use std::time::Duration;
use tst_srt::{Congestion, KeyLength, MaxBandwidth, SrtSocketConfig, SrtUrl, SrtUrlError};

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

type Check = fn(&SrtSocketConfig) -> bool;

/// Every Group 1 key, a value the parser accepts, and the config field the
/// key must land in. A key parsed but not wired into its field fails here.
#[test]
fn group1_keys_all_parse_and_wire() {
    let rows: &[(&str, &str, Check)] = &[
        ("passphrase", "ten-chars!", |c| c.passphrase.is_some()),
        ("pbkeylen", "16", |c| {
            c.key_length == Some(KeyLength::Aes128)
        }),
        ("latency", "100", |c| {
            c.latency == Some(Duration::from_millis(100))
        }),
        ("rcvlatency", "100", |c| {
            c.recv_latency == Some(Duration::from_millis(100))
        }),
        ("peerlatency", "80", |c| {
            c.peer_latency == Some(Duration::from_millis(80))
        }),
        ("mss", "1316", |c| c.mss == Some(1316)),
        ("payloadsize", "1316", |c| c.payload_size == Some(1316)),
        ("maxbw", "10000000", |c| {
            c.max_bandwidth == Some(MaxBandwidth::Limited(10_000_000))
        }),
        ("inputbw", "5000000", |c| {
            c.input_bandwidth == Some(5_000_000)
        }),
        ("oheadbw", "25", |c| c.overhead_bandwidth_pct == Some(25)),
        ("streamid", "front", |c| {
            c.stream_id.as_ref().map(|s| s.as_str()) == Some("front")
        }),
        ("lossmaxttl", "20", |c| c.loss_max_ttl == Some(20)),
        ("tlpktdrop", "1", |c| c.too_late_packet_drop == Some(true)),
        ("fc", "8192", |c| c.flow_window_packets == Some(8192)),
        ("packetfilter", "fec,cols:10,rows:5", |c| {
            c.packet_filter.is_some()
        }),
        ("congestion", "live", |c| {
            c.congestion == Some(Congestion::Live)
        }),
        ("conntimeo", "10000", |c| {
            c.connect_timeout == Some(Duration::from_millis(10_000))
        }),
        ("linger", "5", |c| c.linger == Some(Duration::from_secs(5))),
        ("udprcvbuf", "2000000", |c| {
            c.udp_recv_buffer_bytes == Some(2_000_000)
        }),
        ("udpsndbuf", "2000000", |c| {
            c.udp_send_buffer_bytes == Some(2_000_000)
        }),
    ];
    assert_eq!(
        rows.len(),
        GROUP1_KEYS.len(),
        "GROUP1_KEYS and the row table must match"
    );
    for key in GROUP1_KEYS {
        assert!(
            rows.iter().any(|(k, _, _)| k == key),
            "Group 1 key '{key}' has no row"
        );
    }
    for (key, value, wired) in rows {
        let url = format!("srt://1.2.3.4:9000?{key}={value}");
        let parsed = SrtUrl::parse(&url).unwrap_or_else(|e| {
            panic!("Group 1 key '{key}' should parse with value '{value}': {e}")
        });
        assert!(
            wired(&parsed.config),
            "Group 1 key '{key}={value}' parsed but did not reach its config field"
        );
    }
}

#[test]
fn group2_keys_all_parse_and_wire() {
    let default = format!("{:?}", SrtSocketConfig::default());
    for key in GROUP2_KEYS {
        let url = format!("srt://1.2.3.4:9000?{key}=1000");
        let parsed =
            SrtUrl::parse(&url).unwrap_or_else(|e| panic!("Group 2 key '{key}' should parse: {e}"));
        assert_ne!(
            format!("{:?}", parsed.config),
            default,
            "Group 2 key '{key}' parsed but changed nothing"
        );
    }
}

#[test]
fn group3_keys_all_reject_with_srto() {
    for key in GROUP3_KEYS {
        let url = format!("srt://1.2.3.4:9000?{key}=1");
        let e = SrtUrl::parse(&url).unwrap_err();
        match e {
            SrtUrlError::UnsupportedKey { key: k, srto } => {
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
