//! Pure (socket-free) integration tests. Tests that open a socket live in
//! the `net_whep` binary.

#[path = "whep/sei.rs"]
mod sei;
#[path = "whep/smoke.rs"]
mod smoke;
#[path = "whep/url.rs"]
mod url;
