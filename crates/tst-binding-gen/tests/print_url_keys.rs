//! Emitter tests for `print-url-keys`: grouping, Markdown escaping, and the
//! block splice.

#![allow(dead_code)]

#[path = "tools/print_url_keys/render.rs"]
mod render;

use tst_core::url::query::KeyDoc;

fn doc(scheme: &'static str, canonical: &'static str, meaning: &'static str) -> KeyDoc {
    KeyDoc {
        scheme,
        canonical,
        aliases: &[],
        meaning,
        unit: "bytes",
    }
}

#[test]
fn a_pipe_in_a_meaning_is_escaped_and_a_backtick_is_kept() {
    let rows = render::group([doc("udp", "k", "either `a` | `b`")]);
    let out = render::render(&rows);
    assert!(
        out.contains("| `k` | either `a` \\| `b` | bytes | `udp` |"),
        "{out}"
    );
}

#[test]
fn equal_rows_merge_their_schemes_and_differing_rows_stay_apart() {
    let rows = render::group([
        doc("tcp(s)", "rcvbuf", "Buffer"),
        doc("udp", "rcvbuf", "Buffer"),
        doc("rtp", "rcvbuf", "Receive buffer"),
    ]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].schemes, ["udp", "tcp(s)"]);
    assert_eq!(rows[1].schemes, ["rtp"]);
}

#[test]
fn rows_sort_by_first_scheme_then_key() {
    let rows = render::group([
        doc("srt", "a", "m"),
        doc("udp", "z", "m"),
        doc("udp", "b", "m"),
    ]);
    let keys: Vec<_> = rows.iter().map(|r| r.canonical).collect();
    assert_eq!(keys, ["b", "z", "a"]);
}

#[test]
fn aliases_follow_the_canonical_key() {
    let d = KeyDoc {
        scheme: "srt",
        canonical: "fc",
        aliases: &["ffs"],
        meaning: "m",
        unit: "u",
    };
    let out = render::render(&render::group([d]));
    assert!(
        out.contains("| `fc` (also `ffs`) | m | u | `srt` |"),
        "{out}"
    );
}

#[test]
fn splice_replaces_only_the_marked_block() {
    let doc = format!(
        "before\n\n{}\nold\n{}\n\nafter\n",
        render::BEGIN,
        render::END
    );
    let block = format!("{}\nnew\n{}\n", render::BEGIN, render::END);
    let out = render::splice(&doc, &block).unwrap();
    assert_eq!(out, format!("before\n\n{block}\nafter\n"));
    assert_eq!(render::splice(&out, &block).unwrap(), out);
    assert!(render::splice("no markers", &block).is_none());
}
