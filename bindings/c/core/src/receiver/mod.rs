//! Receiver-side C ABI surface — read-side entry points.
//!
//! Submodules:
//! - [`demux_receiver`] — `tst_demux_receiver_t` (split into 4 sub-files).
//! - [`raw_receiver`]   — `tst_raw_receiver_t` + `tst_managed_raw_receiver_t`.
//! - [`ts_receiver`]    — `tst_receiver_t` + `tst_managed_receiver_t`.
//!
//! The SRT open itself is not here any more: plain opens call
//! `tst_srt::SrtUrl::{connect, accept_one}` and managed opens call the
//! `tst_srt::shells::managed_*_from_url` family (Arc 2 WP-A3), which is
//! also what the Python and JVM bindings use.

pub mod demux_receiver;
pub mod raw_receiver;
pub mod ts_receiver;

/// Open ONE plain SRT transport for the URL's mode. Listener mode goes
/// through [`tst_srt::SrtUrl::accept_one`] with a fresh slot: nothing can
/// fire it before `_open_listener` returns (the C caller has no pointer
/// yet — DEBT-16, deferred in Arc 2), but the accept path is the same
/// cancellable one the managed re-accept uses, so a future two-phase open
/// only has to hand the slot out.
///
/// Caller mode uses [`tst_srt::SrtUrl::connect`], NOT `connect_recv`: every
/// C caller-mode open merged the sender preset through the old `connect_srt`
/// (15 s connect timeout, 5 s linger, `Role::Sender`), and this preserves
/// that exactly.
///
/// Records the failure to the thread-local last-error and returns `Err(())`
/// so callers just `return std::ptr::null_mut()`.
pub(crate) fn open_plain_srt(url: &tst_srt::SrtUrl) -> Result<tst_srt::SrtTransport, ()> {
    let r = match url.mode {
        tst_srt::url::Mode::Listener => url.accept_one(&tst_core::cancel::CancelSlot::new()),
        _ => url.connect(),
    };
    r.map_err(|e| {
        crate::error::record_binding_error(tst_pipeline::binding::BindingError::from(e));
    })
}
