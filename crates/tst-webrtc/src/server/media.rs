//! The server's media thread.

/// Wakes the media thread after a mount queued a message for it.
///
/// This build has no media thread, so every `Waker` is the inert one a
/// mount driven without a server uses, and `wake` does nothing. The
/// server's media thread brings a waker that signals that thread.
#[derive(Clone, Debug)]
pub(crate) struct Waker {
    _inert: (),
}

impl Waker {
    /// A waker that wakes nothing, for a mount driven without a server.
    pub(crate) fn for_test() -> Self {
        Self { _inert: () }
    }

    /// Signal the media thread that a message is queued.
    pub(crate) fn wake(&self) {}
}
