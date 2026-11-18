package org.tstrans.rtp;

import org.tstrans.NativeHandle;

/**
 * RTSP control-plane cancel handle. {@link #cancel()} breaks any in-flight
 * {@code pause}/{@code play}/{@code teardown} on the originating session out of
 * blocking I/O at the next poll (typically &lt;100&nbsp;ms); that call then throws
 * {@link org.tstrans.RtspException} of kind {@code CLOSED}. A handle exists only once
 * {@code RtspClient.connect} has returned the session, so it cannot reach a connect.
 * Mirrors tst-py {@code tstrans.rtp.RtspCancelHandle}. All handles obtained from one
 * session share the same backing flag, which {@link RtspSession#close()} also sets.
 *
 * <p>The native handle is an {@link java.util.concurrent.atomic.AtomicLong}
 * registry key; {@link #close()} claims it atomically with {@code getAndSet(0)},
 * and the leased {@code HandleRegistry} guarantees no use-after-free or
 * double-free for any native call concurrent with {@code close()} — a
 * use-after-close is a clean {@link IllegalStateException}, never UB. The methods
 * remain {@code synchronized} only to keep the per-handle {@code isCancelled()}
 * observation flag consistent.
 */
public final class RtspCancelHandle extends NativeHandle {
    static { org.tstrans.NativeLoader.load(); }

    RtspCancelHandle(long h) { setHandle(h); }

    /** Signal cancellation. Idempotent. */
    public synchronized void cancel() { nCancel(requireOpen("RtspCancelHandle is closed")); }

    /** True once {@link #cancel()} was called on the backing flag. */
    public synchronized boolean isCancelled() {
        return nIsCancelled(requireOpen("RtspCancelHandle is closed"));
    }

    // Preserve synchronized semantics for the cancel/isCancelled/close coordination contract.
    @Override public synchronized void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nCancel(long handle);
    private static native boolean nIsCancelled(long handle);
    private static native void nClose(long handle);
}
