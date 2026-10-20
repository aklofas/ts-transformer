//! Tests that open sockets: a real server on loopback, played by str0m
//! viewers over WHEP. Socket-free tests live in the `whep` binary.

#[path = "fixtures/mod.rs"]
mod fixtures;
#[path = "net_whep/audio.rs"]
mod audio;
#[path = "net_whep/loopback.rs"]
mod loopback;
#[path = "net_whep/shutdown.rs"]
mod shutdown;
#[cfg(feature = "tls")]
#[path = "net_whep/tls.rs"]
mod tls;
