//! Socket tests for the `corpus` module family: the replay stage's child
//! processes, driven over their real stdin and stdout against loopback
//! sockets.

#[path = "corpus_support/mod.rs"]
mod support;
pub use support::{gen_profile_bytes, sha_of, temp_dir_named};

#[path = "net_corpus/receive.rs"]
mod receive;
