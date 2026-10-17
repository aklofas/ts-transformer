//! The dataclass text: the `.py` definition and its `.pyi` stub.

use crate::schema::{Entry, paras, unlink};
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

/// A backtick pair as the RST double backticks of the hand docstrings; an
/// unpaired backtick stays as written.
pub fn rst_code(doc: &str) -> String {
    let parts: Vec<&str> = doc.split('`').collect();
    if parts.len() % 2 == 0 {
        return doc.to_string();
    }
    parts.join("``")
}

/// The longest line a generated `.py` docstring writes.
const DOC_WIDTH: usize = 79;

/// A class docstring at four spaces: one line when it fits in
/// [`DOC_WIDTH`], else wrapped at word boundaries with the closing quotes
/// on the last text line, as the hand-written docstrings are. Paragraphs are
/// separated by one empty line, except that consecutive `- ` bullets sit on
/// adjacent lines and wrap under their text.
pub fn docstring_paras(paras: &[String]) -> String {
    // Every line leaves room for the closing quotes.
    let width = DOC_WIDTH - 3;
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::from("    \"\"\"");
    let mut empty = true;
    let mut prev_bullet = false;
    for (i, text) in paras.iter().enumerate() {
        let bullet = text.starts_with("- ");
        let hang = if bullet { "      " } else { "    " };
        if i > 0 {
            lines.push(std::mem::replace(&mut cur, String::from("    ")));
            if !(bullet && prev_bullet) {
                lines.push(String::new());
            }
            empty = true;
        }
        prev_bullet = bullet;
        for w in text.split_whitespace() {
            if !empty && cur.chars().count() + 1 + w.chars().count() > width {
                lines.push(std::mem::replace(&mut cur, String::from(hang)));
                empty = true;
            }
            if !empty {
                cur.push(' ');
            }
            cur.push_str(w);
            empty = false;
        }
    }
    cur.push_str("\"\"\"");
    lines.push(cur);
    lines.join("\n") + "\n"
}

/// The docstring paragraphs of a record: its own doc, then one bullet per
/// documented crossing field.
pub fn doc_paragraphs(reg: &[Entry], e: &Entry) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = paras(e.schema.doc)
        .iter()
        .map(|p| escape_docstring(&rst_code(&unlink(p))))
        .collect();
    for f in resolve(reg, &e.schema)?.iter().filter(|f| f.ty.is_some()) {
        let text = paras(f.spec.doc).join(" ");
        if !text.is_empty() {
            out.push(format!(
                "- ``{}`` \u{2014} {}",
                f.spec.name,
                escape_docstring(&rst_code(&unlink(&text)))
            ));
        }
    }
    Ok(out)
}

fn fields(reg: &[Entry], e: &Entry, stub: bool) -> Result<String, String> {
    let mut o = String::new();
    for f in resolve(reg, &e.schema)?.iter().filter(|f| f.ty.is_some()) {
        let ty = f.ty.as_ref().expect("crossing");
        let ann = ty.py_ann();
        let name = f.spec.name;
        let opt = ty.is_opt();
        match (&f.py_default, stub) {
            (Some(_), true) if opt => o.push_str(&format!("    {name}: Optional[{ann}] = ...\n")),
            (Some(_), true) => o.push_str(&format!("    {name}: {ann} = ...\n")),
            (Some(d), false) if opt => o.push_str(&format!("    {name}: {ann} | None = {d}\n")),
            (Some(d), false) => o.push_str(&format!("    {name}: {ann} = {d}\n")),
            (None, _) => o.push_str(&format!("    {name}: {ann}\n")),
        }
    }
    Ok(o)
}

/// The block between the `.py` markers.
pub fn py_block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let mut o = String::from("@dataclass(frozen=True, slots=True)\n");
    o.push_str(&format!("class {}:\n", e.name()));
    let doc = doc_paragraphs(reg, e)?;
    if !doc.is_empty() {
        o.push_str(&docstring_paras(&doc));
        o.push('\n');
    }
    let body = fields(reg, e, false)?;
    o.push_str(if body.is_empty() { "    pass\n" } else { &body });
    Ok(o)
}

/// The block between the `.pyi` markers.
pub fn pyi_block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let mut o = String::from("@dataclass(frozen=True, slots=True)\n");
    o.push_str(&format!("class {}:\n", e.name()));
    let body = fields(reg, e, true)?;
    o.push_str(if body.is_empty() { "    pass\n" } else { &body });
    Ok(o)
}
