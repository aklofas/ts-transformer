//! Mount-path normalization: how a request URI, a registered mount and a
//! rule key are reduced to one key, and how a SETUP URI's per-media
//! control segment is told apart from the mount path.

use crate::rtsp::server::session::ServerSessionState;

/// Extract the path component from a request URI. Handles both full
/// `rtsp://host:port/path` and path-only `/path` forms.
///
/// Per-media control URLs like `a=control:trackID=0` from the SDP cause
/// `RtspClient::setup_mp2t_auto` to append `/trackID=N` to the SETUP
/// URI. We strip the trailing per-media segment if it's recognized
/// (RFC 7826 §C.1.1 trackID convention) — otherwise the mount lookup
/// misses the registered base path.
///
/// Every trailing `/` is also stripped (except on the bare root `"/"`) so
/// that `rtsp://host/live/` and `rtsp://host/live` both resolve to the
/// same mount registered at `"/live"`. Without this, a client that
/// appends a trailing slash to the DESCRIBE URI would get a 404 for a
/// perfectly valid mount. DESCRIBE, ANNOUNCE and SETUP go through this
/// function and PLAY uses the mount the reader's SETUP stored, so the
/// normalization is consistent across the whole session.
///
/// The result is a fixed point: it has no trailing `/` and no trailing
/// control segment, so `extract_mount_path(&extract_mount_path(u))` equals
/// `extract_mount_path(u)`. That is what lets [`super::validate_mount_path`]
/// define a canonical mount path as one this function returns unchanged,
/// so a registered mount, a rule key and a request lookup are one key.
pub(crate) fn extract_mount_path(uri: &str) -> String {
    let is_control = |seg: &str| {
        seg.starts_with("trackID=") || seg.starts_with("streamid=") || seg.starts_with("stream=")
    };
    // Normalize once, then strip control segments off the same slice until
    // none is left (`/live/trackID=0/trackID=1` is `/live`). Each step
    // reads only the segment and slashes it removes, so the walk is linear
    // in the URI length and the result is the only allocation.
    let mut path = normalized_path(uri);
    while let Some(shorter) = strip_last_segment(path, is_control) {
        path = shorter;
    }
    path.to_string()
}

/// Shared normalization for [`extract_mount_path`] and the publisher
/// SETUP path's announced-`a=control`-aware mount-path derivation (see
/// [`crate::rtsp::server::publish::handlers::handle_setup_record`]):
/// strip scheme + authority, strip a trailing query string, strip every
/// trailing slash, then strip the last `/`-delimited path segment when
/// `strip_last` says it's a per-media control segment (not part of the
/// registered mount path), then strip every trailing slash again.
fn normalize_mount_path(uri: &str, strip_last: impl FnOnce(&str) -> bool) -> String {
    let path = normalized_path(uri);
    strip_last_segment(path, strip_last)
        .unwrap_or(path)
        .to_string()
}

/// [`normalize_mount_path`] up to the control-segment step: `uri` without
/// scheme, authority, query or trailing slashes. Never allocates.
fn normalized_path(uri: &str) -> &str {
    // "/live/" and "/live//" resolve to the mount registered at "/live".
    trim_trailing_slashes(request_path(uri))
}

/// `uri` without scheme, authority and query string, and nothing else: the
/// path exactly as the peer spelled it. An on-demand ANNOUNCE validates this
/// form, because the normalised key would silently rename `/new/` to `/new`.
pub(crate) fn request_path(uri: &str) -> &str {
    // Strip scheme + authority if present.
    let path_start = if let Some(after_scheme) = uri.strip_prefix("rtsp://") {
        after_scheme
            .find('/')
            .map(|idx| &after_scheme[idx..])
            .unwrap_or("/")
    } else if let Some(after_scheme) = uri.strip_prefix("rtsps://") {
        after_scheme
            .find('/')
            .map(|idx| &after_scheme[idx..])
            .unwrap_or("/")
    } else {
        uri
    };
    // Strip any trailing query string; v1 matches the bare path component
    // of the mount as registered.
    path_start.split('?').next().unwrap_or(path_start)
}

