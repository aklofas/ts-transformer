//! Cross-binding golden-envelope types and comparison semantics.
//!
//! # Comparison rules
//!
//! - `core` events are compared in canonical order (the order stored in the
//!   `Golden`).  Ordering is deterministic because tst-core's mux + demux is
//!   deterministic.
//! - `Error` events are compared by **stable public code** (e.g.
//!   `"STRICT_REJECTION"`), never by Rust-internal kind or free text.
//! - An unknown `event` tag encountered during JSON deserialization causes a
//!   hard parse error (no `#[serde(other)]` catch-all, deliberately).  An
//!   older consumer encountering a `CoreEvent` variant it doesn't recognise
//!   will fail loudly, never skip silently.
//! - `roundtrip` scenarios carry no media events (`core: []`); the whole-stream
//!   byte-identity digest lives under `extensions.output_sha256`. The demux-path
//!   `payload_sha256` field (NAL payload hash) is never overloaded for this.
//! - Comparison between an observed `Golden` and a committed `Golden` is a
//!   simple struct `PartialEq`; the JSON round-trip (`serde_json`) preserves
//!   all fields, so `assert_eq!` catches every difference.

use serde::{Deserialize, Serialize};

/// Root golden envelope.
///
/// Written to `tests/fixtures/scenarios/<id>/golden.json` by `gen-scenarios`.
/// Deserialized and compared struct-equal by the Rust adapter tests.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct Golden {
    pub schema_version: u32, // 0 = experimental
    pub lossy: bool,         // per-binding normaliser admission
    pub core: Vec<CoreEvent>,
    #[serde(default)]
    pub extensions: serde_json::Value,
}

/// One entry in the canonical event sequence.
///
/// `#[serde(tag = "event", rename_all = "snake_case")]` gives each variant a
/// stable JSON tag string (`"video"`, `"audio"`, `"klv"`, `"unknown"`,
/// `"subtitle"`, `"error"`).  The tag values are the public stability surface;
/// do NOT rename variants here without a schema_version bump.
///
/// There is deliberately no `#[serde(other)]` and no `non_exhaustive` attribute
/// here: an unrecognised `event` tag causes a deserialisation error so that an
/// older consumer fails loudly when encountering an event kind it was not built
/// against.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CoreEvent {
    Video {
        program: u16,
        pid: u16,
        stream_type: String,
        pts: i64,
        key: bool,
        payload_sha256: String,
    },
    Audio {
        program: u16,
        pid: u16,
        stream_type: String,
        pts: i64,
        payload_sha256: String,
    },
    Klv {
        program: u16,
        pid: u16,
        stream_type: String,
        /// Which KLV set was detected (e.g. "sync_au_cell" / "async").
        set: String,
    },
    Unknown {
        pid: u16,
    },
    Subtitle {
        program: u16,
        pid: u16,
        stream_type: String,
        /// Subtitle codec tag: "dvb_subtitle" | "dvb_teletext" | "webvtt" | "cea708_standalone".
        codec: String,
    },
    Error {
        /// Stable public code, never Rust-internal kind or free text.
        /// Examples: "STRICT_REJECTION", "SYNC_BUF_EXHAUSTED".
        code: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_variant_roundtrips_through_json() {
        let g = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Subtitle {
                program: 1,
                pid: 0x1100,
                stream_type: "0x06".into(),
                codec: "dvb_subtitle".into(),
            }],
            extensions: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("\"event\":\"subtitle\""));
        let back: Golden = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }
}
