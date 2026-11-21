//! What every binding builds around an [`Owned`](super::Owned) shell, written
//! once: the construction-time [`Snapshot`] (the shell's error [`Domain`] and
//! its constant handles), the [`StreamHandles`] a `MuxSender` exposes and the
//! [`StreamHandlesCell`] that follows a reconfigure, the composing
//! [`CancelLatch`], and the [`BlockingReason`] a reader names when it must
//! wait for the slot.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::{Transport, TransportCancel};

use super::Domain;
use crate::MuxSender;

/// The first configured stream handle of each kind of a `MuxSender`-shaped
/// shell.
///
/// The stream set comes from the `MuxerConfig` the sender is built from and
/// changes only in [`MuxSender::reconfigure`], so the handles are read
/// outside the `*_handle()` getters: once before the sender moves into its
/// slot, and again after each reconfigure. A shell keeps them in a
/// [`StreamHandlesCell`] as its `Owned` snapshot; the getters then never
/// wait behind a `send_*` parked on a full send buffer or in a Blocking
/// reconnect.
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
    /// Read the handles off a sender: freshly built, or just reconfigured.
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

/// The stored value for "no stream of this kind". A packed handle sets
/// only its low eight bits, so this is never one.
const NO_HANDLE: u32 = u32::MAX;

fn store(slot: &AtomicU32, raw: Option<u32>) {
    slot.store(raw.unwrap_or(NO_HANDLE), Ordering::Relaxed);
}

fn load(slot: &AtomicU32) -> Option<u32> {
    let raw = slot.load(Ordering::Relaxed);
    (raw != NO_HANDLE).then_some(raw)
}

/// [`StreamHandles`] that can be replaced after a
/// [`MuxSender::reconfigure`]: the `Owned` snapshot of a `MuxSender`-shaped
/// shell.
///
/// [`get`](Self::get) takes no lock, so a `*_handle()` getter never waits
/// behind a call that holds the slot. The shell's `reconfigure` calls
/// [`refresh`](Self::refresh) after the sender accepted the new
/// configuration and before it releases the slot, so two reconfigures
/// cannot store their handles out of order.
///
/// Each kind is stored on its own: a `get` that races a `refresh` may
/// return some kinds of the old stream set and some of the new one.
pub struct StreamHandlesCell {
    video: AtomicU32,
    klv: AtomicU32,
    audio: AtomicU32,
    subtitle: AtomicU32,
    data: AtomicU32,
}

impl StreamHandlesCell {
    /// A cell that reads `handles` until the first [`refresh`](Self::refresh).
    #[must_use]
    pub fn new(handles: StreamHandles) -> Self {
        let cell = Self {
            video: AtomicU32::new(NO_HANDLE),
            klv: AtomicU32::new(NO_HANDLE),
            audio: AtomicU32::new(NO_HANDLE),
            subtitle: AtomicU32::new(NO_HANDLE),
            data: AtomicU32::new(NO_HANDLE),
        };
        cell.set(handles);
        cell
    }

    /// The handles last stored. Never waits.
    pub fn get(&self) -> StreamHandles {
        StreamHandles {
            video: load(&self.video).map(VideoStreamHandle::from_raw),
            klv: load(&self.klv).map(KlvStreamHandle::from_raw),
            audio: load(&self.audio).map(AudioStreamHandle::from_raw),
            subtitle: load(&self.subtitle).map(SubtitleStreamHandle::from_raw),
            data: load(&self.data).map(DataStreamHandle::from_raw),
        }
    }

    /// Store the handles `sender` has now. This takes the sender's own lock
    /// for the read: call it from inside the shell's slot, right after a
    /// successful [`MuxSender::reconfigure`].
    pub fn refresh<T: Transport>(&self, sender: &MuxSender<T>) {
        self.set(StreamHandles::of(sender));
    }

