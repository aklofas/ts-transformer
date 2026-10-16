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
/// `rust` is the field type with every path prefix and the `Option<…>`
/// wrapper removed (`Vec<OwnedRawField>`, `UniversalLabel`).
struct CustomRow {
    rust: &'static str,
    /// The Python annotation, without `| None`.
    py: &'static str,
    java: &'static str,
    sig: &'static str,
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
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "OperationalMode",
        py: "OperationalMode | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "PlatformStatus",
        py: "PlatformStatus | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "SensorControlMode",
        py: "SensorControlMode | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_default: None,
        java_default: None,
        prim: None,
    },
    CustomRow {
        rust: "SensorFovName",
        py: "SensorFovName | int",
        java: "Integer",
        sig: "Ljava/lang/Integer;",
        py_default: None,
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
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<u32>",
        py: "tuple[int, ...]",
        java: "List<Long>",
        sig: "Ljava/util/List;",
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<KlvFieldError>",
        py: "tuple[KlvFieldError, ...]",
        java: "List<KlvFieldError>",
        sig: "Ljava/util/List;",
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
    CustomRow {
        rust: "Vec<(u32,ImapbSpecial)>",
        py: "tuple[tuple[int, str, int], ...]",
        java: "List<ImapbSpecialEntry>",
        sig: "Ljava/util/List;",
        py_default: Some("()"),
        java_default: Some(EMPTY_LIST),
        prim: None,
    },
];

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

    /// The narrowing helper in `crate::jutil`, when the Java value is wider.
    pub fn narrow(self) -> Option<&'static str> {
        match self {
            Sc::U8 => Some("checked_u8"),
            Sc::U16 => Some("checked_u16"),
            Sc::U32 => Some("checked_u32"),
            Sc::U64 => Some("checked_u64"),
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
    if names.contains(&s) {
        return Ok(Ty::Record(s.to_string()));
    }
    Err(format!("unmapped type `{s}`"))
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
        Ty::Opt(_) | Ty::Record(_) | Ty::Custom(_) => return None,
    })
}

/// Resolves every field of a record against the registry.
pub fn resolve<'a>(reg: &[Entry], schema: &'a RecordSchema) -> Result<Vec<Fld<'a>>, String> {
    let names: Vec<&str> = reg.iter().map(|e| e.schema.name).collect();
    let mut out = Vec::new();
    let mut defaulted: Option<&str> = None;
    for f in schema.fields {
        let ctx = |e: String| format!("{}.{}: {e}", schema.name, f.name);
        let (ty, custom) = match f.mark {
            FieldMark::Skip => (None, false),
            FieldMark::Custom => (Some(map_custom(f.ty).map_err(ctx)?), true),
            _ => (Some(map(f.ty, &names).map_err(ctx)?), false),
        };
        let mut py_default = None;
        let mut java_default = None;
        if let Some(Ty::Opt(inner)) = &ty {
            if inner.custom_prim().is_some() {
                return Err(ctx(
                    "a primitive custom type cannot be optional: a Java primitive has no null"
                        .into(),
                ));
            }
        }
        if let Some(t) = &ty {
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
