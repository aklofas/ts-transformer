//! TLS support for the WHEP signalling endpoint (feature `tls`).

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;

use crate::error::WhepServerError;

/// Load a server certificate and key from PEM files.
#[expect(
    dead_code,
    reason = "called by the server's bind, which is not in the crate yet"
)]
pub(crate) fn load_server_config(
    cert: &Path,
    key: &Path,
) -> Result<Arc<ServerConfig>, WhepServerError> {
    tst_core::net::tls::load_server_config(cert, key)
        .map(Arc::new)
        .map_err(|e| WhepServerError::Tls(e.to_string()))
}
