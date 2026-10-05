//! Typed value enums for ST 0903 LS tags.
//!
//! Reserved for future enum-shaped tags. ST 0903.6 §6 Tables 1 + 2 — the
//! slice this crate types — contain no fields with constrained codepoint
//! ranges. The deferred typed nested LSes (`VObject` carries an
//! ontology-class reference, `Algorithm` carries a class enum, `VTracker`
//! carries a status enum) will populate this file when those typed
//! layers are added.
//!
//! Kept as a separate file so the typed enum surface lives in a
//! predictable location across all `klv::stXXXX` modules (see the
//! `klv::st0102::enums` precedent).
