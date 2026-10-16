//! The registry: the one hand-maintained list of records the generator
//! renders, and the small naming helpers every emitter shares.

use tst_core::binding_schema::{FieldMark, FieldSpec, RecordSchema};

/// Which way a record crosses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Rust to host and host to Rust.
    Both,
    /// Rust to host only: the builder and the Python `_to_py` converter.
    #[allow(dead_code)] // no registered record is one-way yet; the emitter tests build them
    ToHost,
    /// Host to Rust only: the reader and the Python `_from_py` converter.
    #[allow(dead_code)] // as ToHost
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
    pub schema: RecordSchema,
}

/// Every generated record. A record declared with `binding_record!` and
/// absent here is caught by the registry-completeness test.
pub fn records() -> Vec<Entry> {
    use tst_core::klv::st0601::WavelengthRecord;
    vec![Entry {
        family: "st0601",
        py_module: "tstrans.klv",
        java_pkg: "org/tstrans/klv",
        direction: Direction::Both,
        schema: WavelengthRecord::BINDING_SCHEMA,
    }]
}

/// Module segments that are private in the declaring crate; the record is
/// re-exported one level up.
const PRIVATE_SEGMENTS: &[&str] = &["packs"];

/// The path a binding crate can name the record by.
pub fn public_path(schema: &RecordSchema) -> String {
    schema
        .rust_path
        .split("::")
        .filter(|s| !PRIVATE_SEGMENTS.contains(s))
        .collect::<Vec<_>>()
        .join("::")
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
