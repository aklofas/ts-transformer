//! TLS support for the HLS HTTP server (tokio-rustls 0.26; feature `tls`).

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use tst_core::net::tls::TlsLoadError;

use crate::error::HlsError;

/// Load a server certificate + key from PEM files. Read and PEM-parse
/// failures are `HlsError::Io`, as before; the rest are `HlsError::Tls`.
pub(crate) fn load_server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, HlsError> {
    tst_core::net::tls::load_server_config(cert, key)
        .map(Arc::new)
        .map_err(|e| match e {
            TlsLoadError::Read { source, .. } | TlsLoadError::Pem { source, .. } => {
                HlsError::Io(source)
            }
            other => HlsError::Tls(other.to_string()),
        })
}
