//! PMT shape classes, read from the committed catalog
//! `tests/coverage/corpus-shapes.toml`.

use std::sync::OnceLock;

use tst_test_helpers::rawts::ParsedStream;

/// The shape of a capture's PMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeClass {
    A,
    B,
    C,
    Other,
}

impl ShapeClass {
    pub fn key(&self) -> &'static str {
        match self {
            ShapeClass::A => "a",
            ShapeClass::B => "b",
            ShapeClass::C => "c",
            ShapeClass::Other => "other",
        }
    }

    fn from_key(key: &str) -> Option<ShapeClass> {
        match key {
            "a" => Some(ShapeClass::A),
            "b" => Some(ShapeClass::B),
            "c" => Some(ShapeClass::C),
            _ => None,
        }
    }
}

/// One `[[shape]]` table of the catalog.
#[derive(Debug, Clone, Default)]
pub struct ShapeRule {
    pub key: String,
    pub requires_stream_types: Vec<u8>,
    pub requires_klva: bool,
    pub forbids_stream_types: Vec<u8>,
}

const CATALOG: &str = include_str!("../../../../tests/coverage/corpus-shapes.toml");

/// The catalog rules in file order, parsed once.
pub fn rules() -> &'static [ShapeRule] {
    static RULES: OnceLock<Vec<ShapeRule>> = OnceLock::new();
    RULES.get_or_init(|| parse(CATALOG))
}

fn parse(text: &str) -> Vec<ShapeRule> {
    let mut out: Vec<ShapeRule> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let n = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[shape]]" {
            out.push(ShapeRule::default());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            panic!("corpus-shapes.toml line {n}: expected `key = value`, got {line:?}");
        };
        let (key, value) = (key.trim(), value.trim());
        let Some(rule) = out.last_mut() else {
            panic!("corpus-shapes.toml line {n}: `{key} = ...` outside a [[shape]] table");
        };
        match key {
            "key" => {
                let k = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or_else(|| {
                        panic!("corpus-shapes.toml line {n}: `key` must be a quoted string")
                    });
                if ShapeClass::from_key(k).is_none() {
                    panic!("corpus-shapes.toml line {n}: unknown shape key {k:?}");
                }
                rule.key = k.to_string();
            }
            "requires_stream_types" => rule.requires_stream_types = byte_list(value, n),
            "forbids_stream_types" => rule.forbids_stream_types = byte_list(value, n),
            "requires_klva" => {
                rule.requires_klva = match value {
                    "true" => true,
                    "false" => false,
                    _ => {
                        panic!("corpus-shapes.toml line {n}: `requires_klva` must be true or false")
                    }
                }
            }
            _ => panic!("corpus-shapes.toml line {n}: unknown key {key:?}"),
        }
    }
    for rule in &out {
        assert!(
            !rule.key.is_empty(),
            "corpus-shapes.toml: a [[shape]] has no key"
        );
    }
    out
}

fn byte_list(value: &str, n: usize) -> Vec<u8> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or_else(|| panic!("corpus-shapes.toml line {n}: expected a [..] list"));
    inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => u8::from_str_radix(h, 16),
                None => s.parse(),
            };
            parsed.unwrap_or_else(|_| {
                panic!("corpus-shapes.toml line {n}: {s:?} is not a byte value")
            })
        })
        .collect()
}

/// The first catalog rule the stream satisfies; `Other` when none does.
pub fn classify(parsed: &ParsedStream) -> ShapeClass {
    let has = |t: u8| parsed.streams.iter().any(|s| s.stream_type == t);
    let klva = parsed.streams.iter().any(|s| s.klva);
    for rule in rules() {
        if rule.requires_stream_types.iter().all(|&t| has(t))
            && (!rule.requires_klva || klva)
            && !rule.forbids_stream_types.iter().any(|&t| has(t))
        {
            return ShapeClass::from_key(&rule.key).unwrap_or(ShapeClass::Other);
        }
    }
    ShapeClass::Other
}
