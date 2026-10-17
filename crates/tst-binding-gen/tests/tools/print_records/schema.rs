//! The registry: the one hand-maintained list of records the generator
//! renders, and the small naming helpers every emitter shares.

use tst_core::binding_schema::{FieldMark, FieldSpec, RecordSchema};

/// Which way a record crosses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Rust to host and host to Rust.
    Both,
    /// Rust to host only: the builder and the Python `_to_py` converter.
    ToHost,
    /// Host to Rust only: the reader and the Python `_from_py` converter.
    FromHost,
}

impl Direction {
    pub fn crosses_out(self) -> bool {
        matches!(self, Direction::Both | Direction::ToHost)
    }
    pub fn crosses_in(self) -> bool {
        matches!(self, Direction::Both | Direction::FromHost)
    }
}

/// How the Python host type of a record is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PyShape {
    /// A frozen dataclass in the module's `.py` file, converted through
    /// `<snake>_to_py` / `<snake>_from_py`.
    Dataclass,
    /// A native class: a `#[pyclass(frozen, get_all)]` struct of host-typed
    /// copies, generated into `bindings/python/src/generated/<family>.rs`.
    /// Rust to host only.
    Native,
}

/// How a hand-written method or property of a native class is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraKind {
    /// An instance method.
    Method,
    /// A static method returning the class.
    Static,
}

/// A method or property of a native class that the record's fields do not
/// give: the generated class declares it and calls the hand-written
/// `crate::custom::<snake>_<name>` for the body. The stub carries the
/// signature.
pub struct PyExtra {
    pub name: &'static str,
    pub kind: ExtraKind,
    /// `(name, Rust type, Python annotation)` of each parameter after `self`.
    pub params: &'static [(&'static str, &'static str, &'static str)],
    /// The Rust return type (`Self` for a static method).
    pub ret_rs: &'static str,
    /// The Python return annotation.
    pub ret_py: &'static str,
    /// The doc lines of the method, as written after `///`.
    pub doc: &'static str,
}

