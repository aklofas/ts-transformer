//! The dataclass text: the `.py` definition and its `.pyi` stub.

use crate::schema::Entry;
use crate::typemap::resolve;

/// A docstring body: backslashes and triple quotes cannot end the string early.
pub fn escape_docstring(doc: &str) -> String {
    let mut s = doc.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"");
    if s.ends_with('"') {
        s.pop();
        s.push_str("\\\"");
    }
    s
}

fn fields(reg: &[Entry], e: &Entry, stub: bool) -> Result<String, String> {
    let mut o = String::new();
    for f in resolve(reg, &e.schema)?.iter().filter(|f| f.ty.is_some()) {
        let ty = f.ty.as_ref().expect("crossing");
        let ann = ty.py_ann();
        let name = f.spec.name;
        match (ty.is_opt(), stub) {
            (true, true) => o.push_str(&format!("    {name}: Optional[{ann}] = ...\n")),
            (true, false) => o.push_str(&format!("    {name}: {ann} | None = None\n")),
            (false, _) => o.push_str(&format!("    {name}: {ann}\n")),
        }
    }
    Ok(o)
}

/// The block between the `.py` markers.
pub fn py_block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let s = &e.schema;
    let mut o = String::from("@dataclass(frozen=True, slots=True)\n");
    o.push_str(&format!("class {}:\n", s.name));
    if !s.doc.is_empty() {
        o.push_str(&format!("    \"\"\"{}\"\"\"\n\n", escape_docstring(s.doc)));
    }
    let body = fields(reg, e, false)?;
    o.push_str(if body.is_empty() { "    pass\n" } else { &body });
    Ok(o)
}

/// The block between the `.pyi` markers.
pub fn pyi_block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let mut o = String::from("@dataclass(frozen=True, slots=True)\n");
    o.push_str(&format!("class {}:\n", e.schema.name));
    let body = fields(reg, e, true)?;
    o.push_str(if body.is_empty() { "    pass\n" } else { &body });
    Ok(o)
}
