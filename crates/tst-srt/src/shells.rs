//! URL → managed shell in one call: the `from_url` family.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Each function composes, once for the C, Python and JVM bindings: build
//! the reconnect factory from the parsed [`SrtUrl`] (caller →
//! [`SrtUrl::connect`], listener →
//! [`SrtUrl::accept_one`] through the managed transport's
//! [`FactoryCancel`] slot), open the initial transport the same way, wrap
//! it in the managed decorator, wrap THAT in the shell, and hand back the
//! shell together with the five lock-free observers a binding keeps
//! ([`ManagedHandles`]) — every one obtained before the shell moves, so
//! nothing is ever read back out of the shell.
//!
//! The initial open blocks: a listener-mode receiver returns only once
//! the first peer completes its handshake. That first accept already goes
//! through the same slot the returned [`ManagedHandles::cancel`] fires,
//! but nothing exists to fire it with before the function returns — a
//! caller that needs the FIRST accept reachable from another thread uses
//! [`managed_recv_transport_from_url`] with its own pre-created slot.
//! In caller mode that slot is read before every dial (the first connect
//! and each re-dial): a slot already cancelled refuses the dial without
//! dialling. A cancel that arrives during the dial does not interrupt it
//! (the slot holds nothing to fire): if the dial then succeeds the open
//! returns a live transport (end it through its own cancel handle; on a
//! managed receiver the next re-dial is refused), and if it fails the
//! open returns `ExplicitClose`.
//!
//! A sender URL with `x-silentpeertimeout=<ms>` arms the managed decorator
//! for silent-peer detection
//! ([`ManagedTransport::with_silent_peer_timeout`]): the connection is
//! abandoned and re-dialled when the peer has answered none of its sends
//! for that window. The window is taken out of the URL before it is
//! dialled, because every SRT open refuses a config that carries it; the
//! receivers refuse the key.
//!
//! Senders are caller-only: `?mode=listener` is refused before any
//! socket is touched (see `docs/project/deferred-features.md`, "SRT URL
//! `mode=listener` / `mode=rendezvous` dispatch"). Receivers accept both
//! modes.
//!
//! # Why the caller path uses [`SrtUrl::connect`], receivers included
//!
//! Every C caller-mode open — plain and managed, TS / raw / demux — and
//! every managed caller-mode open in the Python and JVM bindings goes
//! through [`SrtUrl::connect`], which applies
//! [`SrtSocketConfig::merge_sender_defaults`](crate::config::SrtSocketConfig::merge_sender_defaults).
//! A managed caller-mode RECEIVER therefore runs with the sender preset
//! (15 s connect timeout, 5 s linger, `Role::Sender`) —
//! [`SrtUrl::connect_recv`] (the URL-keys-only open) is for the plain
//! Python/JVM opens that never merged it, not for this family.

use std::sync::Arc;

use tst_core::mpegts::demux::DemuxerConfig;
use tst_core::mpegts::mux::MuxerConfig;
use tst_core::transport::{BrokenCause, EndReasonHandle, RecvTransport, Transport, TransportError};
use tst_pipeline::binding::ManagedHandles;
use tst_pipeline::{
    FactoryCancel, ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
    ManagedStatsHandle, ManagedTransport, MuxSender, RawReceiver, RawReceiverConfig, RawSender,
    RawSenderConfig, Receiver, ReceiverConfig, ReconnectPolicy, Sender, SenderConfig,
};

use crate::error::{OptionError, SrtError};
use crate::transport::SrtTransport;
use crate::url::{Mode, SrtUrl};

/// Map an open-path error into the `TransportError` the managed
/// decorators' factories return. A `Transport` error from
/// [`SrtUrl::accept_one`] passes through unchanged — `ExplicitClose` must
/// survive so the decorator reports a caller-initiated close, not a wire
/// fault; `Broken { "bind: …" | "accept: …" }` is already the recoverable
/// shape. Everything else (the typed [`SrtConnectError`](crate::SrtConnectError)
/// from [`SrtUrl::connect`]) becomes the recoverable `Broken` the
/// reconnect loop retries, its message prefixed `connect: `. Once
/// process exit has started there is nothing to
/// retry: a dial refused or cancelled by the exit guard is
/// `ExplicitClose`, which ends the reconnect loop.
fn open_error_to_transport(e: SrtError) -> TransportError {
    match e {
        SrtError::Transport(t) => t,
        _ if crate::exit_guard::is_exiting() => TransportError::ExplicitClose,
        other => TransportError::Broken {
            msg: format!("connect: {other}"),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        },
    }
}

