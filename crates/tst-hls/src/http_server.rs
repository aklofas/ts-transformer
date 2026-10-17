//! Internal hyper 1.x HTTP server (serves /playlist.m3u8 + /segment_*.ts).
//!
//! The server runs on one thread named `tst-hls` that owns a current-thread
//! tokio runtime, owned by [`HlsPublisher`].
//! Callers see only the sync `Publisher` API.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::auth::check_basic_auth;
use crate::error::HlsError;
use crate::playlist;
use crate::publisher::State;

/// Routing decision for a single HTTP request.
///
/// Kept as a standalone type so LL-HLS routes (blocking playlist queries,
/// media parts) can slot in alongside `Playlist` and `Segment` without
/// rewriting `serve()`.
pub(crate) enum Route {
    Playlist,
    Segment(String),
    NotFound,
}

/// Method + path dispatcher.
///
/// Only GET is routed; every other method maps to `NotFound` (→ 404).
/// The `path` argument is the raw, percent-encoded URI path as delivered by
/// hyper — `%2F` sequences are intentionally left encoded so they never act
/// as real path separators in a lookup key.
pub(crate) fn route(method: &hyper::Method, path: &str) -> Route {
    if method != hyper::Method::GET {
        return Route::NotFound;
    }
    if path == "/playlist.m3u8" {
        return Route::Playlist;
    }
    // Strip the leading `/`, reject anything with an embedded `/` or `\`
    // (multi-component paths, double-slashes, encoded or literal traversal).
    match path.strip_prefix('/') {
        Some(name) if !name.is_empty() && !name.contains('/') && !name.contains('\\') => {
            Route::Segment(name.to_string())
        }
        _ => Route::NotFound,
    }
}

/// Handle owned by the publisher. The server is one OS thread named
/// `tst-hls` that owns a current-thread runtime; the handle cancels it and
/// waits a bounded time for it to finish.
pub(crate) struct ServerHandle {
    pub(crate) cancel: CancellationToken,
    // Behind a mutex so the handle keeps the auto traits it had with a runtime.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    done: Done,
    pub(crate) local_addr: std::net::SocketAddr,
}

/// Set when the server thread's body ends; `shutdown()` waits on it.
type Done = Arc<(Mutex<bool>, std::sync::Condvar)>;

/// Sets the completion flag when the server thread's body ends, including
/// on a panic.
struct DoneOnDrop(Done);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        let (flag, cv) = &*self.0;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
}

