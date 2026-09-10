//! Domain harness: SRT end-to-end loopback transfer, handshake, encryption, and timeouts
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `loopback::<file>::` prefix.

// Shared loopback helpers + the `require_loopback!` macro, declared once at
// the binary root so `crate::common::*` and the macro resolve for every member.
#[macro_use]
#[path = "common/mod.rs"]
mod common;
#[path = "loopback/cancellation_loopback.rs"]
mod cancellation_loopback;
#[path = "loopback/connect_timeout.rs"]
mod connect_timeout;
#[path = "loopback/encrypted_packet_filter.rs"]
mod encrypted_packet_filter;
#[path = "loopback/getaddrinfo_walk.rs"]
mod getaddrinfo_walk;
#[path = "loopback/handshake.rs"]
mod handshake;
#[path = "loopback/ipv6_loopback.rs"]
mod ipv6_loopback;
#[path = "loopback/listener_accept_one_cancellable.rs"]
mod listener_accept_one_cancellable;
#[path = "loopback/listener_accept_timeout.rs"]
mod listener_accept_timeout;
#[path = "loopback/listener_cancel.rs"]
mod listener_cancel;
#[path = "loopback/maxbw_roundtrip.rs"]
mod maxbw_roundtrip;
#[path = "loopback/payload_limit.rs"]
mod payload_limit;
#[path = "loopback/srto_sender.rs"]
mod srto_sender;
