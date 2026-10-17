//! The native Python class: a frozen `#[pyclass(get_all)]` struct of
//! host-typed copies, built from the Rust record with `From<&Rust>`, as
//! committed Rust text for `bindings/python/src/generated/<family>.rs`, and
//! its `.pyi` class block.
//!
//! The class holds no Rust value: a getter hands out a copy of the host
//! field, as the hand-written getters did. A function that needs the Rust
//! record back (a parser taking its SPS) re-parses the raw bytes the class
//! carries.

use crate::emit_py::HEADER;
use crate::schema::{Entry, ExtraKind, PyExtra, lookup, paras, snake, unlink};
use crate::typemap::{Fld, Sc, Ty, resolve};

/// The longest line of a generated doc comment.
const DOC_WIDTH: usize = 76;

/// `doc` as `///` lines at `indent`: paragraphs wrapped at [`DOC_WIDTH`],
/// separated by an empty `///` line, Rust paths reduced to plain names.
fn doc_comment(doc: &str, indent: &str) -> String {
    let mut out = String::new();
    for (i, p) in paras(doc).iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("{indent}///\n"));
        }
        let text = unlink(p);
        let mut cur = String::new();
        for w in text.split_whitespace() {
            if !cur.is_empty()
                && indent.len() + 4 + cur.chars().count() + 1 + w.chars().count() > DOC_WIDTH
            {
                out.push_str(&format!("{indent}/// {cur}\n"));
                cur.clear();
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(w);
        }
        if !cur.is_empty() {
            out.push_str(&format!("{indent}/// {cur}\n"));
        }
    }
    out
}

/// The fields the class holds and exposes, in declaration order.
fn py_fields<'a>(flds: &'a [Fld<'a>]) -> Vec<&'a Fld<'a>> {
    flds.iter().filter(|f| f.ty.is_some()).collect()
}

/// Whether the generated struct of `e` is `Copy`.
fn is_copy(reg: &[Entry], e: &Entry) -> bool {
    let Ok(flds) = resolve(reg, &e.schema) else {
        return false;
    };
    py_fields(&flds).iter().all(|f| {
        f.ty.as_ref()
            .expect("crossing")
            .host_copy(&|n| lookup(reg, n).is_some_and(|d| d.is_native() && is_copy(reg, d)))
    })
}

/// Whether the struct can derive `Eq` (no float field).
fn is_eq(flds: &[&Fld]) -> bool {
    fn float(t: &Ty) -> bool {
        match t {
            Ty::Scalar(Sc::F32 | Sc::F64) => true,
            Ty::Opt(i) | Ty::List(i) | Ty::Map(i) => float(i),
            _ => false,
        }
    }
    !flds.iter().any(|f| float(f.ty.as_ref().expect("crossing")))
}

fn custom_path(e: &Entry, f: &Fld) -> String {
    format!(
        "crate::custom::{}_{}_to_py",
        snake(e.schema.name),
        f.spec.name
    )
}

fn extra(e: &Entry, x: &PyExtra) -> String {
    let hook = format!("crate::custom::{}_{}", snake(e.schema.name), x.name);
    let params = x
        .params
        .iter()
        .map(|(n, rs, _)| format!("{n}: {rs}"))
        .collect::<Vec<_>>()
        .join(", ");
    let args = x
        .params
        .iter()
        .map(|(n, _, _)| *n)
        .collect::<Vec<_>>()
        .join(", ");
    let (lead_params, lead_args) = if x.params.is_empty() {
        (String::new(), String::new())
    } else {
        (format!(", {params}"), format!(", {args}"))
    };
    let mut o = doc_comment(x.doc, "    ");
    match x.kind {
        ExtraKind::Method => {
            o.push_str(&format!(
                "    fn {}(&self{lead_params}) -> {} {{\n        {hook}(self{lead_args})\n    }}\n",
                x.name, x.ret_rs
            ));
        }
        ExtraKind::Static => {
            o.push_str(&format!(
                "    #[staticmethod]\n    fn {}({params}) -> {} {{\n        {hook}({args})\n    }}\n",
                x.name, x.ret_rs
            ));
        }
    }
    o
}