impl ServerHandle {
    pub(crate) fn start(
        state: Arc<Mutex<State>>,
        bind: std::net::SocketAddr,
        basic_auth: Option<(String, String)>,
        #[cfg(feature = "tls")] tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<Self, HlsError> {
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let state_for_task = state.clone();

        #[cfg(feature = "tls")]
        let acceptor = tls_config.map(tokio_rustls::TlsAcceptor::from);

        let (ready_tx, ready_rx) =
            std::sync::mpsc::channel::<Result<std::net::SocketAddr, HlsError>>();
        let done: Done = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let done_for_thread = Arc::clone(&done);

        let thread = std::thread::Builder::new()
            .name("tst-hls".into())
            .spawn(move || {
                let _done = DoneOnDrop(done_for_thread);
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(HlsError::Io(e)));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let (listener, local_addr) = match async {
                        let l = TcpListener::bind(bind).await?;
                        let a = l.local_addr()?;
                        Ok::<_, std::io::Error>((l, a))
                    }
                    .await
                    {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = ready_tx.send(Err(HlsError::BindFailed(e.to_string())));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(local_addr));
            loop {
                tokio::select! {
                    _ = cancel_for_task.cancelled() => break,
                    accepted = listener.accept() => {
                        let (sock, _peer) = match accepted {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let state = state_for_task.clone();
                        let auth = basic_auth.clone();
                        let conn_cancel = cancel_for_task.clone();
                        #[cfg(feature = "tls")]
                        let acceptor_clone = acceptor.clone();

                        tokio::spawn(async move {
                            #[cfg(feature = "tls")]
                            if let Some(acc) = &acceptor_clone {
                                match acc.accept(sock).await {
                                    Ok(tls_sock) => {
                                        let io = TokioIo::new(tls_sock);
                                        let svc = service_fn(move |req| {
                                            let state = state.clone();
                                            let auth = auth.clone();
                                            async move { Ok::<_, Infallible>(serve(req, state, auth).await) }
                                        });
                                        let _ = tokio::select! {
                                            _ = conn_cancel.cancelled() => Ok(()),
                                            r = http1::Builder::new().serve_connection(io, svc) => r,
                                        };
                                    }
                                    Err(_) => {} // bad TLS handshake — drop
                                }
                                return;
                            }
                            // Plain HTTP path (no TLS feature OR TLS feature but no config)
                            let io = TokioIo::new(sock);
                            let svc = service_fn(move |req| {
                                let state = state.clone();
                                let auth = auth.clone();
                                async move { Ok::<_, Infallible>(serve(req, state, auth).await) }
                            });
                            let _ = tokio::select! {
                                _ = conn_cancel.cancelled() => Ok(()),
                                r = http1::Builder::new().serve_connection(io, svc) => r,
                            };
                        });
                    }
                }
            }
        });
                // Dropping the runtime here ends any connection task still open.
            })
            .map_err(HlsError::Io)?;

        let local_addr = match ready_rx.recv() {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(HlsError::Io(std::io::Error::other(
                    "the HLS server thread ended before it bound",
                )));
            }
        };

        Ok(Self {
            cancel,
            thread: Mutex::new(Some(thread)),
            done,
            local_addr,
        })
    }

    pub(crate) fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Cancel, then wait up to 2 s for the server thread to finish; a thread
    /// that has not finished by then is detached.
    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.cancel.cancel();
        let taken = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(thread) = taken else {
            return;
        };
        let (flag, cv) = &*self.done;
        let finished = flag.lock().unwrap_or_else(|e| e.into_inner());
        let (finished, _) = cv
            .wait_timeout_while(finished, std::time::Duration::from_secs(2), |f| !*f)
            .unwrap_or_else(|e| e.into_inner());
        if *finished {
            drop(finished);
            let _ = thread.join();
        }
    }
}

impl Drop for ServerHandle {
    /// Cancels and detaches: the thread ends on its own once it sees the
    /// cancel.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Per-request handler.
async fn serve(
    req: Request<Incoming>,
    state: Arc<Mutex<State>>,
    basic_auth: Option<(String, String)>,
) -> Response<Full<Bytes>> {
    // Auth check (if configured) — runs before routing so every route is
    // covered, including the playlist endpoint.
    if let Some((user, pass)) = &basic_auth {
        let header = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !check_basic_auth(user, pass, header) {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(WWW_AUTHENTICATE, r#"Basic realm="tst-hls""#)
                .body(Full::new(Bytes::from_static(b"Unauthorized")))
                .unwrap();
        }
    }

    match route(req.method(), req.uri().path()) {
        Route::Playlist => {
            let pl = {
                let s = state.lock().expect("HlsPublisher poisoned");
                playlist::render(&s.segmenter, s.finished)
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .body(Full::new(Bytes::from(pl)))
                .unwrap()
        }
        Route::Segment(name) => {
            // Resolve under the lock, then release it before doing I/O.
            let path = {
                let s = state.lock().expect("HlsPublisher poisoned");
                s.segmenter.serve_lookup(&name)
            };
            match path.and_then(|p| std::fs::read(&p).ok()) {
                Some(bytes) => Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "video/mp2t")
                    .body(Full::new(Bytes::from(bytes)))
                    .unwrap(),
                // Not in known set, or grace-purge race: both map to 404.
                None => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Full::new(Bytes::from_static(b"Not Found")))
                    .unwrap(),
            }
        }
        Route::NotFound => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from_static(b"Not Found")))
            .unwrap(),
    }
}
