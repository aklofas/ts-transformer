//! The Java record: the block between two marker lines of a hand-owned file
//! under `bindings/jvm/src/main/java`. The block is the Javadoc, the record
//! header with its components and the `Builder`; it ends inside the record
//! body, so the hand-written package, imports, compact constructor, methods
//! and closing brace follow the end marker.

use crate::emit_jvm_rs::CTOR_MAX;
use crate::schema::{Entry, paras, unlink};
use crate::typemap::{Fld, resolve};

/// Javadoc text from a Rust doc paragraph. The text cannot close the
/// comment, start a unicode escape, open an HTML tag or entity, or start a
/// block tag. A backtick pair becomes `{@code …}` (its text as written, with
/// a Rust field name, with or without a `Self::` prefix, renamed to its Java name by `rename`), and an
/// intra-doc link `` [`X`] `` becomes `{@code X}`.
pub fn escape_javadoc(doc: &str, rename: &[(&str, &str)]) -> String {
    let doc = unlink(doc);
    let parts: Vec<&str> = doc.split('`').collect();
    let out = if parts.len() % 2 == 0 {
        html(&doc) // an unpaired backtick stays as written
    } else {
        let mut out = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i % 2 == 1 {
                // `Self::field` names the field too.
                let field = p.strip_prefix("Self::").unwrap_or(p);
                let code = rename
                    .iter()
                    .find(|(rust, _)| *rust == field)
                    .map_or(*p, |(_, java)| java);
                out.push_str(&format!("{{@code {code}}}"));
            } else {
                out.push_str(&html(p));
            }
        }
        out
    };
    out.replace("*/", "*&#47;").replace("\\u", "&#92;u")
}

/// Prose outside a code span: `&`, `<` and `>` as entities, and `@` too, so
/// no word reads as a block or inline tag.
fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('@', "&#64;")
}

/// The longest line a generated Javadoc writes.
const DOC_WIDTH: usize = 100;

/// `text` wrapped at word boundaries to [`DOC_WIDTH`]; the first line starts
/// with `first`, the others with `rest`.
fn wrap(first: &str, rest: &str, text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = first.to_string();
    let mut empty = true;
    for w in text.split_whitespace() {
        if !empty && cur.chars().count() + 1 + w.chars().count() > DOC_WIDTH {
            lines.push(std::mem::replace(&mut cur, rest.to_string()));
            empty = true;
        }
        if !empty {
            cur.push(' ');
        }
        cur.push_str(w);
        empty = false;
    }
    lines.push(cur);
    lines
}

fn javadoc(e: &Entry, flds: &[&Fld]) -> String {
    let rename: Vec<(&str, &str)> = flds
        .iter()
        .map(|f| (f.spec.name, f.java.as_str()))
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let doc = paras(e.schema.doc);
    for (i, p) in doc.iter().enumerate() {
        if i > 0 {
            lines.push(" *".into());
        }
        lines.extend(wrap(" * ", " * ", &escape_javadoc(p, &rename)));
    }
    let documented: Vec<(&&Fld, Vec<String>)> = flds
        .iter()
        .map(|f| (f, paras(f.spec.doc)))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    if !documented.is_empty() && !doc.is_empty() {
        lines.push(" *".into());
    }
    for (f, ps) in documented {
        for (i, p) in ps.iter().enumerate() {
            let text = escape_javadoc(p, &rename);
            if i == 0 {
                lines.extend(wrap(&format!(" * @param {} ", f.java), " *     ", &text));
            } else {
                lines.push(" *".into());
                lines.extend(wrap(" *     ", " *     ", &text));
            }
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut o = String::from("/**\n");
    for l in lines {
        o.push_str(&l);
        o.push('\n');
    }
    o.push_str(" */\n");
    o
}

/// The generated block of `<Name>.java`: Javadoc, the record header with its
/// components, and (past [`CTOR_MAX`] components) the `Builder`.
pub fn block(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let flds = resolve(reg, &e.schema)?;
    let cross: Vec<&Fld> = flds.iter().filter(|f| f.ty.is_some()).collect();
    let name = e.schema.name;
    let implements = if e.java_implements.is_empty() {
        String::new()
    } else {
        format!(" implements {}", e.java_implements)
    };
    let mut o = javadoc(e, &cross);
    let comps: Vec<String> = cross
        .iter()
        .map(|f| format!("{} {}", f.ty.as_ref().expect("crossing").java(), f.java))
        .collect();
    if cross.len() <= CTOR_MAX {
        o.push_str(&format!(
            "public record {name}({}){implements} {{\n",
            comps.join(", ")
        ));
    } else {
        o.push_str(&format!("public record {name}(\n"));
        for (i, c) in comps.iter().enumerate() {
            let sep = if i + 1 == comps.len() { "" } else { "," };
            o.push_str(&format!("        {c}{sep}\n"));
        }
        o.push_str(&format!("){implements} {{\n"));
        o.push_str("    /** Collects the components one named setter at a time. */\n");
        o.push_str("    public static final class Builder {\n");
        for (f, c) in cross.iter().zip(&comps) {
            match &f.java_default {
                Some(d) => o.push_str(&format!("        private {c} = {d};\n")),
                None => o.push_str(&format!("        private {c};\n")),
            }
        }
        for f in &cross {
            let ty = f.ty.as_ref().expect("crossing").setter_java();
            o.push_str(&format!(
                "\n        /** Sets {n}. */\n        public Builder {n}({ty} v) {{\n            this.{n} = v;\n            return this;\n        }}\n",
                n = f.java
            ));
        }
        let names: Vec<&str> = cross.iter().map(|f| f.java.as_str()).collect();
        o.push_str(&format!(
            "\n        /** Builds the record. */\n        public {name} build() {{\n            return new {name}({});\n        }}\n    }}\n",
            names.join(", ")
        ));
    }
    Ok(o)
}