fn repr(e: &Entry, flds: &[&Fld]) -> String {
    let name = e.name();
    let (fmt, args): (String, Vec<String>) = match e.py_repr {
        Some((f, a)) => (f.to_string(), a.iter().map(|s| s.to_string()).collect()),
        None => {
            let mut parts = Vec::new();
            let mut args = Vec::new();
            for f in flds {
                let n = e.py_name(f.spec.name);
                if matches!(f.ty, Some(Ty::Bytes)) {
                    parts.push(format!("{n}=<{{}} bytes>"));
                    args.push(format!("self.{n}.0.len()"));
                } else {
                    parts.push(format!("{n}={{:?}}"));
                    args.push(format!("self.{n}"));
                }
            }
            (format!("{name}({})", parts.join(", ")), args)
        }
    };
    let mut o = String::from("    fn __repr__(&self) -> String {\n        format!(\n");
    o.push_str(&format!("            {fmt:?}"));
    for a in args {
        o.push_str(&format!(",\n            {a}"));
    }
    o.push_str("\n        )\n    }\n");
    o
}

fn ctor(e: &Entry, flds: &[&Fld]) -> String {
    let names: Vec<&str> = flds.iter().map(|f| e.py_name(f.spec.name)).collect();
    let params: Vec<String> = flds
        .iter()
        .map(|f| {
            let t = f.ty.as_ref().expect("crossing");
            let rs = if matches!(t, Ty::Bytes) {
                "Vec<u8>".to_string()
            } else {
                t.host_rs()
            };
            format!("{}: {rs}", e.py_name(f.spec.name))
        })
        .collect();
    let init: Vec<String> = flds
        .iter()
        .map(|f| {
            let n = e.py_name(f.spec.name);
            if matches!(f.ty, Some(Ty::Bytes)) {
                format!("{n}: crate::util::HostBytes({n})")
            } else {
                n.to_string()
            }
        })
        .collect();
    format!(
        "    #[new]\n    #[pyo3(signature = ({}))]\n    fn new({}) -> Self {{\n        Self {{ {} }}\n    }}\n",
        names.join(", "),
        params.join(", "),
        init.join(", ")
    )
}

/// The doc text that reaches a Python reader of a native class: the class
/// doc, then each field's and extra's, as the `///` lines read. The binary
/// never calls it; the doc rail of `print_records_emit` does.
#[allow(dead_code)]
pub fn docs(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let flds = resolve(reg, &e.schema)?;
    let mut text = doc_comment(e.schema.doc, "");
    for f in py_fields(&flds) {
        text.push_str(&doc_comment(f.spec.doc, "    "));
    }
    for x in e.py_extras {
        text.push_str(&doc_comment(x.doc, "    "));
    }
    Ok(text)
}

/// The struct, methods and conversion of one record.
pub fn class(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let s = &e.schema;
    let flds = resolve(reg, s)?;
    let cross = py_fields(&flds);
    for (f, _) in e.py_rename {
        if !s.fields.iter().any(|x| x.name == *f) {
            return Err(format!("{}: py_rename names no field `{f}`", s.name));
        }
    }
    let mut derives = vec!["Debug", "Clone"];
    if is_copy(reg, e) {
        derives.push("Copy");
    }
    if e.py_eq {
        derives.push("PartialEq");
        if is_eq(&cross) {
            derives.push("Eq");
        }
    }
    let name = e.name();
    let mut o = doc_comment(s.doc, "");
    let eq = if e.py_eq { ", eq" } else { "" };
    o.push_str(&format!(
        "#[pyclass(frozen, get_all{eq}, name = \"{name}\", module = \"{}\")]\n",
        e.py_module
    ));
    o.push_str(&format!("#[derive({})]\n", derives.join(", ")));
    o.push_str(&format!("pub struct {name} {{\n"));
    for f in &cross {
        o.push_str(&doc_comment(f.spec.doc, "    "));
        o.push_str(&format!(
            "    pub {}: {},\n",
            e.py_name(f.spec.name),
            f.ty.as_ref().expect("crossing").host_rs()
        ));
    }
    o.push_str("}\n\n");

    o.push_str("#[pymethods]\nimpl ");
    o.push_str(name);
    o.push_str(" {\n");
    let mut methods = Vec::new();
    if e.py_ctor {
        methods.push(ctor(e, &cross));
    }
    for x in e.py_extras {
        methods.push(extra(e, x));
    }
    methods.push(repr(e, &cross));
    o.push_str(&methods.join("\n"));
    o.push_str("}\n\n");

    o.push_str(&format!(
        "impl From<&{}> for {name} {{\n    fn from(r: &{}) -> Self {{\n        Self {{\n",
        e.rust_path, e.rust_path
    ));
    for f in &cross {
        let ty = f.ty.as_ref().expect("crossing");
        let custom = (f.custom && !ty.is_py_copy()).then(|| custom_path(e, f));
        o.push_str(&format!(
            "            {}: {},\n",
            e.py_name(f.spec.name),
            ty.to_host(&format!("&r.{}", f.spec.name), custom.as_deref())
        ));
    }
    o.push_str("        }\n    }\n}\n");
    Ok(o)
}

