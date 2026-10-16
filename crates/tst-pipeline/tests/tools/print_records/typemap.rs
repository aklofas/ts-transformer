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
}

/// A host-side type for a `@[custom]` field: the generator never converts
/// it, it only names the host type in the annotation and the signature.
struct CustomRow {
    rust: &'static str,
    py: &'static str,
    java: &'static str,
    sig: &'static str,
}

const CUSTOM: &[CustomRow] = &[CustomRow {
    rust: "UniversalLabel",
    py: "bytes",
    java: "ByteBuffer",
    sig: "Ljava/nio/ByteBuffer;",
}];

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Scalar(Sc),
    Str,
    Bytes,
    Opt(Box<Ty>),
    List(Box<Ty>),
    Record(String),
    Custom(&'static str),
}

/// What one scalar needs on each side.
pub struct ScInfo {
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
        let (_, py, java, boxed, sig, jvalue, extract, unbox) = match self {
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
            Sc::I8 => ("i8", "int", "byte", "Byte", 'B', "Byte", 'b', "byteValue"),
            Sc::I16 => (
                "i16",
                "int",
                "short",
                "Short",
                'S',
                "Short",
                's',
                "shortValue",
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
        };
        ScInfo {
            py,
            java,
            boxed,
            sig,
            jvalue,
            extract,
            unbox,
        }
    }

    /// The unsigned narrowing helper in `crate::jutil`, when the Java value is wider.
    pub fn narrow(self) -> Option<&'static str> {
        match self {
            Sc::U8 => Some("checked_u8"),
            Sc::U16 => Some("checked_u16"),
            Sc::U32 => Some("checked_u32"),
            Sc::U64 => Some("checked_u64"),
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
];

const PATH_PREFIXES: &[&str] = &[
    "std::string::",
    "alloc::string::",
    "std::vec::",
    "alloc::vec::",
    "core::option::",
    "std::option::",
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
    if s == "Vec<u8>" {
        return Ok(Ty::Bytes);
    }
    if let Some(inner) = s.strip_prefix("Vec<").and_then(|r| r.strip_suffix('>')) {
        let t = map_norm(inner, names)?;
        if matches!(t, Ty::Opt(_)) {
            return Err("Vec<Option<T>> is not mapped".into());
        }
        return Ok(Ty::List(Box::new(t)));
    }
    if s == "String" {
        return Ok(Ty::Str);
    }
    if let Some((_, sc)) = SCALARS.iter().find(|(n, _)| *n == s) {
        return Ok(Ty::Scalar(*sc));
    }
    if names.contains(&s) {
        return Ok(Ty::Record(s.to_string()));
    }
    Err(format!("unmapped type `{s}`"))
}

/// Resolves a `@[custom]` field's host type from the custom table.
fn map_custom(spelling: &str) -> Result<Ty, String> {
    let mut s: String = spelling.chars().filter(|c| !c.is_whitespace()).collect();
    let mut opt = false;
    if let Some(inner) = s.strip_prefix("Option<").and_then(|r| r.strip_suffix('>')) {
        opt = true;
        s = inner.to_string();
    }
    let last = s.rsplit("::").next().unwrap_or(&s).to_string();
    let row = CUSTOM
        .iter()
        .find(|r| r.rust == last)
        .ok_or_else(|| format!("no host type for custom type `{spelling}`; add a CUSTOM row"))?;
    let t = Ty::Custom(row.rust);
    Ok(if opt { Ty::Opt(Box::new(t)) } else { t })
}

fn custom_row(name: &str) -> &'static CustomRow {
    CUSTOM.iter().find(|r| r.rust == name).expect("custom row")
}

impl Ty {
    pub fn is_opt(&self) -> bool {
        matches!(self, Ty::Opt(_))
    }

    /// The Python annotation (without the `| None`).
    pub fn py_ann(&self) -> String {
        match self {
            Ty::Scalar(s) => s.info().py.into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Opt(t) => t.py_ann(),
            Ty::List(t) => format!("tuple[{}, ...]", t.py_ann()),
            Ty::Record(n) => n.clone(),
            Ty::Custom(n) => custom_row(n).py.into(),
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
            Ty::Record(n) => n.clone(),
            Ty::Custom(n) => custom_row(n).java.into(),
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
            Ty::Record(n) => {
                let pkg = lookup(reg, n).map(|e| e.java_pkg).unwrap_or("");
                format!("L{pkg}/{n};")
            }
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
}

/// Resolves every field of a record against the registry.
pub fn resolve<'a>(reg: &[Entry], schema: &'a RecordSchema) -> Result<Vec<Fld<'a>>, String> {
    let names: Vec<&str> = reg.iter().map(|e| e.schema.name).collect();
    let mut out = Vec::new();
    let mut seen_opt: Option<&str> = None;
    for f in schema.fields {
        let ctx = |e: String| format!("{}.{}: {e}", schema.name, f.name);
        let (ty, custom) = match f.mark {
            FieldMark::Skip => (None, false),
            FieldMark::Custom => (Some(map_custom(f.ty).map_err(ctx)?), true),
            _ => (Some(map(f.ty, &names).map_err(ctx)?), false),
        };
        if let Some(t) = &ty {
            if t.is_opt() {
                seen_opt = Some(f.name);
            } else if let Some(o) = seen_opt {
                return Err(format!(
                    "{}.{}: a required field follows the optional `{o}`; declare optional fields last",
                    schema.name, f.name
                ));
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
        });
    }
    debug_assert_eq!(
        out.iter().filter(|f| f.ty.is_some()).count(),
        crossing(schema).count()
    );
    Ok(out)
}
