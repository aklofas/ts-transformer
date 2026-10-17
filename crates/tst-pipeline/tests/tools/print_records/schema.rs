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
    /// The path a binding crate names the record by, e.g.
    /// `tst_core::klv::st0601::WavelengthRecord`. Explicit because the
    /// declaring module is often private and re-exported higher up.
    pub rust_path: &'static str,
    /// The interface the Java record implements, `""` for none.
    pub java_implements: &'static str,
    pub schema: RecordSchema,
}

/// One record of a family: both directions, the `tstrans.klv` module and the
/// `org.tstrans.klv` package.
fn klv(
    family: &'static str,
    rust_path: &'static str,
    java_implements: &'static str,
    schema: RecordSchema,
) -> Entry {
    Entry {
        family,
        py_module: "tstrans.klv",
        java_pkg: "org/tstrans/klv",
        direction: Direction::Both,
        jvm: true,
        rust_path,
        java_implements,
        schema,
    }
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
    // The list items and nested records come first: a record is registered
    // before the records that hold it.
    vec![
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
    ]
}

/// A reference-style link definition line, `` [`x`]: crate::path::x ``: it
/// names a Rust item and says nothing to a host reader.
fn is_link_definition(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.contains("]: ")
}

/// The paragraphs of a joined doc text: runs of lines between empty ones,
/// each line without the space a `///` comment leaves, joined by one space.
/// Link definition lines are dropped.
pub fn paras(doc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in doc
        .lines()
        .map(|l| l.strip_prefix(' ').unwrap_or(l))
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
            let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
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