/// The whole `generated/<family>.rs` text of a family of native classes.
pub fn file(reg: &[Entry], family: &str) -> Result<String, String> {
    let mine: Vec<&Entry> = reg
        .iter()
        .filter(|e| e.family == family && e.python)
        .collect();
    if mine.iter().any(|e| !e.is_native()) {
        return Err(format!(
            "family `{family}` mixes native classes and dataclasses; give each its own family"
        ));
    }
    let mut out = String::from(HEADER);
    out.push_str("\n#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]\n\n");
    out.push_str("use pyo3::prelude::*;\n\n");
    for e in &mine {
        out.push_str(&class(reg, e)?);
        out.push('\n');
    }
    out.push_str(
        "/// Adds every generated class to `m`.\npub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {\n",
    );
    for e in &mine {
        out.push_str(&format!("    m.add_class::<{}>()?;\n", e.name()));
    }
    out.push_str("    Ok(())\n}\n");
    Ok(out)
}

/// The block between the `.pyi` markers: the class header and its members,
/// ending inside the class body.
pub fn pyi_block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let flds = resolve(reg, &e.schema)?;
    let cross = py_fields(&flds);
    let name = e.name();
    let mut o = format!("@final\nclass {name}:\n");
    if e.py_ctor {
        let params: Vec<String> = cross
            .iter()
            .map(|f| {
                let t = f.ty.as_ref().expect("crossing");
                let ann = if matches!(t, Ty::Bytes) {
                    "_BytesLike".to_string()
                } else {
                    t.py_ann_native()
                };
                format!("{}: {ann}", e.py_name(f.spec.name))
            })
            .collect();
        if params.is_empty() {
            o.push_str(&format!("    def __new__(cls) -> {name}: ...\n"));
        } else {
            o.push_str(&format!(
                "    def __new__(cls, {}) -> {name}: ...\n",
                params.join(", ")
            ));
        }
    }
    for f in &cross {
        o.push_str(&format!(
            "    @property\n    def {}(self) -> {}: ...\n",
            e.py_name(f.spec.name),
            f.ty.as_ref().expect("crossing").py_ann_native()
        ));
    }
    if e.py_eq {
        o.push_str("    def __eq__(self, other: object, /) -> bool: ...\n");
    }
    for x in e.py_extras {
        let params = x
            .params
            .iter()
            .map(|(n, _, py)| format!("{n}: {py}"))
            .collect::<Vec<_>>()
            .join(", ");
        let lead = if x.params.is_empty() {
            String::new()
        } else {
            format!(", {params}")
        };
        match x.kind {
            ExtraKind::Method => o.push_str(&format!(
                "    def {}(self{lead}) -> {}: ...\n",
                x.name, x.ret_py
            )),
            ExtraKind::Static => o.push_str(&format!(
                "    @staticmethod\n    def {}({}) -> {}: ...\n",
                x.name, params, x.ret_py
            )),
        }
    }
    Ok(o)
}
