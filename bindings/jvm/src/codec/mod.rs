//! JVM `org.tstrans.codec` binding module.
//!
//! [`shared`] holds the value-type marshalling helpers (enum +
//! `Rational`/`ColorInfo` + `NalUnit`/`Obu` builders). The per-codec parser
//! JNI entry points (`parse_h264_sps`, …) reuse these helpers.

pub mod aac;
pub mod av1;
pub mod h264;
pub mod h265;
pub mod h266;
pub mod misp_time;
pub mod mpegaudio;
pub mod shared;
