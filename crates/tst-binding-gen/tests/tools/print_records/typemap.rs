//! The one type table: a Rust type as written in a `binding_record!`
//! declaration to its Python annotation, Java type and JNI signature.

use crate::schema::{Entry, camel, crossing, java_name, lookup};
use tst_core::binding_schema::{FieldMark, FieldSpec, RecordSchema};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sc {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
    /// A `usize` count: crosses to the host as an unsigned 64-bit value.
    Usize,
    /// A `Duration`: crosses to the host as whole microseconds, saturating.
    Duration,
}

/// A host-side type for a `@[custom]` field: the generator never converts
/// it, it only names the host type in the annotation and the signature.
/// `rust` is the field type with every path prefix and the `Option<…>`
/// wrapper removed (`Vec<OwnedRawField>`, `UniversalLabel`).
struct CustomRow {
    rust: &'static str,
    /// The Python annotation, without `| None`.
    py: &'static str,
    java: &'static str,
    sig: &'static str,
    /// The Rust type of a native class's field of this type (`""` when only
    /// a dataclass holds it).
    py_rs: &'static str,
    /// A native class holds the Rust value as it is (a `Copy` type), so no
    /// hand-written converter is needed on the Python side.
    py_copy: bool,
    /// The dataclass default of a required field of this type, when it has one.
    py_default: Option<&'static str>,
    /// The Builder's initial value, when the Java default is not `null`/zero.
    java_default: Option<&'static str>,
    /// The `JValue` constructor (`Long`, `Int`, …) when the Java type is a
    /// primitive: the builder converter then returns that primitive instead
    /// of an object, and the field must be required.
    prim: Option<&'static str>,
}

const EMPTY_LIST: &str = "java.util.Collections.emptyList()";

