//! [`HlsPublisher`] — implements [`tst_core::publisher::Publisher`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tst_core::publisher::{Publisher, PublisherStats};

use crate::config::HlsConfig;
use crate::error::HlsError;
use crate::playlist;
use crate::segmenter::Segmenter;
use crate::stats::HlsStats;

/// HLS publisher.
pub struct HlsPublisher {
    pub(crate) state: Arc<Mutex<State>>,
    pub(crate) finished: bool,
    #[cfg(feature = "serve")]
    pub(crate) server: Option<crate::http_server::ServerHandle>,
}

pub(crate) struct State {
    pub(crate) segmenter: Segmenter,
    pub(crate) bytes_pushed_total: u64,
    /// Set to `true` once the publisher is finished so the HTTP server renders
    /// a terminal playlist (with `#EXT-X-ENDLIST`).
    pub(crate) finished: bool,
}

impl HlsPublisher {
    pub fn with_config(config: HlsConfig) -> Result<Self, HlsError> {
        if let Some(msg) = config.validate() {
            return Err(HlsError::InvalidConfig(msg));
        }
        #[cfg(feature = "serve")]
        let bind = config.bind;
        #[cfg(feature = "serve")]
        let basic_auth = config.basic_auth.clone();

        #[cfg(feature = "tls")]
        let tls_config = match (&config.tls_cert, &config.tls_key) {
            (Some(cert), Some(key)) => Some(crate::tls::load_server_config(cert, key)?),
            _ => None,
        };
        #[cfg(not(feature = "tls"))]
        if config.tls_cert.is_some() || config.tls_key.is_some() {
            return Err(HlsError::TlsDisabled);
        }

        let segmenter = Segmenter::new(config)?;
        let state = Arc::new(Mutex::new(State {
            segmenter,
            bytes_pushed_total: 0,
            finished: false,
        }));

        #[cfg(feature = "serve")]
        let server = crate::http_server::ServerHandle::start(
            state.clone(),
            bind,
            basic_auth,
            #[cfg(feature = "tls")]
            tls_config,
        )?;

        Ok(Self {
            state,
            finished: false,
            #[cfg(feature = "serve")]
            server: Some(server),
        })
    }

    /// Local socket address the HTTP server bound to.
    /// Returns `None` when the `serve` feature is disabled.
    #[cfg(feature = "serve")]
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().map(|s| s.local_addr())
    }

    /// Snapshot of richer per-impl stats.
    pub fn hls_stats(&self) -> HlsStats {
        let s = self.state.lock().expect("HlsPublisher poisoned");
        HlsStats {
            segments_written: s.segmenter.segments_written(),
            bytes_pushed_total: s.bytes_pushed_total,
            open_segment_bytes: s.segmenter.open_segment_bytes(),
            forced_cuts: s.segmenter.forced_cuts(),
        }
    }

    /// Manually render the current playlist (for tests / introspection).
    pub fn render_playlist(&self, is_final: bool) -> String {
        let s = self.state.lock().expect("HlsPublisher poisoned");
        playlist::render(&s.segmenter, is_final)
    }
}

impl Publisher for HlsPublisher {
    type Error = HlsError;

    fn push_ts(&mut self, ts_bytes: &[u8]) -> Result<(), HlsError> {
        if self.finished {
            return Err(HlsError::Finished);
        }
        if ts_bytes.len() % 188 != 0 {
            return Err(HlsError::UnalignedPushTs {
                len: ts_bytes.len(),
            });
        }
        let mut s = self.state.lock().expect("HlsPublisher poisoned");
        s.segmenter.tick()?;
        s.segmenter.push_ts(ts_bytes)?;
        s.bytes_pushed_total = s.bytes_pushed_total.saturating_add(ts_bytes.len() as u64);
        Ok(())
    }

    fn cut_segment(&mut self) -> Result<(), HlsError> {
        if self.finished {
            return Err(HlsError::Finished);
        }
        let mut s = self.state.lock().expect("HlsPublisher poisoned");
        s.segmenter.note_explicit_cut();
        s.segmenter.cut()
    }