/// A refused [`ReconnectPolicy`] as the option error every managed open
/// reports; checked before any socket is touched.
fn policy_refused(e: tst_pipeline::ReconnectPolicyError) -> SrtError {
    SrtError::Option(OptionError::OutOfRange(e.to_string()))
}

/// One open, dispatched on the URL's mode. A slot already cancelled
/// refuses either mode with `ExplicitClose`, the same answer
/// [`SrtUrl::accept_one`] gives, so a caller-mode open or re-dial never
/// starts once the cancel is seen. A dial that fails after a cancel
/// arrived during it reports the cancel too, as the listener path does
/// for a failed accept.
fn open(url: &SrtUrl, slot: &FactoryCancel) -> Result<SrtTransport, SrtError> {
    match url.mode {
        Mode::Caller if slot.is_cancelled() => {
            Err(SrtError::Transport(TransportError::ExplicitClose))
        }
        Mode::Caller => failure_under_cancel(url.connect(), slot),
        Mode::Listener => url.accept_one(slot),
    }
}

/// A failure while `slot` is cancelled is the cancel; a success wins.
fn failure_under_cancel<T>(r: Result<T, SrtError>, slot: &FactoryCancel) -> Result<T, SrtError> {
    r.map_err(|e| {
        if slot.is_cancelled() {
            SrtError::Transport(TransportError::ExplicitClose)
        } else {
            e
        }
    })
}

/// The receive-side reconnect decorator for `url`, opened now (blocking on
/// the first connect / accept) with a factory that re-opens the same URL
/// on every `Broken` and re-accepts through `factory_cancel`.
///
/// The `*_from_url` functions below call this with a fresh slot. Pass your
/// own — from a thread you control, with the slot shared beforehand — to
/// keep the FIRST accept cancellable too: a cancel before or during it
/// returns `Err(SrtError::Transport(TransportError::ExplicitClose))`.
/// In caller mode a cancel BEFORE the call returns the same error without
/// dialling. A cancel that arrives during the first connect does not
/// interrupt it (the slot holds nothing while the connect is in
/// progress): if the connect then succeeds this returns the live
/// transport (end it through its own cancel handle; its next re-dial is
/// refused), and if it fails this returns the same error. Every re-dial
/// checks the slot the same way, so a slot cancelled directly ends a
/// caller-mode reconnect with `ExplicitClose` instead of dialling again.
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - [`SrtError::Transport`]`(`[`TransportError::ExplicitClose`]`)` — `factory_cancel`
///   was cancelled before the call (either mode), during the first
///   accept, or during a first connect that then failed.
/// - Whatever the initial [`SrtUrl::connect`] / [`SrtUrl::accept_one`]
///   returned; the managed transport is not built.
pub fn managed_recv_transport_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    factory_cancel: Arc<FactoryCancel>,
) -> Result<ManagedRecvTransport<SrtTransport>, SrtError> {
    policy.validate().map_err(policy_refused)?;
    let initial = open(url, &factory_cancel)?;
    let url = url.clone();
    let slot = Arc::clone(&factory_cancel);
    let factory: Box<dyn FnMut() -> Result<SrtTransport, TransportError> + Send> =
        Box::new(move || open(&url, &slot).map_err(open_error_to_transport));
    ManagedRecvTransport::new_with_factory_cancel(initial, factory, policy, factory_cancel)
        .map_err(policy_refused)
}

/// The observers for a receive-side decorator, taken BEFORE it moves into
/// a shell. `end_reason` starts as a fresh never-set handle; the demux
/// shell replaces it with the one it records into.
fn recv_handles(managed: &ManagedRecvTransport<SrtTransport>) -> ManagedHandles {
    ManagedHandles {
        // Always `Some` by construction — `ManagedRecvTransport::cancel_handle`
        // builds a `ManagedRecvCancel` unconditionally. This is the ONE
        // `.expect`, so no binding site carries its own.
        cancel: managed
            .cancel_handle()
            .expect("ManagedRecvTransport::cancel_handle is always Some"),
        end_reason: EndReasonHandle::default(),
        reconnects: managed.reconnects_handle(),
        attempts: managed.attempts_handle(),
        reconnecting: managed.reconnecting_handle(),
    }
}

/// Open `url` (either mode) as a reconnecting demux receiver with `demux`
/// options, and return it with its [`ManagedHandles`].
/// [`ManagedHandles::end_reason`] is the shell's own handle: it records
/// `Cancelled` / `ReconnectExhausted` as the stream ends.
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_demux_receiver_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    demux: DemuxerConfig,
) -> Result<(ManagedDemuxReceiver<SrtTransport>, ManagedHandles), SrtError> {
    managed_demux_receiver_from_url_with_cancel(url, policy, demux, Arc::new(FactoryCancel::new()))
}

