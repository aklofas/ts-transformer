//! A SIGUSR1 handler without `SA_RESTART`, shared by the handled-signal
//! (EINTR) tests of the RTP and RTSP client binaries. Unix only: the
//! including binary gates the `mod`.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Signals the handler has seen in this process.
pub static DELIVERED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_sigusr1(_: libc::c_int) {
    DELIVERED.fetch_add(1, Ordering::SeqCst);
}

/// Install a SIGUSR1 handler WITHOUT `SA_RESTART`, so an interrupted
/// blocking read returns `EINTR` on every Unix (Linux returns it even with
/// `SA_RESTART` once `SO_RCVTIMEO` is set; macOS needs the flag absent).
pub fn install_handler() {
    // SAFETY: a zeroed sigaction is valid; the handler touches one atomic.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigusr1 as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        assert_eq!(libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()), 0);
    }
}