    fn cut_segment_with_duration(&mut self, media_duration: Duration) -> Result<(), HlsError> {
        if self.finished {
            return Err(HlsError::Finished);
        }
        let mut s = self.state.lock().expect("HlsPublisher poisoned");
        s.segmenter.note_explicit_cut();
        s.segmenter.cut_with_duration(Some(media_duration))
    }

    fn finish(mut self) -> Result<(), HlsError> {
        if self.finished {
            return Err(HlsError::Finished);
        }
        self.finalize_and_write()?;
        #[cfg(feature = "serve")]
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
        Ok(())
    }

    fn stats(&self) -> PublisherStats {
        let s = self.state.lock().expect("HlsPublisher poisoned");
        let mut out = PublisherStats::default();
        out.segments_written = s.segmenter.segments_written();
        out.bytes_written = s.bytes_pushed_total;
        out.current_segment_age = s.segmenter.current_segment_age();
        out.last_segment_duration = s.segmenter.last_segment_duration();
        out
    }
}

impl HlsPublisher {
    /// Shared terminal-state work for [`Publisher::finish`] and
    /// [`Self::finish_serving`]: cuts any open segment (which rewrites the
    /// non-terminal playlist, like every cut), marks the segmenter
    /// finished, and writes the terminal playlist (with `#EXT-X-ENDLIST`)
    /// to disk. Callers still own taking/shutting down the HTTP server.
    fn finalize_and_write(&mut self) -> Result<(), HlsError> {
        self.finished = true;
        let mut s = self.state.lock().expect("HlsPublisher poisoned");
        s.segmenter.finalize()?;
        s.finished = true;
        s.segmenter.write_playlist(true)
    }

    /// Like [`Publisher::finish`], but keeps the built-in HTTP server serving
    /// the completed (terminal) playlist and segments until the returned
    /// [`HlsServerHandle`] is dropped or [`HlsServerHandle::shutdown`] is
    /// called.
    ///
    /// This is how a VOD or EVENT stream becomes observable after the stream
    /// ends: the server stays up so clients can request the full playlist and
    /// all segment files.
    ///
    /// Returns [`HlsError::Finished`] if the publisher has already been
    /// finished.
    #[cfg(feature = "serve")]
    pub fn finish_serving(mut self) -> Result<HlsServerHandle, HlsError> {
        if self.finished {
            return Err(HlsError::Finished);
        }
        self.finalize_and_write()?;
        let server = self
            .server
            .take()
            .ok_or_else(|| HlsError::Internal("HTTP server not running".into()))?;
        Ok(HlsServerHandle {
            server,
            _state: self.state,
        })
    }
}

/// Keeps the finished playlist and segments served until dropped or
/// [`shutdown`](HlsServerHandle::shutdown) is called.
///
/// Obtained by calling [`HlsPublisher::finish_serving`].
#[cfg(feature = "serve")]
pub struct HlsServerHandle {
    server: crate::http_server::ServerHandle,
    // Keeps the Arc<Mutex<State>> alive so the HTTP server's shared state
    // is not freed while the handle is live.
    _state: Arc<Mutex<State>>,
}