/// [`managed_demux_receiver_from_url`] with the caller's `factory_cancel`
/// slot, shared before the call so the first accept is reachable too: a
/// cancel of `factory_cancel` before or during the first accept returns
/// `Err(SrtError::Transport(TransportError::ExplicitClose))`. The same slot
/// then wakes every parked re-accept.
/// In caller mode a cancel before the call refuses the first connect
/// with the same error and nothing is dialled; a cancel during the connect
/// does not interrupt it: a connect that succeeds returns the live shell,
/// one that fails returns the same error (see
/// [`managed_recv_transport_from_url`]).
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_demux_receiver_from_url_with_cancel(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    demux: DemuxerConfig,
    factory_cancel: Arc<FactoryCancel>,
) -> Result<(ManagedDemuxReceiver<SrtTransport>, ManagedHandles), SrtError> {
    let managed = managed_recv_transport_from_url(url, policy, factory_cancel)?;
    let mut handles = recv_handles(&managed);
    let rx = ManagedDemuxReceiver::with_demux_options(
        managed,
        demux,
        ManagedDemuxReceiverConfig::default(),
    );
    handles.end_reason = rx.end_reason_handle();
    Ok((rx, handles))
}

/// Open `url` (either mode) as a reconnecting TS-bytes receiver. The plain
/// [`Receiver`] records no end reason, so
/// [`ManagedHandles::end_reason`] is a fresh handle that is never set.
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_receiver_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
) -> Result<(Receiver<ManagedRecvTransport<SrtTransport>>, ManagedHandles), SrtError> {
    managed_receiver_from_url_with_cancel(url, policy, Arc::new(FactoryCancel::new()))
}

/// [`managed_receiver_from_url`] with the caller's `factory_cancel` slot,
/// shared before the call so the first accept is reachable too: a cancel of
/// `factory_cancel` before or during the first accept returns
/// `Err(SrtError::Transport(TransportError::ExplicitClose))`. The same slot
/// then wakes every parked re-accept.
/// In caller mode a cancel before the call refuses the first connect
/// with the same error and nothing is dialled; a cancel during the connect
/// does not interrupt it: a connect that succeeds returns the live shell,
/// one that fails returns the same error (see
/// [`managed_recv_transport_from_url`]).
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_receiver_from_url_with_cancel(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    factory_cancel: Arc<FactoryCancel>,
) -> Result<(Receiver<ManagedRecvTransport<SrtTransport>>, ManagedHandles), SrtError> {
    let managed = managed_recv_transport_from_url(url, policy, factory_cancel)?;
    let handles = recv_handles(&managed);
    Ok((Receiver::new(managed, ReceiverConfig::default()), handles))
}

/// Open `url` (either mode) as a reconnecting raw-bytes receiver (C's
/// `tst_managed_raw_receiver`). No end reason is recorded: a fresh,
/// never-set handle.
///
/// # Errors
///
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_raw_receiver_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
) -> Result<
    (
        RawReceiver<ManagedRecvTransport<SrtTransport>>,
        ManagedHandles,
    ),
    SrtError,
> {
    let managed = managed_recv_transport_from_url(url, policy, Arc::new(FactoryCancel::new()))?;
    let handles = recv_handles(&managed);
    Ok((
        RawReceiver::new(managed, RawReceiverConfig::default()),
        handles,
    ))
}

/// Senders open as callers only (the C sender family never had a
/// listener path; Python refuses `?mode=listener` at construction).
/// Refused before any socket is touched.
fn callers_only() -> SrtError {
    SrtError::Option(OptionError::OutOfRange(
        "?mode=listener: managed senders open as callers only \
         (see deferred-features.md, \"SRT URL mode=listener / mode=rendezvous dispatch\")"
            .to_string(),
    ))
}

/// The send-side reconnect decorator for `url`: the initial connect now,
/// a factory that re-dials the same URL on every `Broken`.
fn managed_transport_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
) -> Result<ManagedTransport<SrtTransport>, SrtError> {
    if url.mode == Mode::Listener {
        return Err(callers_only());
    }
    policy.validate().map_err(policy_refused)?;
    // The silent-peer window belongs to the wrapper, not to the socket:
    // every SRT open refuses a config that carries it, so the first dial
    // and every re-dial go without it.
    let mut url = url.clone();
    let window = url.config.silent_peer_timeout.take();
    let initial = url.connect()?;
    let managed = ManagedTransport::new(
        initial,
        move || url.connect().map_err(open_error_to_transport),
        policy,
    )
    .map_err(policy_refused)?;
    // Armed before the shell exists, so before any send.
    Ok(match window {
        Some(w) => managed.with_silent_peer_timeout(w),
        None => managed,
    })
}