    fn set(&self, handles: StreamHandles) {
        store(&self.video, handles.video.map(VideoStreamHandle::raw));
        store(&self.klv, handles.klv.map(KlvStreamHandle::raw));
        store(&self.audio, handles.audio.map(AudioStreamHandle::raw));
        store(
            &self.subtitle,
            handles.subtitle.map(SubtitleStreamHandle::raw),
        );
        store(&self.data, handles.data.map(DataStreamHandle::raw));
    }
}

impl core::fmt::Debug for StreamHandlesCell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("StreamHandlesCell")
            .field(&self.get())
            .finish()
    }
}

/// The construction-time snapshot of a shell: its error [`Domain`] and the
/// handles `H` it exposes without the slot (constants, or a cell such as
/// [`StreamHandlesCell`]). Read through
/// [`Owned::snapshot`](super::Owned::snapshot), never the slot.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<H = ()> {
    /// The shell's error domain.
    pub domain: Domain,
    /// The shell's construction-time constants, or the cell that follows
    /// them.
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
    use tst_core::mpegts::mux::{
        KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
    };
    use tst_core::transport::TransportError;

    #[test]
    fn cancel_latch_reports_its_own_latch_and_fires_the_inner_handle() {
        let inner = Arc::new(FlagCancel::new());
        let latch = CancelLatch::new(inner.clone());
        assert!(!latch.is_cancelled());
        TransportCancel::cancel(&latch);
        assert!(latch.is_cancelled() && inner.is_cancelled());
        let inner2 = Arc::new(FlagCancel::new());
        inner2.cancel();
        assert!(
            !CancelLatch::new(inner2).is_cancelled(),
            "the inner handle's latch is not this latch"
        );
    }

    /// An inner handle that records whether its latch was already set when
    /// the forwarded cancel reached it.
    struct SeesLatch {
        latch: alloc::sync::Weak<CancelLatch>,
        saw_latch_set: Arc<AtomicBool>,
    }

    impl TransportCancel for SeesLatch {
        fn cancel(&self) {
            let set = self.latch.upgrade().is_some_and(|l| l.is_cancelled());
            self.saw_latch_set.store(set, Ordering::Release);
        }
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    #[test]
    fn cancel_latch_sets_its_latch_before_it_fires_the_inner_handle() {
        let saw = Arc::new(AtomicBool::new(false));
        let latch = Arc::new_cyclic(|weak| {
            CancelLatch::new(Arc::new(SeesLatch {
                latch: weak.clone(),
                saw_latch_set: saw.clone(),
            }))
        });
        TransportCancel::cancel(&*latch);
        assert!(
            saw.load(Ordering::Acquire),
            "a poll loop woken by the inner cancel must already see the latch"
        );
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

    /// A transport that takes everything.
    struct Sink;

    impl Transport for Sink {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    fn config(with_klv: bool) -> MuxerConfig {
        let mut program = MuxerProgramConfigBuilder::new(1, 0x1000);
        program.add_video(0x1011, VideoCodec::H264);
        if with_klv {
            program.add_klv(0x1031, KlvStreamType::PrivateData, true);
        }
        let mut b = MuxerConfig::builder();
        b.add_program(program.build());
        b.build().expect("valid muxer config")
    }

    fn sender(with_klv: bool) -> MuxSender<Sink> {
        MuxSender::new(Sink, config(with_klv)).expect("sender")
    }

    /// The five handles as packed values, in the struct's field order.
    fn fields(h: StreamHandles) -> [Option<u32>; 5] {
        [
            h.video.map(VideoStreamHandle::raw),
            h.klv.map(KlvStreamHandle::raw),
            h.audio.map(AudioStreamHandle::raw),
            h.subtitle.map(SubtitleStreamHandle::raw),
            h.data.map(DataStreamHandle::raw),
        ]
    }

    #[test]
    fn the_cell_reads_what_it_was_built_from() {
        // A different position per kind, so a kind stored in or read from
        // another kind's place shows.
        let mixed = StreamHandles {
            video: Some(VideoStreamHandle::pack(0, 1)),
            klv: Some(KlvStreamHandle::pack(1, 0)),
            audio: Some(AudioStreamHandle::pack(1, 2)),
            subtitle: Some(SubtitleStreamHandle::pack(2, 3)),
            data: Some(DataStreamHandle::pack(15, 15)),
        };
        assert_eq!(fields(StreamHandlesCell::new(mixed).get()), fields(mixed));

        let none = StreamHandles {
            video: None,
            klv: None,
            audio: None,
            subtitle: None,
            data: None,
        };
        assert_eq!(fields(StreamHandlesCell::new(none).get()), fields(none));

        // The first stream of the first program packs to zero: it is a
        // handle, not "none".
        let s = sender(true);
        let built = StreamHandles::of(&s);
        assert_eq!(built.video.map(VideoStreamHandle::raw), Some(0));
        assert!(built.klv.is_some() && built.audio.is_none());
        assert_eq!(fields(StreamHandlesCell::new(built).get()), fields(built));
    }

    #[test]
    fn a_refresh_after_a_reconfigure_that_adds_klv_reads_some() {
        let s = sender(false);
        let cell = StreamHandlesCell::new(StreamHandles::of(&s));
        assert_eq!(cell.get().klv, None);

        s.reconfigure(config(true)).expect("reconfigure");
        assert_eq!(cell.get().klv, None, "only a refresh changes the cell");
        cell.refresh(&s);

        let klv = s.klv_handles().into_iter().next();
        assert!(klv.is_some());
        assert_eq!(cell.get().klv, klv);
        assert_eq!(fields(cell.get()), fields(StreamHandles::of(&s)));
    }

    #[test]
    fn a_refresh_after_a_reconfigure_that_removes_klv_reads_none() {
        let s = sender(true);
        let cell = StreamHandlesCell::new(StreamHandles::of(&s));
        let video = cell.get().video;
        assert!(cell.get().klv.is_some() && video.is_some());

        s.reconfigure(config(false)).expect("reconfigure");
        assert!(cell.get().klv.is_some(), "only a refresh changes the cell");
        cell.refresh(&s);

        assert_eq!(cell.get().klv, None);
        assert_eq!(
            cell.get().video,
            video,
            "the surviving stream keeps its handle"
        );
    }

    #[test]
    fn the_cell_is_read_while_the_slot_is_held() {
        use std::sync::mpsc;
        let s = sender(false);
        let handles = StreamHandlesCell::new(StreamHandles::of(&s));
        let owned = Owned::new(
            s,
            Arc::new(FlagCancel::new()),
            Snapshot {
                domain: crate::binding::Domain::Udp,
                handles,
            },
        );
        let owned = &owned;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (answer_tx, answer_rx) = mpsc::channel();
        let deadline = tst_test_helpers::timing::scaled(std::time::Duration::from_secs(5));
        std::thread::scope(|scope| {
            // A binding's reconfigure: it refreshes the cell inside its slot
            // hold. This one then stays in the slot until the gate opens.
            let holder = scope.spawn(move || {
                owned.with_mut(|sender| {
                    sender.reconfigure(config(true)).expect("reconfigure");
                    owned.snapshot().handles.refresh(sender);
                    let _ = entered_tx.send(sender.klv_handles().into_iter().next());
                    // An error is the dropped sender: the gate is open.
                    let _ = gate_rx.recv();
                })
            });
            let added = entered_rx.recv_timeout(deadline);
            let held = owned.try_with_ref(|_| ()).is_none();
            scope.spawn(move || {
                let _ = answer_tx.send(owned.open_snapshot().map(|s| s.handles.get().klv));
            });
            let during = answer_rx.recv_timeout(deadline);
            // Open the gate before any assertion: the scope joins the holder.
            drop(gate_tx);
            assert!(holder.join().unwrap().is_ok());
            let added = added.expect("the holder took the slot");
            assert!(added.is_some(), "the reconfigure added a KLV stream");
            assert!(held, "the slot was free when the cell was read");
            assert_eq!(
                during.expect("the cell answered while the slot was held"),
                Some(added)
            );
        });
    }
}