/// `path` (already without trailing slashes) minus its last `/`-delimited
/// segment and the slashes before it, when `strip_last` says that segment
/// is a per-media control segment; `None` otherwise. SDP
/// `a=control:trackID=N` (the canonical form per RFC 7826 §C.1.1), or
/// `a=control:stream=N` (gst-rtsp-server's convention), causes a SETUP
/// URI of `<base>/trackID=N` or `<base>/stream=N`. SETUP matches against
/// the base mount path; the control segment is per-media and not part of
/// the registered mount. A `Some` is always shorter than `path`.
fn strip_last_segment(path: &str, strip_last: impl FnOnce(&str) -> bool) -> Option<&str> {
    let last_slash = path.rfind('/')?;
    if !strip_last(&path[last_slash + 1..]) {
        return None;
    }
    // Keep the leading slash if it's the only one (root mount).
    Some(if last_slash == 0 {
        "/"
    } else {
        trim_trailing_slashes(&path[..last_slash])
    })
}

/// `path` without its trailing slashes; the bare root `"/"` (and any
/// all-slash path) keeps one.
fn trim_trailing_slashes(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/"
    } else {
        trimmed
    }
}

/// The raw last `/`-delimited path segment of a request URI (scheme and
/// authority, if present, are irrelevant — `rsplit('/')` finds the same
/// trailing chunk either way), with no prefix check at all. `None` for
/// an empty segment (e.g. a URI ending in `/`). Shared by
/// [`control_segment`] (which additionally requires a recognized
/// prefix) and the publisher SETUP path's exact-match-against-announced-
/// `a=control` resolution, which needs the raw text regardless of
/// whether it looks like `trackID=`/`streamid=`/`stream=`.
pub(crate) fn last_path_segment(uri: &str) -> Option<&str> {
    let path = uri.split('?').next().unwrap_or(uri);
    let last = path.rsplit('/').next()?;
    (!last.is_empty()).then_some(last)
}

/// The trailing per-media control segment of a SETUP URI when it uses one
/// of the three recognized prefixes (`trackID=`, `streamid=`, `stream=`),
/// else `None`. [`extract_mount_path`] strips exactly these segments when
/// it locates the mount, so the two agree on what counts as one.
///
/// The publisher SETUP path tries `PublishSession::track_for_raw_segment`
/// first, which matches the URI's raw last segment against what the SDP
/// announced: a relative `a=control` value by exact text, an absolute
/// `rtsp(s)://` value by its path's last segment. This function is the
/// fallback after that, used by `PublishSession::track_for_control` for
/// the single-track announce whose SETUP names the bare mount URI.
pub(crate) fn control_segment(uri: &str) -> Option<&str> {
    let last = last_path_segment(uri)?;
    (last.starts_with("trackID=") || last.starts_with("streamid=") || last.starts_with("stream="))
        .then_some(last)
}

/// For a session that has already announced a publisher on this
/// connection, try resolving `uri`'s mount path by matching its raw
/// trailing path segment against this session's announced `a=control`
/// values (see `PublishSession::track_for_raw_segment`: exact text for a
/// relative value, the path's last segment for an absolute `rtsp(s)://`
/// value) and, on a match, taking the URI's path minus that segment.
/// Handles any control-naming convention (gst-rtsp-server's
/// `rtspclientsink` announces bare `a=control:stream=0`; other tools may
/// use yet other text, or a full URL naming their own view of the host)
/// that [`extract_mount_path`]'s fixed `trackID=`/`streamid=`/`stream=`
/// prefix list doesn't recognize. `None` when this session isn't a
/// publisher yet, or when nothing it announced matches the URI's
/// trailing segment — the caller falls back to [`extract_mount_path`]
/// in both cases.
pub(crate) fn publisher_mount_path(uri: &str, session: &ServerSessionState) -> Option<String> {
    let publish = session.publisher()?;
    let last = last_path_segment(uri)?;
    publish.track_for_raw_segment(last)?;
    Some(normalize_mount_path(uri, |seg| seg == last))
}