/// One registered record.
pub struct Entry {
    /// The generated-file family, e.g. `st0601`.
    pub family: &'static str,
    /// The Python module that holds the dataclass, e.g. `tstrans.klv`.
    pub py_module: &'static str,
    /// The Java package directory, e.g. `org/tstrans/klv`.
    pub java_pkg: &'static str,
    pub direction: Direction,
    /// Whether the JVM side is generated. `false` for a record whose Java
    /// shape the schema cannot express: it is generated for Python only.
    pub jvm: bool,
    /// Whether a JNI builder is generated for the record. `false` for a
    /// record Java builds itself: only its Java record is generated.
    pub jvm_builder: bool,
    /// The path a binding crate names the record by, e.g.
    /// `tst_core::klv::st0601::WavelengthRecord`. Explicit because the
    /// declaring module is often private and re-exported higher up.
    pub rust_path: &'static str,
    /// The interface the Java record implements, `""` for none.
    pub java_implements: &'static str,
    pub schema: RecordSchema,
    /// Whether the Python side is generated. `false` for a record only Java
    /// exposes (Python hands the same numbers over as a dict).
    pub python: bool,
    /// The Python host type.
    pub shape: PyShape,
    /// The name of the host classes when it is not the Rust name (the Rust
    /// `AdtsFrameOwned` is `AdtsFrame` to a host), `""` for the Rust name.
    /// A renamed record cannot be held by another record.
    pub host_name: &'static str,
    /// A native class takes its crossing fields as constructor arguments,
    /// all required, in declaration order.
    pub py_ctor: bool,
    /// A native class compares by field (`==`); the Rust record needs no
    /// `PartialEq`, the generated struct derives it.
    pub py_eq: bool,
    /// `(Rust field, Python name)` where the Python attribute is not named
    /// as the Rust field.
    pub py_rename: &'static [(&'static str, &'static str)],
    /// A native class's `__repr__` as a `format!` string and its argument
    /// expressions (`self` is the class); `None` lists the fields.
    pub py_repr: Option<(&'static str, &'static [&'static str])>,
    pub py_extras: &'static [PyExtra],
}

impl Entry {
    /// A record with no native-class options: a dataclass crossing in
    /// `direction`.
    pub fn new(
        family: &'static str,
        py_module: &'static str,
        java_pkg: &'static str,
        direction: Direction,
        rust_path: &'static str,
        java_implements: &'static str,
        schema: RecordSchema,
    ) -> Entry {
        Entry {
            family,
            py_module,
            java_pkg,
            direction,
            jvm: true,
            jvm_builder: true,
            rust_path,
            java_implements,
            schema,
            python: true,
            shape: PyShape::Dataclass,
            host_name: "",
            py_ctor: false,
            py_eq: false,
            py_rename: &[],
            py_repr: None,
            py_extras: &[],
        }
    }

    /// The name the host classes and files go by.
    pub fn name(&self) -> &'static str {
        if self.host_name.is_empty() {
            self.schema.name
        } else {
            self.host_name
        }
    }

    pub fn is_native(&self) -> bool {
        self.shape == PyShape::Native
    }

    /// The Python attribute name of a Rust field.
    pub fn py_name(&self, rust: &'static str) -> &'static str {
        self.py_rename
            .iter()
            .find(|(r, _)| *r == rust)
            .map_or(rust, |(_, p)| p)
    }
}

/// One record of a family: both directions, the `tstrans.klv` module and the
/// `org.tstrans.klv` package.
fn klv(
    family: &'static str,
    rust_path: &'static str,
    java_implements: &'static str,
    schema: RecordSchema,
) -> Entry {
    Entry::new(
        family,
        "tstrans.klv",
        "org/tstrans/klv",
        Direction::Both,
        rust_path,
        java_implements,
        schema,
    )
}

/// Every generated record. A record declared with `binding_record!` and
/// absent here is caught by the registry-completeness test.
pub fn records() -> Vec<Entry> {
    use tst_core::klv::st0102::SecurityLs;
    use tst_core::klv::st0601::{
        AirbaseLocations, ControlCommand, CountryCodes, ImageHorizonPixels, Location,
        MetadataSubstreamId, PayloadList, PayloadRecord, SdccFlpField, SensorFrameRate,
        UasDatalinkLs, ViewDomain, ViewDomainPair, WavelengthRecord, Waypoint, WeaponsStore,
    };
    use tst_core::klv::st0605::PrecisionTimeStampPack;
    use tst_core::klv::st0805::CotConfig;
    use tst_core::klv::st0806::{RvtAoi, RvtLs, RvtPoi, RvtUserData};
    use tst_core::klv::st0903::{VTargetPack, VmtiLs};
    use tst_core::klv::st1010::SdccFlp;
    use tst_core::klv::st1204::CoreId;
    macro_rules! rec {
        ($family:literal, $module:literal, $name:ident) => {
            rec!($family, $module, $name, "")
        };
        ($family:literal, $module:literal, $name:ident, $implements:expr) => {
            klv(
                $family,
                concat!("tst_core::klv::", $module, "::", stringify!($name)),
                $implements,
                $name::BINDING_SCHEMA,
            )
        };
    }
    let mut core_id = rec!("st1204", "st1204", CoreId);
    core_id.jvm = false;
    let mut cot_config = rec!("st0805", "st0805", CotConfig);
    cot_config.direction = Direction::FromHost;
    // The only SDCC-FLP encoder takes plain lists, not the record.
    let mut sdcc_flp = rec!("st1010", "st1010", SdccFlp);
    sdcc_flp.direction = Direction::ToHost;
    let mut v = vec![
        rec!("st0601", "st0601", WavelengthRecord),
        rec!("st0601", "st0601", ImageHorizonPixels),
        rec!("st0601", "st0601", ControlCommand),
        rec!("st0601", "st0601", SensorFrameRate),
        rec!("st0601", "st0601", MetadataSubstreamId),
        rec!("st0601", "st0601", CountryCodes),
        rec!("st0601", "st0601", Location),
        rec!("st0601", "st0601", AirbaseLocations),
        rec!("st0601", "st0601", PayloadRecord),
        rec!("st0601", "st0601", PayloadList),
        rec!("st0601", "st0601", WeaponsStore),
        rec!("st0601", "st0601", Waypoint),
        rec!("st0601", "st0601", ViewDomainPair),
        rec!("st0601", "st0601", ViewDomain),
        rec!("st0601", "st0601", SdccFlpField),
        rec!("st0601", "st0601", UasDatalinkLs, "KlvSet"),
        rec!("st0903", "st0903", VTargetPack),
        rec!("st0903", "st0903", VmtiLs, "KlvSet"),
        rec!("st0806", "st0806", RvtUserData),
        rec!("st0806", "st0806", RvtPoi),
        rec!("st0806", "st0806", RvtAoi),
        rec!("st0806", "st0806", RvtLs),
        rec!("st0102", "st0102", SecurityLs, "KlvSet"),
        core_id,
        rec!("st0605", "st0605", PrecisionTimeStampPack, "KlvSet"),
        sdcc_flp,
        cot_config,
    ];
    v.extend(codec_records());
    v.extend(crate::stats::stats_records());
    v
}

/// A reference-style link definition line, `` [`x`]: crate::path::x ``: it
/// names a Rust item and says nothing to a host reader.
fn is_link_definition(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.contains("]: ")
}

/// The line that ends the host-facing part of a doc: what follows it is for
/// Rust readers (a doctest, a note on the owned/borrowed pair) and never
/// reaches a host doc. rustdoc renders an HTML comment as nothing.
pub const RUST_ONLY: &str = "<!-- rust-only -->";

/// The paragraphs of a joined doc text: runs of lines between empty ones,
/// each line without the space a `///` comment leaves, joined by one space.
/// Link definition lines are dropped, and so is everything from the
/// [`RUST_ONLY`] line on.
pub fn paras(doc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in doc
        .lines()
        .map(|l| l.strip_prefix(' ').unwrap_or(l))
        .take_while(|l| l.trim() != RUST_ONLY)
        .filter(|l| !is_link_definition(l))
    {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                out.push(cur.join(" "));
                cur.clear();
            }
        } else {
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        out.push(cur.join(" "));
    }
    out
}

/// A code span that is a Rust path, `` `klv::st0806::decode` `` or
/// `` `Self::field` ``, optionally ending in `()`: its final name.
fn path_tail(span: &str) -> Option<&str> {
    let body = span.strip_suffix("()").unwrap_or(span);
    if !body.contains("::") {
        return None;
    }
    let ok = body
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    let tail = body.rsplit("::").next().unwrap_or("");
    (ok && !tail.is_empty()).then_some(tail)
}

/// Rust spellings a host reader has no use for, reduced to plain names:
/// `` [`X`] `` to `` `X` `` (Javadoc and Sphinx have no target for a rustdoc
/// link), and a code span that is a Rust path to its final name.
pub fn unlink(doc: &str) -> String {
    let mut out = String::new();
    let mut rest = doc;
    while let Some(i) = rest.find("[`") {
        let after = &rest[i + 2..];
        match after.find("`]") {
            Some(j) if !after[..j].contains('`') => {
                out.push_str(&rest[..i]);
                out.push('`');
                out.push_str(&after[..j]);
                out.push('`');
                rest = &after[j + 2..];
            }
            _ => {
                out.push_str(&rest[..i + 2]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    plain_paths(&out)
}

/// Each `` `a::b::Name` `` span of `doc` as `` `Name` ``.
fn plain_paths(doc: &str) -> String {
    let parts: Vec<&str> = doc.split('`').collect();
    if parts.len() % 2 == 0 {
        return doc.to_string(); // an unpaired backtick stays as written
    }
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i % 2 == 1 {
            out.push('`');
            out.push_str(path_tail(p).unwrap_or(p));
            out.push('`');
        } else {
            out.push_str(p);
        }
    }
    out
}

/// `WavelengthRecord` to `wavelength_record`.
pub fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            // A digit ends a word as a lower-case letter does: `H264Sps`.
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if i > 0 && (prev_lower || (next_lower && chars[i - 1].is_ascii_uppercase())) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// `min_nm` to `minNm`.
pub fn camel(field: &str) -> String {
    let mut out = String::new();
    for (i, part) in field.split('_').enumerate() {
        if i == 0 {
            out.push_str(part);
        } else {
            let mut cs = part.chars();
            if let Some(f) = cs.next() {
                out.push(f.to_ascii_uppercase());
                out.push_str(cs.as_str());
            }
        }
    }
    out
}

const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "record",
    "var",
    "yield",
];

/// The Java accessor name of a field: the `@[java("x")]` override, else the
/// camelCase of the Rust name. A Java keyword is an error.
pub fn java_name(record: &str, f: &FieldSpec) -> Result<String, String> {
    let n = f.java.map(str::to_string).unwrap_or_else(|| camel(f.name));
    if JAVA_KEYWORDS.contains(&n.as_str()) {
        return Err(format!(
            "{record}.{}: `{n}` is a Java keyword; give the field @[java(\"…\")]",
            f.name
        ));
    }
    Ok(n)
}

/// The fields that cross, in declaration order.
pub fn crossing(schema: &RecordSchema) -> impl Iterator<Item = &FieldSpec> {
    schema.fields.iter().filter(|f| f.mark != FieldMark::Skip)
}

pub fn lookup<'a>(reg: &'a [Entry], name: &str) -> Option<&'a Entry> {
    reg.iter().find(|e| e.schema.name == name)
}

// ---------------------------------------------------------------------------
// The codec parameter sets: native Python classes, built from the Rust parse
// results and handed to the hosts (Rust to host only).
// ---------------------------------------------------------------------------

/// A coded-size method of an SPS.
const fn coded(name: &'static str, doc: &'static str) -> PyExtra {
    PyExtra {
        name,
        kind: ExtraKind::Method,
        params: &[],
        ret_rs: "u32",
        ret_py: "int",
        doc,
    }
}

const fn ptl_method(ret: &'static str, doc: &'static str) -> PyExtra {
    PyExtra {
        name: "profile_tier_level",
        kind: ExtraKind::Method,
        params: &[],
        ret_rs: ret,
        ret_py: ret,
        doc,
    }
}

const H264_SPS_EXTRAS: &[PyExtra] = &[
    coded(
        "coded_width",
        " Coded picture width before `frame_crop` is applied (luma samples).\n Equal to `width + crop_left + crop_right`.",
    ),
    coded(
        "coded_height",
        " Coded picture height before `frame_crop` is applied (luma samples).\n Equal to `height + crop_top + crop_bottom`.",
    ),
];

const H265_SPS_EXTRAS: &[PyExtra] = &[
    ptl_method(
        "H265ProfileTierLevel",
        " Reconstructed `H265ProfileTierLevel` from the fields decoded inside\n this SPS's `profile_tier_level()` block.",
    ),
    coded(
        "coded_width",
        " Coded picture width before conformance-window crop is applied\n (luma samples). Equal to `width + crop_left + crop_right`.",
    ),
    coded(
        "coded_height",
        " Coded picture height before conformance-window crop is applied\n (luma samples). Equal to `height + crop_top + crop_bottom`.",
    ),
];

const H265_VPS_EXTRAS: &[PyExtra] = &[ptl_method(
    "H265ProfileTierLevel",
    " Reconstructed `H265ProfileTierLevel` from the fields decoded inside\n this VPS's `profile_tier_level()` block.",
)];

const H266_SPS_EXTRAS: &[PyExtra] = &[
    coded(
        "coded_width",
        " Pre-crop luma width — `pic_width_max_in_luma_samples` before\n conformance-window cropping. Equal to `width + crop_left + crop_right`.",
    ),
    coded(
        "coded_height",
        " Pre-crop luma height — `pic_height_max_in_luma_samples` before\n conformance-window cropping. Equal to `height + crop_top + crop_bottom`.",
    ),
];

const MISP_EXTRAS: &[PyExtra] = &[
    PyExtra {
        name: "micros",
        kind: ExtraKind::Static,
        params: &[("value_us", "u64", "int"), ("time_status", "u8", "int")],
        ret_rs: "Self",
        ret_py: "MispTimestamp",
        doc: " Construct a microsecond-precision timestamp (valid for H.264 + H.265).",
    },
    PyExtra {
        name: "nanos",
        kind: ExtraKind::Static,
        params: &[("value_ns", "u64", "int"), ("time_status", "u8", "int")],
        ret_rs: "Self",
        ret_py: "MispTimestamp",
        doc: " Construct a nanosecond-precision timestamp (H.265-only, ST 0604.6 §12.2).",
    },
];

const RATIONAL_EXTRAS: &[PyExtra] = &[PyExtra {
    name: "as_float",
    kind: ExtraKind::Method,
    params: &[],
    ret_rs: "f64",
    ret_py: "float",
    doc: " Return `num / den` as a floating-point value.",
}];

/// A codec record: native class, to the host only, in `tstrans.codec` and
/// `org.tstrans.codec`.
fn codec(rust_path: &'static str, java_implements: &'static str, schema: RecordSchema) -> Entry {
    let mut e = Entry::new(
        "codec",
        "tstrans.codec",
        "org/tstrans/codec",
        Direction::ToHost,
        rust_path,
        java_implements,
        schema,
    );
    e.shape = PyShape::Native;
    e
}

/// The codec records. The values the others hold come first.
fn codec_records() -> Vec<Entry> {
    use tst_core::codec::aac::AdtsFrameOwned;
    use tst_core::codec::av1::{Av1FrameHeaderLight, Av1ObuStream, Av1SequenceHeader};
    use tst_core::codec::h264::{H264ParameterSets, H264Pps, H264SliceHeaderLight, H264Sps};
    use tst_core::codec::h265::{
        H265ParameterSets, H265Pps, H265ProfileTierLevel, H265SliceHeaderLight, H265Sps, H265Vps,
    };
    use tst_core::codec::h266::{
        H266ParameterSets, H266Pps, H266ProfileTierLevel, H266SliceHeaderLight, H266Sps, H266Vps,
    };
    use tst_core::codec::misp_time::MispTimestamp;
    use tst_core::codec::mpegaudio::FrameOwned;
    use tst_core::codec::{ColorInfo, Obu, ObuExtension, Rational};
    macro_rules! rec {
        ($module:literal, $name:ident) => {
            rec!($module, $name, "")
        };
        ($module:literal, $name:ident, $implements:expr) => {
            codec(
                concat!("tst_core::codec::", $module, stringify!($name)),
                $implements,
                $name::BINDING_SCHEMA,
            )
        };
    }
    // The module prefix is empty for the types declared in `codec` itself.
    let mut rational = rec!("", Rational);
    rational.py_ctor = true;
    rational.py_extras = RATIONAL_EXTRAS;
    rational.py_repr = Some(("Rational(num={}, den={})", &["self.num", "self.den"]));

    let mut color = rec!("", ColorInfo);
    color.py_ctor = true;
    color.py_repr = Some((
        "ColorInfo(primaries={:?}, transfer={:?}, matrix={:?}, full_range={})",
        &[
            "self.primaries",
            "self.transfer",
            "self.matrix",
            "self.full_range",
        ],
    ));

    let mut ext = rec!("", ObuExtension);
    ext.py_ctor = true;
    ext.py_repr = Some((
        "ObuExtension(temporal_id={}, spatial_id={})",
        &["self.temporal_id", "self.spatial_id"],
    ));

    let mut obu = rec!("", Obu, "VideoUnit");
    obu.py_ctor = true;
    obu.py_repr = Some((
        "Obu(obu_type={}, payload_len={})",
        &["self.obu_type", "self.payload.0.len()"],
    ));

    let mut misp = rec!("misp_time::", MispTimestamp);
    misp.py_ctor = true;
    misp.py_eq = true;
    misp.py_extras = MISP_EXTRAS;
    misp.py_repr = Some((
        "MispTimestamp(kind={}, time_status={:#04X}, value={})",
        &[
            "crate::custom::misp_timestamp_kind_text(self.kind)",
            "self.time_status",
            "self.value",
        ],
    ));

    let mut h264_sps = rec!("h264::", H264Sps);
    h264_sps.py_extras = H264_SPS_EXTRAS;
    h264_sps.py_repr = Some((
        "H264Sps(profile={}, level={}, {}x{}, sps_id={})",
        &[
            "self.profile_idc",
            "self.level_idc",
            "self.width",
            "self.height",
            "self.seq_parameter_set_id",
        ],
    ));
    let mut h264_pps = rec!("h264::", H264Pps);
    h264_pps.py_repr = Some((
        "H264Pps(pps_id={}, sps_id={})",
        &["self.pic_parameter_set_id", "self.seq_parameter_set_id"],
    ));
    let mut h264_slice = rec!("h264::", H264SliceHeaderLight);
    h264_slice.py_repr = Some((
        "H264SliceHeaderLight(first={}, slice_type={:?}, idr={})",
        &["self.first_in_pic", "self.slice_type", "self.idr"],
    ));
    let mut h264_sets = rec!("h264::", H264ParameterSets);
    h264_sets.py_repr = Some((
        "H264ParameterSets(n_sps={}, n_pps={})",
        &["self.sps_by_id.len()", "self.pps_by_id.len()"],
    ));

    let mut h265_ptl = rec!("h265::", H265ProfileTierLevel);
    // Java builds it from an SPS or a VPS; no native hands one over.
    h265_ptl.jvm_builder = false;
    h265_ptl.py_repr = Some((
        "H265ProfileTierLevel(profile_idc={}, tier={}, level_idc={})",
        &[
            "self.general_profile_idc",
            "self.general_tier_flag",
            "self.general_level_idc",
        ],
    ));
    let mut h265_sps = rec!("h265::", H265Sps);
    h265_sps.py_extras = H265_SPS_EXTRAS;
    h265_sps.py_repr = Some((
        "H265Sps(profile={}, level={}, {}x{}, sps_id={})",
        &[
            "self.general_profile_idc",
            "self.general_level_idc",
            "self.width",
            "self.height",
            "self.sps_seq_parameter_set_id",
        ],
    ));
    let mut h265_vps = rec!("h265::", H265Vps);
    h265_vps.py_extras = H265_VPS_EXTRAS;
    h265_vps.py_repr = Some((
        "H265Vps(vps_id={}, profile={}, level={})",
        &[
            "self.vps_video_parameter_set_id",
            "self.general_profile_idc",
            "self.general_level_idc",
        ],
    ));
    let mut h265_pps = rec!("h265::", H265Pps);
    h265_pps.py_repr = Some((
        "H265Pps(pps_id={}, sps_id={})",
        &[
            "self.pps_pic_parameter_set_id",
            "self.pps_seq_parameter_set_id",
        ],
    ));
    let mut h265_slice = rec!("h265::", H265SliceHeaderLight);
    h265_slice.py_repr = Some((
        "H265SliceHeaderLight(first={}, slice_type={:?}, idr={})",
        &["self.first_in_pic", "self.slice_type", "self.idr"],
    ));
    let mut h265_sets = rec!("h265::", H265ParameterSets);
    h265_sets.py_repr = Some((
        "H265ParameterSets(n_vps={}, n_sps={}, n_pps={})",
        &[
            "self.vps_by_id.len()",
            "self.sps_by_id.len()",
            "self.pps_by_id.len()",
        ],
    ));

    let mut h266_ptl = rec!("h266::", H266ProfileTierLevel);
    h266_ptl.py_repr = Some((
        "H266ProfileTierLevel(profile_idc={}, tier={}, level_idc={})",
        &[
            "self.general_profile_idc",
            "self.general_tier_flag",
            "self.general_level_idc",
        ],
    ));
    let mut h266_sps = rec!("h266::", H266Sps);
    // Python calls the VUI colour `color`, as H.264 and H.265 do.
    h266_sps.py_rename = &[("color_info", "color")];
    h266_sps.py_extras = H266_SPS_EXTRAS;
    h266_sps.py_repr = Some((
        "H266Sps(profile={}, level={}, {}x{}, sps_id={})",
        &[
            "self.profile_tier_level.general_profile_idc",
            "self.profile_tier_level.general_level_idc",
            "self.width",
            "self.height",
            "self.sps_id",
        ],
    ));
    let mut h266_vps = rec!("h266::", H266Vps);
    h266_vps.py_repr = Some((
        "H266Vps(vps_id={}, max_layers={}, max_sub_layers={})",
        &["self.vps_id", "self.max_layers", "self.max_sub_layers"],
    ));
    let mut h266_pps = rec!("h266::", H266Pps);
    h266_pps.py_repr = Some((
        "H266Pps(pps_id={}, sps_id={})",
        &["self.pps_id", "self.sps_id"],
    ));
    let mut h266_slice = rec!("h266::", H266SliceHeaderLight);
    h266_slice.py_repr = Some((
        "H266SliceHeaderLight(first={}, slice_type={:?}, idr={})",
        &["self.first_in_pic", "self.slice_type", "self.idr"],
    ));
    let mut h266_sets = rec!("h266::", H266ParameterSets);
    h266_sets.py_repr = Some((
        "H266ParameterSets(n_vps={}, n_sps={}, n_pps={})",
        &["self.vpses.len()", "self.spses.len()", "self.ppses.len()"],
    ));

    let mut av1_seq = rec!("av1::", Av1SequenceHeader);
    av1_seq.py_repr = Some((
        "Av1SequenceHeader(profile={}, {}x{}, bit_depth={})",
        &[
            "self.profile",
            "self.max_frame_width",
            "self.max_frame_height",
            "self.bit_depth",
        ],
    ));
    let mut av1_frame = rec!("av1::", Av1FrameHeaderLight);
    av1_frame.py_repr = Some((
        "Av1FrameHeaderLight(type={}, show={}, show_existing={})",
        &[
            "self.frame_type",
            "self.show_frame",
            "self.show_existing_frame",
        ],
    ));
    let av1_stream = rec!("av1::", Av1ObuStream);

    let mut adts = rec!("aac::", AdtsFrameOwned, "AudioFrame");
    adts.host_name = "AdtsFrame";
    adts.py_rename = &[("body", "payload")];
    adts.py_repr = Some((
        "AdtsFrame(profile={:?}, sr={}, ch={}, frame_len={})",
        &[
            "self.profile",
            "self.sample_rate_hz",
            "self.channel_configuration",
            "self.frame_length_bytes",
        ],
    ));
    let mut mpeg = rec!("mpegaudio::", FrameOwned, "AudioFrame");
    mpeg.host_name = "Mpeg2AudioFrame";
    mpeg.py_rename = &[("body", "payload")];
    mpeg.py_repr = Some((
        "Mpeg2AudioFrame(layer={:?}, version={:?}, bitrate={}kbps, sr={}Hz, ch_mode={:?}, frame_len={})",
        &[
            "self.layer",
            "self.version",
            "self.bitrate_kbps",
            "self.sample_rate_hz",
            "self.channel_mode",
            "self.frame_length_bytes",
        ],
    ));

    vec![
        rational, color, ext, obu, misp, h264_sps, h264_pps, h264_slice, h264_sets, h265_ptl,
        h265_sps, h265_vps, h265_pps, h265_slice, h265_sets, h266_ptl, h266_sps, h266_vps,
        h266_pps, h266_slice, h266_sets, av1_seq, av1_frame, av1_stream, adts, mpeg,
    ]
}