/// The observers for a send-side decorator, taken BEFORE it moves into a
/// shell. Senders record no end reason: a fresh, never-set handle. The
/// [`ManagedStatsHandle`] rides alongside because the gap-buffer counters
/// it snapshots (`gap_len`, `gap_messages_dropped`, `gap_bytes_dropped`)
/// exist only on the send side and every binding's `reconnect_stats()`
/// reads them — `MuxSender` exposes no transport accessor, so the handle
/// is unreachable once the shell owns the transport.
fn send_handles(managed: &ManagedTransport<SrtTransport>) -> (ManagedHandles, ManagedStatsHandle) {
    let handles = ManagedHandles {
        // Always `Some` — `ManagedTransport::cancel_handle` builds a
        // `ManagedCancel` unconditionally.
        cancel: managed
            .cancel_handle()
            .expect("ManagedTransport::cancel_handle is always Some"),
        end_reason: EndReasonHandle::default(),
        reconnects: managed.reconnects_handle(),
        attempts: managed.attempts_handle(),
        reconnecting: managed.reconnecting_handle(),
    };
    (handles, managed.stats_handle())
}

/// Open `url` (caller mode only) as a reconnecting mux sender for
/// `config`, and return it with its [`ManagedHandles`] and the
/// [`ManagedStatsHandle`] (gap-buffer + reconnect telemetry).
///
/// # Errors
///
/// - [`SrtError::Option`] — `?mode=listener` (refused before connecting).
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - [`SrtError::Connect`] — the initial connect failed.
/// - [`SrtError::Mux`] — `config` was rejected by the muxer; the connected
///   transport is dropped.
pub fn managed_mux_sender_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    config: MuxerConfig,
) -> Result<
    (
        MuxSender<ManagedTransport<SrtTransport>>,
        ManagedHandles,
        ManagedStatsHandle,
    ),
    SrtError,
> {
    let managed = managed_transport_from_url(url, policy)?;
    let (handles, stats) = send_handles(&managed);
    let tx = MuxSender::new(managed, config)?;
    Ok((tx, handles, stats))
}

/// Open `url` (caller mode only) as a reconnecting TS-bytes sender with
/// `config` (framing mode + unsynced watchdog; `SenderConfig::default()`
/// is RECOVER framing with an 18 800-byte watchdog — what Python and the
/// JVM pass; C forwards its `tst_sender_config_t`).
///
/// # Errors
///
/// - [`SrtError::Option`] — `?mode=listener` (refused before connecting).
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - [`SrtError::Connect`] — the initial connect failed.
pub fn managed_sender_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    config: SenderConfig,
) -> Result<
    (
        Sender<ManagedTransport<SrtTransport>>,
        ManagedHandles,
        ManagedStatsHandle,
    ),
    SrtError,
> {
    let managed = managed_transport_from_url(url, policy)?;
    let (handles, stats) = send_handles(&managed);
    Ok((Sender::new(managed, config), handles, stats))
}

/// Open `url` (caller mode only) as a reconnecting raw-bytes sender (C's
/// `tst_managed_raw_sender`) for `config`.
///
/// # Errors
///
/// - [`SrtError::Option`] — `?mode=listener` (refused before connecting).
/// - [`SrtError::Option`] — the policy is zero backoff with unlimited attempts (refused before connecting).
/// - [`SrtError::Connect`] — the initial connect failed.
pub fn managed_raw_sender_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    config: RawSenderConfig,
) -> Result<
    (
        RawSender<ManagedTransport<SrtTransport>>,
        ManagedHandles,
        ManagedStatsHandle,
    ),
    SrtError,
> {
    let managed = managed_transport_from_url(url, policy)?;
    let (handles, stats) = send_handles(&managed);
    Ok((RawSender::new(managed, config), handles, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed_out() -> Result<(), SrtError> {
        Err(SrtError::Connect(crate::SrtConnectError::TimedOut))
    }

    #[test]
    fn a_failure_under_a_cancelled_slot_is_the_cancel() {
        let slot = FactoryCancel::new();
        assert!(matches!(
            failure_under_cancel(timed_out(), &slot),
            Err(SrtError::Connect(_))
        ));
        slot.cancel();
        assert!(matches!(
            failure_under_cancel(timed_out(), &slot),
            Err(SrtError::Transport(TransportError::ExplicitClose))
        ));
        assert!(
            failure_under_cancel(Ok(()), &slot).is_ok(),
            "a success wins"
        );
    }
}