#[cfg(feature = "serve")]
impl HlsServerHandle {
    /// The local socket address the HTTP server is bound to.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.server.local_addr()
    }

    /// Stop serving and drain the runtime. Also happens automatically on drop.
    pub fn shutdown(self) {
        self.server.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HlsMode;
    use std::path::PathBuf;
    use std::time::Duration;

    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hls-pub-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn push_segments_then_finish_writes_playlist() {
        let dir = tmpdir("ok");
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            ..HlsConfig::default()
        };
        let mut p = HlsPublisher::with_config(cfg).unwrap();
        p.push_ts(&[0x47u8; 376]).unwrap();
        p.cut_segment().unwrap();
        p.push_ts(&[0x47u8; 376]).unwrap();
        p.cut_segment().unwrap();

        let stats = p.stats();
        assert_eq!(stats.segments_written, 2);
        assert_eq!(stats.bytes_written, 752);

        p.finish().unwrap();
        let pl = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
        assert!(pl.contains("#EXTM3U"));
        assert!(pl.contains("segment_00000.ts"));
    }

    #[test]
    fn cut_writes_a_live_playlist_before_finish() {
        let dir = tmpdir("live-pl");
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Event,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..HlsConfig::default()
        };
        let mut p = HlsPublisher::with_config(cfg).unwrap();
        p.push_ts(&[0x47u8; 376]).unwrap();
        p.cut_segment().unwrap();
        // An external static server (nginx, a CDN origin) can serve the
        // stream while it runs: the playlist is on disk after the first cut.
        let pl = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
        assert!(pl.contains("segment_00000.ts"));
        assert!(!pl.contains("#EXT-X-ENDLIST"), "not terminal before finish");
        assert_eq!(pl, p.render_playlist(false));

        p.finish().unwrap();
        let pl = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
        assert!(
            pl.contains("#EXT-X-ENDLIST"),
            "finish writes the terminal playlist"
        );
    }

    #[test]
    fn unaligned_push_rejected() {
        let dir = tmpdir("unalign");
        let cfg = HlsConfig {
            output_dir: dir,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..HlsConfig::default()
        };
        let mut p = HlsPublisher::with_config(cfg).unwrap();
        assert!(matches!(
            p.push_ts(&[0u8; 187]),
            Err(HlsError::UnalignedPushTs { len: 187 })
        ));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn http_serves_playlist_and_segment() {
        let dir = tmpdir("http");
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            ..HlsConfig::default()
        };
        let mut p = HlsPublisher::with_config(cfg).unwrap();
        p.push_ts(&[0x47u8; 376]).unwrap();
        p.cut_segment().unwrap();
        let addr = p.local_addr().unwrap();

        // Blocking HTTP GET using std::net.
        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        sock.write_all(b"GET /playlist.m3u8 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        sock.read_to_string(&mut resp).unwrap();
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("#EXTM3U"));
        assert!(resp.contains("segment_00000.ts"));

        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        sock.write_all(b"GET /segment_00000.ts HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).unwrap();
        let s = String::from_utf8_lossy(&resp);
        assert!(s.contains("200 OK"));
        assert!(s.contains("video/mp2t"));

        p.finish().unwrap();
    }

    #[test]
    fn cut_with_duration_sets_media_extinf() {
        let dir = tmpdir("mediadur");
        let cfg = HlsConfig {
            output_dir: dir,
            bind: "127.0.0.1:0".parse().unwrap(),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut p = HlsPublisher::with_config(cfg).unwrap();
        p.push_ts(&[0x47u8; 188]).unwrap();
        p.cut_segment_with_duration(Duration::from_millis(3200))
            .unwrap();
        p.push_ts(&[0x47u8; 188]).unwrap();
        p.cut_segment_with_duration(Duration::from_millis(4100))
            .unwrap();
        let pl = p.render_playlist(false);
        assert!(pl.contains("#EXTINF:3.200,"), "playlist:\n{pl}");
        assert!(pl.contains("#EXTINF:4.100,"), "playlist:\n{pl}");
        p.finish().unwrap();
    }

    #[cfg(feature = "serve")]
    #[test]
    fn basic_auth_rejects_unauthorized() {
        let dir = tmpdir("auth");
        let cfg = HlsConfig {
            output_dir: dir,
            bind: "127.0.0.1:0".parse().unwrap(),
            basic_auth: Some(("alice".into(), "s3cret".into())),
            ..HlsConfig::default()
        };
        let p = HlsPublisher::with_config(cfg).unwrap();
        let addr = p.local_addr().unwrap();

        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        sock.write_all(b"GET /playlist.m3u8 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        sock.read_to_string(&mut resp).unwrap();
        assert!(resp.contains("401 Unauthorized"));
    }
}
