//! Test fixtures shared across `tst-rtp` integration tests.

#![allow(dead_code)] // not every test uses every helper

pub mod h264_payloader;
pub mod raw_rtsp;
pub mod raw_rtsp_publisher;
pub mod rtsp_loopback_server;

#[cfg(feature = "tls")]
pub mod tls_certs;
