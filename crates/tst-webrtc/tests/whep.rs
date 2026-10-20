//! Pure (socket-free) integration tests. Tests that open a socket live in
//! the `net_whep` binary.

#[path = "whep/binding.rs"]
mod binding;
#[path = "whep/clock.rs"]
mod clock;
#[path = "whep/http.rs"]
mod http;
#[path = "whep/mp2t.rs"]
mod mp2t;
#[path = "whep/negotiate.rs"]
mod negotiate;
#[path = "whep/packetize_h264.rs"]
mod packetize_h264;
#[path = "whep/packetize_h265.rs"]
mod packetize_h265;
#[path = "whep/sei.rs"]
mod sei;
#[path = "whep/smoke.rs"]
mod smoke;
#[path = "whep/url.rs"]
mod url;