/// The normalized path of an absolute `rtsp://` / `rtsps://` `a=control`
/// value (scheme matched case-insensitively, authority and query
/// dropped, a trailing `/` stripped), or `None` for a relative value. A
/// publisher writes its own view of the server's address into an
/// absolute control URL, so only the path is meaningful to the server.
pub(crate) fn absolute_control_path(control: &str) -> Option<String> {
    let rest = ["rtsp://", "rtsps://"].iter().find_map(|scheme| {
        let head = control.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme)
            .then(|| &control[scheme.len()..])
    })?;
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    Some(normalize_mount_path(path, |_| false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_keeps_trailing_slashes_and_control_segments() {
        assert_eq!(request_path("rtsp://h/new/?x=1"), "/new/");
        assert_eq!(request_path("rtsp://h:554/new/trackID=0"), "/new/trackID=0");
        assert_eq!(request_path("rtsps://h"), "/");
        assert_eq!(request_path("/a//b"), "/a//b");
    }

    #[test]
    fn extract_mount_path_strips_scheme_and_authority() {
        assert_eq!(extract_mount_path("rtsp://host:8554/live"), "/live");
        assert_eq!(extract_mount_path("rtsps://host/live"), "/live");
        assert_eq!(extract_mount_path("/live"), "/live");
        assert_eq!(extract_mount_path("rtsp://host:8554/live?x=1"), "/live");
        // No path → root.
        assert_eq!(extract_mount_path("rtsp://host:8554"), "/");
        // Trailing slash is normalized away (except root).
        assert_eq!(extract_mount_path("rtsp://host:8554/live/"), "/live");
        assert_eq!(extract_mount_path("/live/"), "/live");
        // Root path ("/") keeps its slash.
        assert_eq!(extract_mount_path("rtsp://host:8554/"), "/");
        assert_eq!(extract_mount_path("/"), "/");
    }

    #[test]
    fn extract_mount_path_strips_trackid_segment() {
        // RtspClient::setup_mp2t_auto appends /trackID=0 from the SDP
        // a=control: attribute, so the SETUP URI is /live/trackID=0 but
        // only /live is registered as a mount. The fix strips the per-media
        // control segment.
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/trackID=0"),
            "/live"
        );
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/trackID=1"),
            "/live"
        );
        // Also strip the alternate streamid= form (older cameras).
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/streamid=0"),
            "/live"
        );
        // Non-track suffixes are kept (multi-segment mount paths).
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/audio"),
            "/live/audio"
        );
        // Root mount with trackID — keep root slash.
        assert_eq!(extract_mount_path("rtsp://host:8554/trackID=0"), "/");
    }

    #[test]
    fn normalizer_is_idempotent_on_every_corpus_entry() {
        for uri in [
            "/live//",
            "/live/trackID=0/",
            "/x//trackID=0",
            "/live/trackID=0/trackID=1",
            "rtsp://h/live///",
            "/trackID=0/",
            "/",
            "/a/b/",
            "rtsp://h:8554/a/stream=1/",
        ] {
            let once = extract_mount_path(uri);
            assert_eq!(extract_mount_path(&once), once, "uri {uri:?}");
        }
        assert_eq!(extract_mount_path("/live/trackID=0/"), "/live");
        assert_eq!(extract_mount_path("/live//trackID=0"), "/live");
        assert_eq!(extract_mount_path("/live//"), "/live");
        assert_eq!(extract_mount_path("/trackID=0/"), "/");
    }

    /// The normalizer as it was written before the single-pass walk: one
    /// full normalization per control segment until a fixed point.
    fn extract_mount_path_by_repeated_passes(uri: &str) -> String {
        let is_control = |seg: &str| {
            seg.starts_with("trackID=")
                || seg.starts_with("streamid=")
                || seg.starts_with("stream=")
        };
        let mut path = normalize_mount_path(uri, is_control);
        loop {
            let next = normalize_mount_path(&path, is_control);
            if next == path {
                return path;
            }
            path = next;
        }
    }

    #[test]
    fn single_pass_normalizer_matches_the_repeated_pass_definition() {
        let heads = ["", "/", "rtsp://h:8554", "rtsps://h", "rtsp://h/", "x"];
        let segs = [
            "/live",
            "/",
            "//",
            "/trackID=0",
            "/streamid=1",
            "/stream=2",
            "/audio",
            "/trackID",
        ];
        let tails = ["", "?q=1", "/?q=/trackID=0", "/"];
        for head in heads {
            for a in segs {
                for b in segs {
                    for c in segs {
                        for tail in tails {
                            let uri = format!("{head}{a}{b}{c}{tail}");
                            assert_eq!(
                                extract_mount_path(&uri),
                                extract_mount_path_by_repeated_passes(&uri),
                                "uri {uri:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn many_trailing_control_segments_normalize_to_the_mount() {
        // A request URI within the 64 KiB header cap can carry thousands
        // of control segments; all of them go, and the walk is linear.
        let uri = format!("rtsp://h:8554/live{}", "/stream=0".repeat(7_000));
        assert!(uri.len() < crate::rtsp::message::MAX_RTSP_MESSAGE_BYTES);
        assert_eq!(extract_mount_path(&uri), "/live");
        let uri = format!("/a//{}/", "trackID=1//streamid=2/".repeat(2_000));
        assert_eq!(extract_mount_path(&uri), "/a");
    }
}
