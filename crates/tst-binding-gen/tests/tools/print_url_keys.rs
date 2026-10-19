//! Maintainer tool: writes the URL key table in `docs/reference/conventions.md`
//! from the key tables that drive the eight URL parsers.
//!
//!   print-url-keys --list       the rendered block on stdout
//!   print-url-keys --write      rewrite the block in the document
//!   print-url-keys --check      exit 1 with a unified diff if the document's
//!                               block differs from a fresh render
//!   --root `<dir>`              the workspace root (default: the current directory)
//!
//! Not learner code. `scripts/check/repo/url-keys.sh` runs `--check`.

use std::path::PathBuf;

use tst_core::url::query::{KeyDoc, docs};

#[path = "print_url_keys/render.rs"]
mod render;

const DOC: &str = "docs/reference/conventions.md";

fn usage(msg: &str) -> ! {
    eprintln!("print-url-keys: {msg}");
    eprintln!("usage: print-url-keys [--list | --write | --check] [--root <dir>]");
    std::process::exit(2);
}

fn all_docs() -> Vec<KeyDoc> {
    let mut v: Vec<KeyDoc> = Vec::new();
    v.extend(docs("udp", tst_udp::url::URL_KEYS));
    v.extend(docs("rtp", tst_rtp::url::URL_KEYS_RTP));
    v.extend(docs("rtsp(s)", tst_rtp::url::URL_KEYS_RTSP));
    v.extend(docs("tcp(s)", tst_tcp::url::URL_KEYS));
    v.extend(docs("hls(s)", tst_hls::url::URL_KEYS));
    v.extend(docs("rist", tst_rist::url::URL_KEYS));
    v.extend(docs("whep(s)", tst_webrtc::url::URL_KEYS));
    v.extend(docs("srt", tst_srt::url::URL_KEYS));
    v.extend(tst_srt::url::URL_LEVEL_KEYS.iter().cloned());
    v
}

fn main() {
    let mut mode = "--list".to_string();
    let mut root = PathBuf::from(".");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" | "--write" | "--check" => mode = a,
            "--root" => match it.next() {
                Some(d) => root = PathBuf::from(d),
                None => usage("--root takes a directory"),
            },
            other => usage(&format!("unknown argument `{other}`")),
        }
    }
    let rows = render::group(all_docs());
    let block = render::render(&rows);
    if mode == "--list" {
        print!("{block}");
        return;
    }
    let path = root.join(DOC);
    let old = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("print-url-keys: {}: {e}", path.display());
        std::process::exit(2);
    });
    let Some(new) = render::splice(&old, &block) else {
        eprintln!(
            "print-url-keys: {} has no `{}` … `{}` block",
            path.display(),
            render::BEGIN,
            render::END
        );
        std::process::exit(1);
    };
    if mode == "--write" {
        if new != old {
            std::fs::write(&path, &new).unwrap_or_else(|e| {
                eprintln!("print-url-keys: {}: {e}", path.display());
                std::process::exit(2);
            });
        }
        println!("print-url-keys: {} key rows written", rows.len());
    } else if new == old {
        println!("print-url-keys: OK ({} key rows)", rows.len());
    } else {
        let tmp = std::env::temp_dir().join(format!("print-url-keys-{}.md", std::process::id()));
        std::fs::write(&tmp, &new).expect("write temp render");
        let _ = std::process::Command::new("diff")
            .args(["-u", "--label", DOC, "--label", "generated"])
            .arg(&path)
            .arg(&tmp)
            .status();
        let _ = std::fs::remove_file(&tmp);
        eprintln!("url-keys: the block in {DOC} differs from the key tables.");
        eprintln!("Regenerate: cargo run -p tst-binding-gen --bin print-url-keys -- --write");
        std::process::exit(1);
    }
}
