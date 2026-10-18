//! The one reader for the soak and stress harnesses' comma-separated
//! sample files (`rss.csv`, `proc.csv`, `host.csv`): check the header,
//! count the columns, and name the offending line. Callers parse the
//! fields.

/// Header of `rss.csv`, shared by the soak and stress parsers.
pub(super) const RSS_HEADER: &str = "elapsed_s,leg,process,pid,rss_kb";

/// Split `text` into `(1-based line number, fields)` rows after checking
/// the header line and that every non-blank row has exactly `columns`
/// comma-separated fields. Errors name `file` and the line.
pub(super) fn csv_rows<'a>(
    text: &'a str,
    header: &str,
    columns: usize,
    file: &str,
) -> Result<Vec<(usize, Vec<&'a str>)>, String> {
    let mut lines = text.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(format!("{file}: empty file (expected a header line)"));
    };
    if first.trim() != header {
        return Err(format!(
            "{file} line 1: expected header `{header}`, got: {first:?}"
        ));
    }
    let mut rows = Vec::new();
    for (idx, raw) in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != columns {
            return Err(format!(
                "{file} line {}: expected {columns} fields, got {}: {line:?}",
                idx + 1,
                fields.len()
            ));
        }
        rows.push((idx + 1, fields));
    }
    Ok(rows)
}