const CUSTOM: &[CustomRow] = &[
    CustomRow {
        rust: "UniversalLabel",
        py: "bytes",
        java: "ByteBuffer",
        sig: "Ljava/nio/ByteBuffer;",
        py_rs: "",
        py_copy: false,
        py_default: Some("b\"\\x00\" * 16"),
        java_default: None,
        prim: None,
    },
    // A byte string the Java record carries as `byte[]` rather than as the
    // `ByteBuffer` a plain `Vec<u8>` field gets (the hand ST 0601 record's
    // MIIS Core Identifier).
    CustomRow {
        rust: "Vec<u8>",
        py: "bytes",
        java: "byte[]",
        sig: "[B",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // A 16-byte identifier, `byte[]` in Java.
    CustomRow {
        rust: "[u8;16]",
        py: "bytes",
        java: "byte[]",
        sig: "[B",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // A coded enum whose code point is a full-range `u64`: a Java `long`.
    CustomRow {
        rust: "PayloadType",
        py: "PayloadType | int",
        java: "long",
        sig: "J",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: Some("Long"),
    },
    // Coded enums cross as their raw code point.
    CustomRow {
        rust: "IcingDetected",
        py: "IcingDetected | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "OperationalMode",
        py: "OperationalMode | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "PlatformStatus",
        py: "PlatformStatus | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "SensorControlMode",
        py: "SensorControlMode | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "SensorFovName",
        py: "SensorFovName | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // ST 0102 and ST 0806 coded enums cross as the enum member (Python) or
    // the raw code point in a boxed `Integer` (Java), as the ST 0601 ones do.
    CustomRow {
        rust: "SecurityClassification",
        py: "SecurityClassification | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "ClassifyingCountryCodingMethod",
        py: "ClassifyingCountryCodingMethod | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "ObjectCountryCodingMethod",
        py: "ObjectCountryCodingMethod | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "RvtPoiType",
        py: "RvtPoiType | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "RvtAoiType",
        py: "RvtAoiType | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // The ST 0605 time status byte, a host class of its own.
    CustomRow {
        rust: "TimeStatus",
        py: "TimeStatus",
        java: "TimeStatus",
        sig: "Lorg/tstrans/klv/TimeStatus;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // The ST 0903 target colour: a 3-tuple in Python, a nested record in Java.
    CustomRow {
        rust: "[u8;3]",
        py: "tuple[int, int, int]",
        java: "TargetColor",
        sig: "Lorg/tstrans/klv/VTargetPack$TargetColor;",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // An ST 1204 typed UUID. The Java record flattens it into two
    // components, so the record is generated for Python only and these
    // Java columns are never read.
    CustomRow {
        rust: "(IdType,[u8;16])",
        py: "tuple[IdType, bytes]",
        java: "",
        sig: "",
        py_rs: "",
        py_copy: false,
        py_default: None,
        java_default: None,
        prim: None,
    },
    // The ST 1010 matrices: tuples in Python, primitive arrays in Java.
    CustomRow {
        rust: "Vec<f64>",
        py: "tuple[float, ...]",
        java: "double[]",
        sig: "[D",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "Vec<bool>",
        py: "tuple[bool, ...]",
        java: "boolean[]",
        sig: "[Z",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: None,
        prim: None,
    },
    // The ST 0601 pass-through lists, whose host item types are not the
    // Rust ones.
    CustomRow {
        rust: "Vec<OwnedRawField>",
        py: "tuple[tuple[int, bytes], ...]",
        java: "List<KlvUnknownField>",
        sig: "Ljava/util/List;",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<u32>",
        py: "tuple[int, ...]",
        java: "List<Long>",
        sig: "Ljava/util/List;",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<KlvFieldError>",
        py: "tuple[KlvFieldError, ...]",
        java: "List<KlvFieldError>",
        sig: "Ljava/util/List;",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<(u32,ImapbSpecial)>",
        py: "tuple[tuple[int, str, int], ...]",
        java: "List<ImapbSpecialEntry>",
        sig: "Ljava/util/List;",
        py_rs: "",
        py_copy: false,
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    // Codec fields whose Java type is not the one the Rust type implies.
    // The frame number of a slice header is a Java `Integer`.
    CustomRow {
        rust: "u32",
        py: "int",
        py_rs: "u32",
        py_copy: true,
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_default: None,
        java_default: None,
        prim: None,
    },
    // A frame size is a pair in Python and a nested record in Java.
    CustomRow {
        rust: "(u32,u32)",
        py: "Tuple[int, int]",
        py_rs: "(u32, u32)",
        py_copy: true,
        java: "FrameSize",
        sig: "Lorg/tstrans/codec/Av1FrameHeaderLight$FrameSize;",
        py_default: None,
        java_default: None,
        prim: None,
    },
    // The OBUs an AV1 walk could not parse: `(type, message)` pairs in
    // Python, a nested record in Java.
    CustomRow {
        rust: "Vec<(u8,CodecParseError)>",
        py: "List[Tuple[int, str]]",
        py_rs: "Vec<(u8, String)>",
        py_copy: false,
        java: "List<UnparseableObu>",
        sig: "Ljava/util/List;",
        py_default: None,
        java_default: None,
        prim: None,
    },
];

/// A `Copy` Rust enum (or small value) that both bindings already convert
/// by hand: a native Python class holds the host type, a JVM builder
/// returns the Java object.
pub struct HostRow {
    pub rust: &'static str,
    /// The Python annotation.
    pub py: &'static str,
    /// The Rust type of the host class, built with `From<Rust>`.
    pub py_rs: &'static str,
    /// The Java type.
    pub java: &'static str,
    /// The JNI signature of the Java type.
    pub sig: &'static str,
    /// The JVM builder: `fn(&mut Env, Rust) -> Result<JObject, ()>`.
    pub jvm: &'static str,
}

const fn host(
    rust: &'static str,
    py_rs: &'static str,
    java_pkg_class: &'static str,
    sig: &'static str,
    jvm: &'static str,
) -> HostRow {
    HostRow {
        rust,
        py: rust,
        py_rs,
        java: java_pkg_class,
        sig,
        jvm,
    }
}

pub const HOSTS: &[HostRow] = &[
    host(
        "ChromaFormat",
        "crate::codec::ChromaFormatPy",
        "ChromaFormat",
        "Lorg/tstrans/codec/ChromaFormat;",
        "crate::codec::shared::build_chroma_format",
    ),
    host(
        "ColourPrimaries",
        "crate::codec::ColourPrimariesPy",
        "ColourPrimaries",
        "Lorg/tstrans/codec/ColourPrimaries;",
        "crate::codec::shared::build_colour_primaries",
    ),
    host(
        "TransferCharacteristics",
        "crate::codec::TransferCharacteristicsPy",
        "TransferCharacteristics",
        "Lorg/tstrans/codec/TransferCharacteristics;",
        "crate::codec::shared::build_transfer",
    ),
    host(
        "MatrixCoefficients",
        "crate::codec::MatrixCoefficientsPy",
        "MatrixCoefficients",
        "Lorg/tstrans/codec/MatrixCoefficients;",
        "crate::codec::shared::build_matrix",
    ),
    host(
        "EntropyCodingMode",
        "crate::codec::EntropyCodingModePy",
        "EntropyCodingMode",
        "Lorg/tstrans/codec/EntropyCodingMode;",
        "crate::codec::h264::build_entropy_mode",
    ),
    host(
        "H264SliceType",
        "crate::codec::H264SliceTypePy",
        "H264SliceType",
        "Lorg/tstrans/codec/H264SliceType;",
        "crate::codec::h264::build_slice_type",
    ),
    host(
        "H265SliceType",
        "crate::codec::H265SliceTypePy",
        "H265SliceType",
        "Lorg/tstrans/codec/H265SliceType;",
        "crate::codec::h265::build_slice_type",
    ),
    host(
        "H266SliceType",
        "crate::codec::H266SliceTypePy",
        "H266SliceType",
        "Lorg/tstrans/codec/H266SliceType;",
        "crate::codec::h266::build_slice_type",
    ),
    host(
        "AacProfile",
        "crate::codec::AacProfilePy",
        "AacProfile",
        "Lorg/tstrans/codec/AacProfile;",
        "crate::codec::aac::build_profile",
    ),
    host(
        "MpegVersion",
        "crate::codec::MpegVersionPy",
        "MpegVersion",
        "Lorg/tstrans/codec/MpegVersion;",
        "crate::codec::aac::build_mpeg_version",
    ),
    host(
        "AacChannelLayout",
        "crate::codec::AacChannelLayoutPy",
        "AacChannelLayout",
        "Lorg/tstrans/codec/AacChannelLayout;",
        "crate::codec::aac::build_channel_layout",
    ),
    host(
        "Layer",
        "crate::codec::LayerPy",
        "Layer",
        "Lorg/tstrans/codec/Layer;",
        "crate::codec::mpegaudio::build_layer",
    ),
    host(
        "Version",
        "crate::codec::VersionPy",
        "Version",
        "Lorg/tstrans/codec/Version;",
        "crate::codec::mpegaudio::build_version",
    ),
    host(
        "ChannelMode",
        "crate::codec::ChannelModePy",
        "ChannelMode",
        "Lorg/tstrans/codec/ChannelMode;",
        "crate::codec::mpegaudio::build_channel_mode",
    ),
    host(
        "MispTimeKind",
        "crate::codec::MispTimeKindPy",
        "MispTimeKind",
        "Lorg/tstrans/codec/MispTimeKind;",
        "crate::codec::misp_time::build_kind",
    ),
    host(
        "ClockAlignment",
        "crate::rtp::publish::ClockAlignmentPy",
        "ClockAlignment",
        "Lorg/tstrans/rtp/ClockAlignment;",
        "crate::rtp::publish::build_clock_alignment",
    ),
];

/// Whether the Python conversion of a host row can fail (a variant this
/// build does not know). The host type is then built with `TryFrom<Rust>`
/// and the class that holds it with `TryFrom<&Record>`, raising `PyErr` as
/// the JVM builder throws.
pub fn host_fallible(name: &str) -> bool {
    name == "ClockAlignment"
}

fn host_row(name: &str) -> &'static HostRow {
    HOSTS.iter().find(|h| h.rust == name).expect("host row")
}

/// The path of the JVM builder of a host-typed value.
pub fn host_jvm(name: &str) -> &'static str {
    host_row(name).jvm
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Scalar(Sc),
    Str,
    Bytes,
    Opt(Box<Ty>),
    List(Box<Ty>),
    /// `BTreeMap<u8, T>`: a dict in Python, a `HashMap` in Java. To the host only.
    Map(Box<Ty>),
    Record(String),
    /// A type in [`HOSTS`].
    Host(&'static str),
    Custom(&'static str),
}

/// What one scalar needs on each side.
pub struct ScInfo {
    /// The Rust spelling.
    pub rs: &'static str,
    pub py: &'static str,
    pub java: &'static str,
    pub boxed: &'static str,
    pub sig: char,
    pub jvalue: &'static str,
    /// The `JValueOwned` accessor for the signature character.
    pub extract: char,
    pub unbox: &'static str,
}

impl Sc {
    pub fn info(self) -> ScInfo {
        let (rs, py, java, boxed, sig, jvalue, extract, unbox) = match self {
            Sc::U8 | Sc::U16 => (
                if self == Sc::U8 { "u8" } else { "u16" },
                "int",
                "int",
                "Integer",
                'I',
                "Int",
                'i',
                "intValue",
            ),
            Sc::U32 => ("u32", "int", "long", "Long", 'J', "Long", 'j', "longValue"),
            Sc::U64 => ("u64", "int", "long", "Long", 'J', "Long", 'j', "longValue"),
            // Narrow signed integers cross as Java `int`, range-checked on the
            // way in: the hand-written ST 0601 record carries its `i8` and
            // `i16` items so, and a `byte`/`short` component would break
            // every caller.
            Sc::I8 | Sc::I16 => (
                if self == Sc::I8 { "i8" } else { "i16" },
                "int",
                "int",
                "Integer",
                'I',
                "Int",
                'i',
                "intValue",
            ),
            Sc::I32 => ("i32", "int", "int", "Integer", 'I', "Int", 'i', "intValue"),
            Sc::I64 => ("i64", "int", "long", "Long", 'J', "Long", 'j', "longValue"),
            Sc::F32 => (
                "f32",
                "float",
                "float",
                "Float",
                'F',
                "Float",
                'f',
                "floatValue",
            ),
            Sc::F64 => (
                "f64",
                "float",
                "double",
                "Double",
                'D',
                "Double",
                'd',
                "doubleValue",
            ),
            Sc::Bool => (
                "bool",
                "bool",
                "boolean",
                "Boolean",
                'Z',
                "Bool",
                'z',
                "booleanValue",
            ),
            Sc::Usize | Sc::Duration => (
                if self == Sc::Usize {
                    "usize"
                } else {
                    "Duration"
                },
                "int",
                "long",
                "Long",
                'J',
                "Long",
                'j',
                "longValue",
            ),
        };
        ScInfo {
            rs,
            py,
            java,
            boxed,
            sig,
            jvalue,
            extract,
            unbox,
        }
    }

    /// The Rust type of a native Python class's field of this scalar.
    pub fn host_rs(self) -> &'static str {
        match self {
            Sc::Usize | Sc::Duration => "u64",
            other => other.info().rs,
        }
    }

    /// The host value of `e`, a by-value expression of this scalar.
    pub fn to_host(self, e: &str) -> String {
        match self {
            Sc::Usize => format!("{e} as u64"),
            Sc::Duration => {
                // A dereference binds looser than a method call.
                let recv = if e.starts_with('*') {
                    format!("({e})")
                } else {
                    e.to_string()
                };
                format!("u64::try_from({recv}.as_micros()).unwrap_or(u64::MAX)")
            }
            _ => e.to_string(),
        }
    }

    /// The narrowing helper in `crate::jutil`, when the Java value is wider.
    /// `u64` has none: it reads the bit pattern of the Java `long`.
    pub fn narrow(self) -> Option<&'static str> {
        match self {
            Sc::U8 => Some("checked_u8"),
            Sc::U16 => Some("checked_u16"),
            Sc::U32 => Some("checked_u32"),
            Sc::I8 => Some("checked_i8"),
            Sc::I16 => Some("checked_i16"),
            _ => None,
        }
    }
}

const SCALARS: &[(&str, Sc)] = &[
    ("u8", Sc::U8),
    ("u16", Sc::U16),
    ("u32", Sc::U32),
    ("u64", Sc::U64),
    ("i8", Sc::I8),
    ("i16", Sc::I16),
    ("i32", Sc::I32),
    ("i64", Sc::I64),
    ("f32", Sc::F32),
    ("f64", Sc::F64),
    ("bool", Sc::Bool),
    ("usize", Sc::Usize),
    ("Duration", Sc::Duration),
];

const PATH_PREFIXES: &[&str] = &[
    "std::string::",
    "alloc::string::",
    "std::vec::",
    "alloc::vec::",
    "core::option::",
    "std::option::",
    "alloc::collections::",
    "std::collections::",
    "std::time::",
    "core::time::",
];

/// Maps a type spelling as `stringify!` wrote it. `names` are the registered
/// record names a `Vec<…>` or `Option<…>` may hold. An unmapped spelling is
/// an error naming it.
pub fn map(spelling: &str, names: &[&str]) -> Result<Ty, String> {
    let mut s: String = spelling.chars().filter(|c| !c.is_whitespace()).collect();
    for p in PATH_PREFIXES {
        s = s.replace(p, "");
    }
    map_norm(&s, names).map_err(|e| format!("{e} (type `{spelling}`)"))
}

fn map_norm(s: &str, names: &[&str]) -> Result<Ty, String> {
    if let Some(inner) = s.strip_prefix("Option<").and_then(|r| r.strip_suffix('>')) {
        let t = map_norm(inner, names)?;
        if matches!(t, Ty::Opt(_)) {
            return Err("nested Option is not mapped".into());
        }
        return Ok(Ty::Opt(Box::new(t)));
    }
    if s == "Vec<u8>" || s == "SharedBytes" || is_u8_array(s) {
        return Ok(Ty::Bytes);
    }
    if let Some(inner) = s
        .strip_prefix("BTreeMap<u8,")
        .and_then(|r| r.strip_suffix('>'))
    {
        let t = map_norm(inner, names)?;
        if !matches!(t, Ty::Record(_)) {
            return Err("a BTreeMap<u8, _> holds records".into());
        }
        return Ok(Ty::Map(Box::new(t)));
    }
    if let Some(inner) = s.strip_prefix("Vec<").and_then(|r| r.strip_suffix('>')) {
        let t = map_norm(inner, names)?;
        if matches!(t, Ty::Opt(_)) {
            return Err("Vec<Option<T>> is not mapped".into());
        }
        if t == Ty::Bytes {
            // The list emitters convert each item as a value, not as bytes.
            return Err("Vec<Vec<u8>> is not mapped".into());
        }
        return Ok(Ty::List(Box::new(t)));
    }
    if s == "String" {
        return Ok(Ty::Str);
    }
    if let Some((_, sc)) = SCALARS.iter().find(|(n, _)| *n == s) {
        return Ok(Ty::Scalar(*sc));
    }
    if let Some(h) = HOSTS.iter().find(|h| h.rust == s) {
        return Ok(Ty::Host(h.rust));
    }
    if names.contains(&s) {
        return Ok(Ty::Record(s.to_string()));
    }
    Err(format!("unmapped type `{s}`"))
}

/// `[u8;4]`: a fixed byte array crosses as bytes.
fn is_u8_array(s: &str) -> bool {
    s.strip_prefix("[u8;")
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// `crate::klv::imapb::ImapbSpecial` to `ImapbSpecial`, everywhere in `s`.
fn strip_paths(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let (mut out, mut ident, mut i) = (String::new(), String::new(), 0);
    while i < cs.len() {
        let c = cs[i];
        if c.is_alphanumeric() || c == '_' {
            ident.push(c);
            i += 1;
        } else if c == ':' && cs.get(i + 1) == Some(&':') {
            ident.clear();
            i += 2;
        } else {
            out.push_str(&ident);
            ident.clear();
            out.push(c);
            i += 1;
        }
    }
    out.push_str(&ident);
    out
}

/// Resolves a `@[custom]` field's host type from the custom table.
fn map_custom(spelling: &str) -> Result<Ty, String> {
    let mut s = strip_paths(
        &spelling
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>(),
    );
    let mut opt = false;
    if let Some(inner) = s.strip_prefix("Option<").and_then(|r| r.strip_suffix('>')) {
        opt = true;
        s = inner.to_string();
    }
    let row = CUSTOM
        .iter()
        .find(|r| r.rust == s)
        .ok_or_else(|| format!("no host type for custom type `{spelling}`; add a CUSTOM row"))?;
    let t = Ty::Custom(row.rust);
    Ok(if opt { Ty::Opt(Box::new(t)) } else { t })
}

fn custom_row(name: &str) -> &'static CustomRow {
    CUSTOM.iter().find(|r| r.rust == name).expect("custom row")
}

impl Ty {
    /// The `JValue` constructor of a custom field whose Java type is a primitive.
    pub fn custom_prim(&self) -> Option<&'static str> {
        match self {
            Ty::Custom(n) => custom_row(n).prim,
            _ => None,
        }
    }

    pub fn is_opt(&self) -> bool {
        matches!(self, Ty::Opt(_))
    }

    /// Whether a native class holds this custom type as the Rust value, so
    /// that no hand-written converter is called.
    pub fn is_py_copy(&self) -> bool {
        match self {
            Ty::Opt(t) => t.is_py_copy(),
            Ty::Custom(n) => custom_row(n).py_copy,
            _ => false,
        }
    }

    /// Whether a value of this type has no way back from the host: a map, a
    /// host-typed enum, a count or a duration, or a list or option of them.
    pub fn is_to_host_only(&self) -> bool {
        match self {
            Ty::Map(_) | Ty::Host(_) | Ty::Scalar(Sc::Usize | Sc::Duration) => true,
            Ty::Opt(t) | Ty::List(t) => t.is_to_host_only(),
            _ => false,
        }
    }

    /// The Python annotation (without the `| None`).
    pub fn py_ann(&self) -> String {
        match self {
            Ty::Scalar(s) => s.info().py.into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Opt(t) => t.py_ann(),
            Ty::List(t) => format!("tuple[{}, ...]", t.py_ann()),
            Ty::Map(t) => format!("Dict[int, {}]", t.py_ann()),
            Ty::Record(n) => n.clone(),
            Ty::Host(n) => host_row(n).py.into(),
            Ty::Custom(n) => custom_row(n).py.into(),
        }
    }

    /// The annotation in a native class's stub, in the `typing` spelling the
    /// module's stub uses (an optional one is `Optional[…]`).
    pub fn py_ann_native(&self) -> String {
        match self {
            Ty::Opt(t) => format!("Optional[{}]", t.py_ann_native()),
            Ty::List(t) => format!("List[{}]", t.py_ann_native()),
            Ty::Map(t) => format!("Dict[int, {}]", t.py_ann_native()),
            Ty::Custom(n) => custom_row(n).py.into(),
            other => other.py_ann(),
        }
    }

    /// The Rust type of a native class's field.
    pub fn host_rs(&self) -> String {
        match self {
            Ty::Scalar(s) => s.host_rs().into(),
            Ty::Str => "String".into(),
            Ty::Bytes => "crate::util::HostBytes".into(),
            Ty::Opt(t) => format!("Option<{}>", t.host_rs()),
            Ty::List(t) => format!("Vec<{}>", t.host_rs()),
            Ty::Map(t) => format!("std::collections::BTreeMap<u8, {}>", t.host_rs()),
            Ty::Record(n) => n.clone(),
            Ty::Host(n) => host_row(n).py_rs.into(),
            Ty::Custom(n) => custom_row(n).py_rs.into(),
        }
    }

    /// Whether a native class's field of this type is `Copy`; `rec_copy`
    /// answers for a nested record.
    pub fn host_copy(&self, rec_copy: &dyn Fn(&str) -> bool) -> bool {
        match self {
            Ty::Scalar(_) | Ty::Host(_) => true,
            Ty::Opt(t) => t.host_copy(rec_copy),
            Ty::Record(n) => rec_copy(n),
            Ty::Custom(n) => custom_row(n).py_copy,
            Ty::Str | Ty::Bytes | Ty::List(_) | Ty::Map(_) => false,
        }
    }

    /// The conversion of `e`, an expression of type `&T` where `T` is the Rust
    /// field type, to the native class's field type. `custom` is the
    /// hand-written converter's path for a custom field.
    pub fn to_host(&self, e: &str, custom: Option<&str>) -> String {
        // A method receiver needs no `&`; a scalar read needs a `*` unless
        // the expression is an address.
        let recv = e.strip_prefix('&').unwrap_or(e);
        let val = match e.strip_prefix('&') {
            Some(place) => place.to_string(),
            None => format!("*{e}"),
        };
        if let Ty::Opt(inner) = self {
            if let Some(c) = custom {
                return format!("{c}({e})");
            }
            return match inner.as_ref() {
                Ty::Scalar(sc @ (Sc::Usize | Sc::Duration)) => {
                    format!("{val}.map(|v| {})", sc.to_host("v"))
                }
                Ty::Scalar(_) => val,
                Ty::Custom(_) => val,
                Ty::Host(h) => format!("{val}.map({}::from)", host_row(h).py_rs),
                Ty::Record(n) => format!("{recv}.as_ref().map({n}::from)"),
                other => format!("{recv}.as_ref().map(|v| {})", other.to_host("v", None)),
            };
        }
        match self {
            Ty::Scalar(sc) => sc.to_host(&val),
            Ty::Str => format!("{recv}.clone()"),
            Ty::Bytes => format!("crate::util::HostBytes({recv}.to_vec())"),
            Ty::Record(n) => format!("{n}::from({e})"),
            Ty::Host(h) if host_fallible(h) => {
                format!("{}::try_from({val})?", host_row(h).py_rs)
            }
            Ty::Host(h) => format!("{}::from({val})", host_row(h).py_rs),
            Ty::List(inner) => match inner.as_ref() {
                Ty::Record(n) => format!("{recv}.iter().map({n}::from).collect()"),
                _ => format!("{recv}.clone()"),
            },
            Ty::Map(inner) => match inner.as_ref() {
                Ty::Record(n) => {
                    format!("{recv}.iter().map(|(k, v)| (*k, {n}::from(v))).collect()")
                }
                _ => unreachable!("a map holds records"),
            },
            Ty::Custom(n) => match custom {
                Some(c) => format!("{c}({e})"),
                None => {
                    debug_assert!(custom_row(n).py_copy);
                    val
                }
            },
            Ty::Opt(_) => unreachable!("handled above"),
        }
    }

    /// The Java type as written in a record component.
    pub fn java(&self) -> String {
        match self {
            Ty::Scalar(s) => s.info().java.into(),
            Ty::Opt(t) => t.java_boxed(),
            _ => self.java_boxed(),
        }
    }

    /// The reference type (what a generic argument or an `Option` needs).
    pub fn java_boxed(&self) -> String {
        match self {
            Ty::Scalar(s) => s.info().boxed.into(),
            Ty::Str => "String".into(),
            Ty::Bytes => "ByteBuffer".into(),
            Ty::Opt(t) => t.java_boxed(),
            Ty::List(t) => format!("List<{}>", t.java_boxed()),
            Ty::Map(t) => format!("Map<Integer, {}>", t.java_boxed()),
            Ty::Record(n) => n.clone(),
            Ty::Host(n) => host_row(n).java.into(),
            Ty::Custom(n) => custom_row(n).java.into(),
        }
    }

    /// The Java type a Builder setter takes: an optional scalar is set by its
    /// primitive (an absent one is simply not set), anything else as the
    /// component is typed.
    pub fn setter_java(&self) -> String {
        match self {
            Ty::Opt(t) if matches!(**t, Ty::Scalar(_)) => t.java(),
            _ => self.java(),
        }
    }

    /// The JNI signature of [`Ty::setter_java`].
    pub fn setter_sig(&self, reg: &[Entry]) -> String {
        match self {
            Ty::Opt(t) if matches!(**t, Ty::Scalar(_)) => t.sig(reg),
            _ => self.sig(reg),
        }
    }

    /// The JNI signature of the Java type of a component.
    pub fn sig(&self, reg: &[Entry]) -> String {
        match self {
            Ty::Scalar(s) => s.info().sig.to_string(),
            Ty::Opt(t) => t.sig_boxed(reg),
            _ => self.sig_boxed(reg),
        }
    }

    pub fn sig_boxed(&self, reg: &[Entry]) -> String {
        match self {
            Ty::Scalar(s) => format!("Ljava/lang/{};", s.info().boxed),
            Ty::Str => "Ljava/lang/String;".into(),
            Ty::Bytes => "Ljava/nio/ByteBuffer;".into(),
            Ty::Opt(t) => t.sig_boxed(reg),
            Ty::List(_) => "Ljava/util/List;".into(),
            Ty::Map(_) => "Ljava/util/Map;".into(),
            Ty::Record(n) => {
                let pkg = lookup(reg, n).map(|e| e.java_pkg).unwrap_or("");
                format!("L{pkg}/{n};")
            }
            Ty::Host(n) => host_row(n).sig.into(),
            Ty::Custom(n) => custom_row(n).sig.into(),
        }
    }
}

/// One field with its resolved types.
pub struct Fld<'a> {
    pub spec: &'a FieldSpec,
    /// `None` for a skipped field.
    pub ty: Option<Ty>,
    /// The Java accessor name.
    pub java: String,
    pub custom: bool,
    /// The dataclass default: `None` for an absent optional, the type's zero
    /// for a required field that follows a defaulted one (a dataclass cannot
    /// take a required field after a default), the CUSTOM row's default for
    /// a custom field. `None` when the field is plain required.
    pub py_default: Option<String>,
    /// The Builder's initial value when it is not Java's own default.
    pub java_default: Option<String>,
}

/// The host zero of a required field that must be defaulted.
fn type_default(ty: &Ty) -> Option<(String, Option<String>)> {
    Some(match ty {
        Ty::Scalar(Sc::F32 | Sc::F64) => ("0.0".into(), None),
        Ty::Scalar(Sc::Bool) => ("False".into(), None),
        Ty::Scalar(_) => ("0".into(), None),
        Ty::Str => ("\"\"".into(), Some("\"\"".into())),
        Ty::Bytes => (
            "b\"\"".into(),
            Some("java.nio.ByteBuffer.allocate(0)".into()),
        ),
        Ty::List(_) => ("()".into(), Some(EMPTY_LIST.into())),
        Ty::Opt(_) | Ty::Record(_) | Ty::Host(_) | Ty::Map(_) | Ty::Custom(_) => return None,
    })
}

/// The record a type holds when that record has a host name of its own.
fn renamed_record<'a>(reg: &'a [Entry], t: &'a Ty) -> Option<&'a str> {
    match t {
        Ty::Opt(i) | Ty::List(i) | Ty::Map(i) => renamed_record(reg, i),
        Ty::Record(n) => lookup(reg, n)
            .filter(|e| !e.host_name.is_empty())
            .map(|_| n.as_str()),
        _ => None,
    }
}

/// Resolves every field of a record against the registry.
pub fn resolve<'a>(reg: &[Entry], schema: &'a RecordSchema) -> Result<Vec<Fld<'a>>, String> {
    let names: Vec<&str> = reg.iter().map(|e| e.schema.name).collect();
    // A native class has no constructor defaults to derive.
    let native = lookup(reg, schema.name).is_some_and(Entry::is_native);
    let mut out = Vec::new();
    let mut defaulted: Option<&str> = None;
    for f in schema.fields {
        let ctx = |e: String| format!("{}.{}: {e}", schema.name, f.name);
        let (ty, custom) = match f.mark {
            FieldMark::Skip => (None, false),
            FieldMark::Custom => (Some(map_custom(f.ty).map_err(ctx)?), true),
            _ => (Some(map(f.ty, &names).map_err(ctx)?), false),
        };
        if let Some(t) = &ty
            && let Some(n) = renamed_record(reg, t)
        {
            return Err(ctx(format!(
                "`{n}` has another name in the hosts, so no record can hold it"
            )));
        }
        let mut py_default = None;
        let mut java_default = None;
        if let Some(Ty::Opt(inner)) = &ty
            && inner.custom_prim().is_some()
        {
            return Err(ctx(
                "a primitive custom type cannot be optional: a Java primitive has no null".into(),
            ));
        }
        if let Some(t) = ty.as_ref().filter(|_| !native) {
            if t.is_opt() {
                py_default = Some("None".to_string());
            } else if let Ty::Custom(n) = t {
                let row = custom_row(n);
                py_default = row.py_default.map(str::to_string);
                java_default = row.java_default.map(str::to_string);
            } else if defaulted.is_none() {
                // Nothing before it has a default: required, no default.
            } else if let Some((p, j)) = type_default(t) {
                py_default = Some(p);
                java_default = j;
            }
            // A declared default wins over the one the type implies.
            if let Some(d) = f.py_default {
                py_default = Some(d.to_string());
            }
            if py_default.is_none() {
                if let Some(d) = defaulted {
                    return Err(ctx(format!(
                        "a required field follows the defaulted `{d}` but has no host default; \
                         give its CUSTOM row a default or declare it before the optional fields"
                    )));
                }
            } else if defaulted.is_none() {
                defaulted = Some(f.name);
            }
        }
        let java = if ty.is_some() {
            java_name(schema.name, f)?
        } else {
            camel(f.name)
        };
        out.push(Fld {
            spec: f,
            ty,
            java,
            custom,
            py_default,
            java_default,
        });
    }
    debug_assert_eq!(
        out.iter().filter(|f| f.ty.is_some()).count(),
        crossing(schema).count()
    );
    Ok(out)
}
