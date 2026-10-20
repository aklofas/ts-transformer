//! `org.tstrans.webrtc` — the WHEP playback server JNI surface: `WhepServer`
//! (`server.rs`) and `MountHandle` (`mount.rs`). Errors are
//! `org.tstrans.WebrtcException` through the shared kind table (`errors.rs`).

mod errors;
mod mount;
mod server;
