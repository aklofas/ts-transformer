package org.tstrans.rtp;

import org.tstrans.NativeHandle;

/**
 * Cross-thread cancel handle for an RTP {@link Sender} / {@link Receiver}.
 * {@link #cancel()} wakes a thread parked in {@code send}/{@code recv} within
 * ~100 ms; that call then throws {@link org.tstrans.RtpException} with kind
 * {@code CLOSED}. Mirrors tst-py {@code tstrans.rtp.CancelHandle} — which
 * exposes only {@code cancel()} (no {@code isCancelled}).
 *
 * <p>The native handle is an {@link java.util.concurrent.atomic.AtomicLong}
 * registry key; {@link #close()} claims it atomically with {@code getAndSet(0)},
 * and the leased {@code HandleRegistry} guarantees no use-after-free or
 * double-free for any native call concurrent with {@code close()} — a
 * use-after-close is a clean {@link IllegalStateException}, never UB.
 */
public final class CancelHandle extends NativeHandle {
    static { org.tstrans.NativeLoader.load(); }

    CancelHandle(long h) { setHandle(h); }

    /** Signal cancellation. Idempotent. */
    public synchronized void cancel() { nCancel(requireOpen("CancelHandle is closed")); }

    // Preserve synchronized semantics for the cancel/close coordination contract.
    @Override public synchronized void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nCancel(long handle);
    private static native void nClose(long handle);
}
