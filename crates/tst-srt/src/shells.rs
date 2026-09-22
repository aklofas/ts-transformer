//! URL → managed shell in one call: the `from_url` family (Arc 2 WP-A3,
//! ARCH-01).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Each function composes what the C, Python and JVM bindings each used
//! to compose by hand: build the reconnect factory from the parsed
//! [`SrtUrl`] (caller → [`SrtUrl::connect`], listener →
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
//!
//! Senders are caller-only: `?mode=listener` is refused before any
//! socket is touched (see `docs/project/deferred-features.md`, "SRT URL
//! `mode=listener` / `mode=rendezvous` dispatch"). Receivers accept both
//! modes.
//!
//! # Why the caller path uses [`SrtUrl::connect`], receivers included
//!
//! Every C caller-mode open — plain and managed, TS / raw / demux — goes
//! through `connect_srt`, which applies
//! [`SocketConfig::merge_sender_defaults`](crate::config::SocketConfig::merge_sender_defaults)
//! (`bindings/c/core/src/sender/connect.rs:30`; receiver call sites
//! `receiver/demux_receiver/managed.rs:220,231`, `receiver/ts_receiver.rs:94`,
//! `receiver/raw_receiver.rs:99`). A managed caller-mode RECEIVER
//! therefore runs today with the sender preset (15 s connect timeout, 5 s
//! linger, `Role::Sender`), and this family preserves that exactly —
//! [`SrtUrl::connect_recv`] (the overlay-only open) is for the plain
//! Python/JVM opens that never merged it, not for this family.

use std::sync::Arc;

use tst_core::mpegts::demux::DemuxerConfig;
use tst_core::mpegts::mux::MuxerConfig;
use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportError};
use tst_pipeline::binding::ManagedHandles;
use tst_pipeline::{
    FactoryCancel, ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
    ManagedStatsHandle, ManagedTransport, MuxSender, RawReceiver, RawReceiverConfig, RawSender,
    RawSenderConfig, Receiver, ReceiverConfig, ReconnectPolicy, RecvEndReasonHandle, Sender,
    SenderConfig,
};

use crate::error::{OptionError, SrtError};
use crate::transport::SrtTransport;
use crate::url::{Mode, SrtUrl};

/// Map an open-path error into the `TransportError` the managed
/// decorators' factories return. A `Transport` error from
/// [`SrtUrl::accept_one`] passes through unchanged — `ExplicitClose` must
/// survive so the decorator reports a caller-initiated close, not a wire
/// fault; `Broken { "bind: …" | "accept: …" }` is already the recoverable
/// shape. Everything else (the typed [`ConnectError`](crate::ConnectError)
/// from [`SrtUrl::connect`]) becomes the recoverable `Broken` the
/// reconnect loop retries, message-prefixed the way the bindings'
/// `connect_srt` did.
fn open_error_to_transport(e: SrtError) -> TransportError {
    match e {
        SrtError::Transport(t) => t,
        other => TransportError::Broken {
            msg: format!("connect: {other}"),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        },
    }
}

/// One open, dispatched on the URL's mode.
fn open(url: &SrtUrl, slot: &FactoryCancel) -> Result<SrtTransport, SrtError> {
    match url.mode {
        Mode::Caller => url.connect(),
        Mode::Listener => url.accept_one(slot),
    }
}

/// The receive-side reconnect decorator for `url`, opened now (blocking on
/// the first connect / accept) with a factory that re-opens the same URL
/// on every `Broken` and re-accepts through `factory_cancel`.
///
/// The `*_from_url` functions below call this with a fresh slot. Pass your
/// own — from a thread you control, with the slot shared beforehand — to
/// keep the FIRST accept cancellable too: a cancel before or during it
/// returns `Err(SrtError::Transport(TransportError::ExplicitClose))`.
///
/// # Errors
///
/// Whatever the initial [`SrtUrl::connect`] / [`SrtUrl::accept_one`]
/// returned; the managed transport is not built.
pub fn managed_recv_transport_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    factory_cancel: Arc<FactoryCancel>,
) -> Result<ManagedRecvTransport<SrtTransport>, SrtError> {
    let initial = open(url, &factory_cancel)?;
    let url = url.clone();
    let slot = Arc::clone(&factory_cancel);
    let factory: Box<dyn FnMut() -> Result<SrtTransport, TransportError> + Send> =
        Box::new(move || open(&url, &slot).map_err(open_error_to_transport));
    Ok(ManagedRecvTransport::new_with_factory_cancel(
        initial,
        factory,
        policy,
        factory_cancel,
    ))
}

/// The observers for a receive-side decorator, taken BEFORE it moves into
/// a shell. `end_reason` starts as a fresh never-set handle; the demux
/// shell replaces it with the one it records into.
fn recv_handles(managed: &ManagedRecvTransport<SrtTransport>) -> ManagedHandles {
    ManagedHandles {
        // Always `Some` by construction — `ManagedRecvTransport::cancel_handle`
        // builds a `ManagedRecvCancel` unconditionally. This is the ONE
        // `.expect` the ≥ 16 binding sites used to carry each.
        cancel: managed
            .cancel_handle()
            .expect("ManagedRecvTransport::cancel_handle is always Some"),
        end_reason: RecvEndReasonHandle::default(),
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
/// The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_demux_receiver_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
    demux: DemuxerConfig,
) -> Result<(ManagedDemuxReceiver<SrtTransport>, ManagedHandles), SrtError> {
    let managed = managed_recv_transport_from_url(url, policy, Arc::new(FactoryCancel::new()))?;
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
/// The initial open's error (see [`managed_recv_transport_from_url`]).
pub fn managed_receiver_from_url(
    url: &SrtUrl,
    policy: ReconnectPolicy,
) -> Result<(Receiver<ManagedRecvTransport<SrtTransport>>, ManagedHandles), SrtError> {
    let managed = managed_recv_transport_from_url(url, policy, Arc::new(FactoryCancel::new()))?;
    let handles = recv_handles(&managed);
    Ok((Receiver::new(managed, ReceiverConfig::default()), handles))
}

/// Open `url` (either mode) as a reconnecting raw-bytes receiver (C's
/// `tst_managed_raw_receiver`). No end reason is recorded: a fresh,
/// never-set handle.
///
/// # Errors
///
/// The initial open's error (see [`managed_recv_transport_from_url`]).
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
    let initial = url.connect()?;
    let url = url.clone();
    Ok(ManagedTransport::new(
        initial,
        move || url.connect().map_err(open_error_to_transport),
        policy,
    ))
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
        end_reason: RecvEndReasonHandle::default(),
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
/// [`SrtError::Option`] for `?mode=listener`; otherwise the initial
/// connect's [`SrtError::Connect`].
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
/// [`SrtError::Option`] for `?mode=listener`; otherwise the initial
/// connect's [`SrtError::Connect`].
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
