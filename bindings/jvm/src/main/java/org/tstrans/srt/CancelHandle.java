package org.tstrans.srt;

import org.tstrans.NativeHandle;

/**
 * Cancel handle for a {@link Sender} / {@link Receiver} / {@link Listener}.
 * {@link #cancel()} wakes a thread parked in {@code sendBytes}/{@code recvBytes}/
 * {@code accept} within a few ms; that call then throws
 * {@link org.tstrans.SrtException} with kind {@code CLOSED} (message
 * {@code cancelled from another thread}).
 * {@link #isCancelled()} reflects the shell's one cancel state: {@code true}
 * once {@link #cancel()} was called on ANY handle of the shell, or the shell was
 * {@code close()}d (close cancels first). All handles of one shell agree, and a
 * handle outlives its shell's {@code close()} harmlessly (further
 * {@code cancel()} calls are no-ops).
 *
 * <p>The native handle is an {@link java.util.concurrent.atomic.AtomicLong}
 * registry key; {@link #close()} claims it atomically with {@code getAndSet(0)},
 * and the leased {@code HandleRegistry} guarantees no use-after-free or
 * double-free for any native call concurrent with {@code close()} — a
 * use-after-close is a clean {@link IllegalStateException}, never UB. The methods
 * remain {@code synchronized} only to keep the per-handle {@code isCancelled()}
 * observation flag consistent.
 */
public final class CancelHandle extends NativeHandle {
    static { org.tstrans.NativeLoader.load(); }

    CancelHandle(long h) { setHandle(h); }

    /** Signal cancellation. Idempotent. */
    public synchronized void cancel() { nCancel(requireOpen("CancelHandle is closed")); }

    /** True once the shell was cancelled — by this or any other handle, or by {@code close()}. */
    public synchronized boolean isCancelled() {
        return nIsCancelled(requireOpen("CancelHandle is closed"));
    }

    // Preserve synchronized semantics for the cancel/isCancelled/close coordination contract.
    @Override public synchronized void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nCancel(long handle);
    private static native boolean nIsCancelled(long handle);
    private static native void nClose(long handle);
}
