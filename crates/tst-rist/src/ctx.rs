//! The librist context a sender and a receiver both own: context creation,
//! the one peer, the stats callback, `rist_start`, and the single destroy
//! that reclaims the stats reference handed to librist.

use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::{Arc, Mutex};

use crate::config::{RistConfig, RistProfile};
use crate::error::RistError;
use crate::init::ensure_init;
use crate::stats::{OnDrop, RistStats};
use crate::transport::{apply_peer_overrides, global_logging_ptr, rist_profile_to_c};
use crate::url::{RistUrl, native_endpoint};

/// Which end of the session a [`RistCtx`] is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Caller: `rist_sender_create`, peer `initiate_conn = 1`.
    Sender,
    /// Listener: `rist_receiver_create`, a bind endpoint, `initiate_conn = 0`.
    Receiver,
}

pub(crate) struct RistCtx {
    ptr: *mut rist_sys::rist_ctx,
    /// Cumulative stats, written by librist's stats callback.
    pub(crate) stats: Arc<Mutex<RistStats>>,
    /// Leaked `Arc<Mutex<RistStats>>` ref handed to librist as the stats
    /// callback `arg`. Reclaimed exactly once in [`Self::destroy`] after
    /// `rist_destroy`.
    stats_arg: *mut c_void,
    /// The native endpoint the peer was created from (for diagnostics).
    pub(crate) endpoint: String,
}

impl RistCtx {
    /// Create, configure and start a context for `role`. Every failure after
    /// the context exists destroys it before returning.
    pub(crate) fn open(url: &RistUrl, cfg: &RistConfig, role: Role) -> Result<Self, RistError> {
        ensure_init();

        #[cfg(not(feature = "mbedtls"))]
        if cfg.encryption.is_some() {
            return Err(RistError::EncryptionDisabled);
        }

        // librist's Simple-profile receiver (rist.c's rist_receiver_peer_create)
        // opens a second, RTCP peer on port + 1 by rebuilding the bind address
        // from host and port, and drops the IPv6 brackets doing it, so for an
        // IPv6 bind that peer never comes up. The vendored build null-checks
        // the RTCP peer, so there the failure would be PeerCreateFailed; but a
        // system librist 0.2.20 dereferences the failed peer and SIGSEGVs the
        // process. Refuse here, before any librist context or peer creation:
        // the IPv6 Simple-profile receiver cannot work with either build.
        // Caller/sender side is unaffected — rist_sender_peer_create
        // null-checks before dereferencing — so only the receiver bind is
        // guarded.
        if role == Role::Receiver && cfg.profile == RistProfile::Simple && url.addr.is_ipv6() {
            return Err(RistError::InvalidConfig(
                "IPv6 bind is not supported in the Simple profile: librist rebuilds the RTCP \
                 peer's address without the IPv6 brackets, and librist 0.2.20 as shipped \
                 upstream dereferences that failed peer; use RistProfile::Main"
                    .into(),
            ));
        }

        let profile = rist_profile_to_c(cfg.profile);
        let logging_settings = global_logging_ptr();

        // ===== Create the context =====
        let mut ctx: *mut rist_sys::rist_ctx = std::ptr::null_mut();
        let rc = unsafe {
            match role {
                Role::Sender => {
                    rist_sys::rist_sender_create(&mut ctx, profile, 0, logging_settings)
                }
                Role::Receiver => {
                    rist_sys::rist_receiver_create(&mut ctx, profile, logging_settings)
                }
            }
        };
        if rc != 0 || ctx.is_null() {
            return Err(RistError::ContextCreateFailed);
        }
        // Guard that calls rist_destroy on every error-exit path. Disarmed at
        // the bottom once ctx is safely in Self.
        let mut ctx_guard = OnDrop::new(|| unsafe {
            rist_sys::rist_destroy(ctx);
        });

        // ===== Parse the peer (sender) or bind (receiver) URL =====
        let bind = role == Role::Receiver;
        let endpoint = native_endpoint(url, bind);
        let endpoint_c = CString::new(endpoint.clone()).map_err(|e| {
            let which = if bind { "bind" } else { "peer" };
            RistError::InvalidConfig(format!("bad {which} URL: {e}"))
        })?;

        let mut peer_config: *mut rist_sys::rist_peer_config = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_parse_address2(endpoint_c.as_ptr(), &mut peer_config) };
        if rc != 0 || peer_config.is_null() {
            return Err(RistError::Ffi {
                code: rc,
                function: "rist_parse_address2",
            });
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // Sender = caller (initiate_conn=1); receiver = listener (0).
        unsafe {
            (*peer_config).initiate_conn = if bind { 0 } else { 1 };
        }

        // The cfg overlays (cname / secret / buffer / …) apply to both ends.
        if let Err(e) = apply_peer_overrides(peer_config, cfg) {
            unsafe {
                // peer_config_free2 must happen before rist_destroy (guard drop).
                rist_sys::rist_peer_config_free2(&mut peer_config);
            }
            return Err(e);
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // ===== Add the peer =====
        let mut peer: *mut rist_sys::rist_peer = std::ptr::null_mut();
        let rc = unsafe { rist_sys::rist_peer_create(ctx, &mut peer, peer_config) };
        // peer_config is now owned by librist (or freed internally); per
        // librist docs we still free the wrapper.
        unsafe {
            rist_sys::rist_peer_config_free2(&mut peer_config);
        }
        if rc != 0 {
            return Err(RistError::PeerCreateFailed);
            // ctx_guard drops here → rist_destroy(ctx)
        }

        // Register the librist stats callback BEFORE rist_start (interval,
        // leak-one-Arc-ref + reclaim-at-close contract live in
        // stats::register_stats_callback). The order is load-bearing: librist
        // >= 0.2.20's protocol thread re-reads the stats interval lock-free on
        // every tick, so registering once that thread is running is a data
        // race that TSan reports. librist's own tools register before start too.
        let (stats, stats_arg) = crate::stats::register_stats_callback(ctx);

        // ===== Start the session =====
        let rc = unsafe { rist_sys::rist_start(ctx) };
        if rc != 0 {
            // Mirror destroy(): rist_destroy first (no protocol thread ever
            // ran, so no callback can be in flight), then reclaim the leaked
            // Arc ref exactly once — there is no Self to do it.
            drop(ctx_guard);
            unsafe { drop(Arc::from_raw(stats_arg as *const Mutex<RistStats>)) };
            return Err(RistError::Ffi {
                code: rc,
                function: "rist_start",
            });
        }

        // All setup succeeded — ctx ownership transfers to Self.
        ctx_guard.disarm();
        Ok(Self {
            ptr: ctx,
            stats,
            stats_arg,
            endpoint,
        })
    }

    /// The live context, or null after [`Self::destroy`].
    pub(crate) fn as_ptr(&self) -> *mut rist_sys::rist_ctx {
        self.ptr
    }

    #[cfg(test)]
    pub(crate) fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// `rist_destroy` (which joins librist's protocol thread), then reclaim
    /// the leaked stats reference — exactly once: both steps are
    /// null-guarded, so a second call and the drop after an explicit close
    /// are no-ops.
    pub(crate) fn destroy(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                rist_sys::rist_destroy(self.ptr);
            }
            self.ptr = std::ptr::null_mut();
        }
        // No callback can be in flight once rist_destroy has returned.
        if !self.stats_arg.is_null() {
            unsafe {
                drop(Arc::from_raw(self.stats_arg as *const Mutex<RistStats>));
            }
            self.stats_arg = std::ptr::null_mut();
        }
    }
}

impl Drop for RistCtx {
    fn drop(&mut self) {
        self.destroy();
    }
}
