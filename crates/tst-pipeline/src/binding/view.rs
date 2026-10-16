//! What every binding builds around an [`Owned`](super::Owned) shell, written
//! once: the construction-time [`Snapshot`] (the shell's error [`Domain`] and
//! its constant handles), the [`StreamHandles`] a `MuxSender` exposes, the
//! composing [`CancelLatch`], and the [`BlockingReason`] a reader names when
//! it must wait for the slot.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::{Transport, TransportCancel};

use super::Domain;
use crate::MuxSender;

/// The first configured stream handle of each kind — the `Owned` snapshot
/// of every `MuxSender`-shaped shell.
///
/// The stream set is fixed by the `MuxerProgramConfig` the sender is built
/// from (the muxer has no way to add a stream afterwards), so the handles
/// are read once, before the sender moves into its slot. The `*_handle()`
/// getters then never wait behind a `send_*` parked on a full send buffer
/// or in a Blocking reconnect.
#[derive(Debug, Clone, Copy)]
pub struct StreamHandles {
    /// The first video stream.
    pub video: Option<VideoStreamHandle>,
    /// The first KLV stream.
    pub klv: Option<KlvStreamHandle>,
    /// The first audio stream.
    pub audio: Option<AudioStreamHandle>,
    /// The first subtitle stream.
    pub subtitle: Option<SubtitleStreamHandle>,
    /// The first data stream.
    pub data: Option<DataStreamHandle>,
}

impl StreamHandles {
    /// Read the handles off a freshly built sender.
    pub fn of<T: Transport>(sender: &MuxSender<T>) -> Self {
        Self {
            video: sender.video_handles().into_iter().next(),
            klv: sender.klv_handles().into_iter().next(),
            audio: sender.audio_handles().into_iter().next(),
            subtitle: sender.subtitle_handles().into_iter().next(),
            data: sender.data_handles().into_iter().next(),
        }
    }
}

/// The construction-time snapshot of a shell: its error [`Domain`] and the
/// constant handles `H` it exposes. Read through
/// [`Owned::snapshot`](super::Owned::snapshot), never the slot.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<H = ()> {
    /// The shell's error domain.
    pub domain: Domain,
    /// The shell's construction-time constants.
    pub handles: H,
}

/// A cancel handle with its own latch, composed around a transport's.
///
/// [`cancel`](TransportCancel::cancel) sets this latch and then fires the
/// inner handle; [`is_cancelled`](Self::is_cancelled) reports THIS latch
/// only — "was this shell cancelled" — and deliberately does not OR in the
/// inner handle's, which `Owned::is_cancelled` already composes.
pub struct CancelLatch {
    inner: Arc<dyn TransportCancel>,
    cancelled: AtomicBool,
}

impl CancelLatch {
    /// Wrap `inner` with a fresh, unset latch.
    #[must_use]
    pub fn new(inner: Arc<dyn TransportCancel>) -> Self {
        Self {
            inner,
            cancelled: AtomicBool::new(false),
        }
    }

    /// `true` once THIS latch has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl TransportCancel for CancelLatch {
    fn cancel(&self) {
        // Flag first so a poll loop that wakes because of the forwarded
        // cancel already sees the state.
        self.cancelled.store(true, Ordering::Release);
        self.inner.cancel();
    }

    fn is_cancelled(&self) -> bool {
        CancelLatch::is_cancelled(self)
    }
}

/// Why a reader takes the shell slot and may wait behind a call parked in
/// it. A reader with an answer for "the slot is held" uses
/// [`Owned::try_with_ref`](super::Owned::try_with_ref) or
/// [`Owned::alive`](super::Owned::alive), and a construction constant reads
/// [`Owned::snapshot`](super::Owned::snapshot); only these two may block.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockingReason {
    /// The reader is itself a blocking operation; waiting for the slot is
    /// its contract.
    IsItselfBlocking,
    /// A dynamic read of state inside the shell with no meaningful answer
    /// while the slot is held (e.g. a stats snapshot).
    NoAnswerWithoutSlot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::Owned;
    use alloc::sync::Arc;
    use tst_core::cancel::FlagCancel;

    #[test]
    fn cancel_latch_reports_its_own_latch_and_fires_the_inner_handle() {
        let inner = Arc::new(FlagCancel::new());
        let latch = CancelLatch::new(inner.clone());
        assert!(!latch.is_cancelled());
        TransportCancel::cancel(&latch);
        assert!(latch.is_cancelled() && inner.is_cancelled());
        let other = CancelLatch::new(Arc::new(FlagCancel::new()));
        let inner2 = Arc::new(FlagCancel::new());
        inner2.cancel();
        assert!(
            !CancelLatch::new(inner2).is_cancelled(),
            "the inner handle's latch is not this latch"
        );
        drop(other);
    }

    #[test]
    fn alive_and_open_snapshot_never_take_a_held_slot() {
        let o = Owned::new(
            5u8,
            Arc::new(FlagCancel::new()),
            Snapshot {
                domain: crate::binding::Domain::Udp,
                handles: (),
            },
        );
        assert!(o.alive(|v| *v == 5));
        assert_eq!(
            o.open_snapshot().map(|s| s.domain),
            Some(crate::binding::Domain::Udp)
        );
        assert_eq!(
            o.with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |v| *v),
            Ok(5)
        );
        o.take();
        assert!(!o.alive(|_| true));
        assert!(o.open_snapshot().is_none());
    }
}
