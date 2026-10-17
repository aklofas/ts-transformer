//! The surface manifest's binding cells of a generated record: the
//! `python:` and `java:` entries of the `[[surface]]` row that is keyed by the
//! record's Rust path. A row for a method of the record keeps its own cells,
//! and a record with no row gets none.

use crate::schema::Entry;

/// Where the manifest lives, below the workspace root.
pub const PATH: &str = "tests/coverage/surface-manifest.toml";

/// One cell a record owns: the `[[surface]]` row's item, the column prefix and
/// the full cell text.
pub struct Cell {
    pub item: &'static str,
    pub prefix: &'static str,
    pub text: String,
}

/// The cells of every record, in registry order. A column the generator does
/// not write for a record (`python` or `jvm` false) is left to its hand cell.
pub fn cells(reg: &[Entry]) -> Vec<Cell> {
    let mut out = Vec::new();
    for e in reg {
        if e.python {
            out.push(Cell {
                item: e.rust_path,
                prefix: "python",
                text: format!("python:{}.{}", e.py_module, e.name()),
            });
        }
        if e.jvm {
            out.push(Cell {
                item: e.rust_path,
                prefix: "java",
                text: format!("java:{}.{}", e.java_pkg.replace('/', "."), e.name()),
            });
        }
    }
    out
}

/// `text` with each cell written into the row of its item. An item without a
/// row is skipped; a row without the cell's column is an error.
pub fn apply(text: &str, cells: &[Cell]) -> Result<String, String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    for c in cells {
        let key = format!("item = \"{}\"", c.item);
        let Some(at) = lines.iter().position(|l| l.trim_end() == key) else {
            continue;
        };
        let col = format!("\"{}:", c.prefix);
        let bindings = lines[at..]
            .iter()
            .take_while(|l| !l.starts_with("[["))
            .position(|l| l.starts_with("bindings = "))
            .map(|i| at + i)
            .ok_or_else(|| format!("the row of {} has no bindings array", c.item))?;
        let line = &lines[bindings];
        let Some(start) = line.find(&col) else {
            return Err(format!("the row of {} has no {} column", c.item, c.prefix));
        };
        let close = line[start + 1..]
            .find('"')
            .map(|i| start + 1 + i)
            .ok_or_else(|| format!("the {} cell of {} is unterminated", c.prefix, c.item))?;
        let new = format!("{}\"{}{}", &line[..start], c.text, &line[close..]);
        lines[bindings] = new;
    }
    Ok(lines.concat())
}
