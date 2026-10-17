//! Builder for [`crate::HlsPublisher`].

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use crate::config::{HlsConfig, HlsMode};
use crate::error::HlsError;
use crate::publisher::HlsPublisher;
#[cfg(feature = "serve")]
use crate::url::HlsUrl;
#[cfg(feature = "serve")]
use tst_core::url::UrlError;

/// Builder for [`HlsPublisher`].
#[must_use]
#[derive(Debug, Clone)]
pub struct HlsPublisherBuilder {
    config: HlsConfig,
}

impl HlsPublisherBuilder {
    /// Start an empty builder using [`HlsConfig::default`].
    pub fn new() -> Self {
        Self {
            config: HlsConfig::default(),
        }
    }

    /// Parse an `hls://` or `hlss://` URL and seed the config from it.
    #[cfg(feature = "serve")]
    pub fn from_url(url: &str) -> Result<Self, UrlError> {
        Ok(Self {
            config: HlsUrl::parse(url)?.config,
        })
    }

    /// HTTP server bind address.
    pub fn bind(&mut self, addr: SocketAddr) -> &mut Self {
        self.config.bind = addr;
        self
    }

    /// Filesystem directory for `.ts` segments + `playlist.m3u8`.
    pub fn output_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.config.output_dir = dir.as_ref().to_path_buf();
        self
    }

    /// Target segment duration.
    pub fn segment_duration(&mut self, d: Duration) -> &mut Self {
        self.config.segment_duration = d;
        self
    }

    /// Hard upper bound on an open segment's wall-clock age in the
    /// keyframe-driven flow (force-cuts when a keyframe is overdue). Defaults
    /// to `2 × segment_duration`; must be `≥ segment_duration`.
    pub fn max_segment_duration(&mut self, d: Duration) -> &mut Self {
        self.config.max_segment_duration = Some(d);
        self
    }

    /// Rolling window size in LIVE mode.
    pub fn playlist_window(&mut self, n: usize) -> &mut Self {
        self.config.playlist_window = n;
        self
    }

    /// Playlist mode (LIVE / EVENT / VOD).
    pub fn mode(&mut self, mode: HlsMode) -> &mut Self {
        self.config.mode = mode;
        self
    }

    /// Enable HTTP Basic auth.
    pub fn basic_auth<S: Into<String>>(&mut self, user: S, pass: S) -> &mut Self {
        self.config.basic_auth = Some((user.into(), pass.into()));
        self
    }

    /// Enable HTTPS by supplying cert + key paths (PEM).  Requires the `tls`
    /// cargo feature; without it [`HlsPublisherBuilder::build`] returns
    /// [`HlsError::TlsDisabled`].
    pub fn enable_tls<P: AsRef<Path>>(&mut self, cert: P, key: P) -> &mut Self {
        self.config.tls_cert = Some(cert.as_ref().to_path_buf());
        self.config.tls_key = Some(key.as_ref().to_path_buf());
        self
    }

    /// Construct the publisher (binds the HTTP server immediately).
    pub fn build(&self) -> Result<HlsPublisher, HlsError> {
        #[cfg(not(feature = "tls"))]
        if self.config.tls_cert.is_some() || self.config.tls_key.is_some() {
            return Err(HlsError::TlsDisabled);
        }
        HlsPublisher::with_config(self.config.clone())
    }
}

impl Default for HlsPublisherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "serve")]
    use tst_core::publisher::Publisher;

    #[test]
    fn fluent_chain_compiles() {
        let dir = std::env::temp_dir().join(format!("hls-builder-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _b = HlsPublisherBuilder::new()
            .bind("127.0.0.1:0".parse().unwrap())
            .output_dir(&dir)
            .segment_duration(Duration::from_secs(2))
            .playlist_window(3)
            .mode(HlsMode::Event)
            .basic_auth("u", "p");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn from_url_seeds_config() {
        let b = HlsPublisherBuilder::from_url("hls://127.0.0.1:9100?mode=vod&playlist_window=10")
            .unwrap();
        assert_eq!(b.config.mode, HlsMode::Vod);
        assert_eq!(b.config.playlist_window, 10);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn builder_build_smoke() {
        let dir = std::env::temp_dir().join(format!(
            "hls-builder-smoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let p = HlsPublisherBuilder::new()
            .bind("127.0.0.1:0".parse().unwrap())
            .output_dir(&dir)
            .build()
            .unwrap();
        let addr = p.local_addr().unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        p.finish().unwrap();
    }
}
