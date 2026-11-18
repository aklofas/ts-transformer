package org.tstrans.rtp;

import org.tstrans.NativeHandle;

/**
 * Hard-cancel handle for an {@link RtspServer}. {@link #cancel()} sets one flag and
 * returns; it sends no Notice 5402 and waits for no drain. From that moment the
 * server refuses new work with {@code RtspException(CLOSED)}: {@link MountHandle}
 * pushes, mount registration and {@link RtspServer#removeMount(String)}, {@link
 * MountRules} edits and a new {@link RtspServer#nextPublisher(long)}. Within about
 * 100 ms the server notices the cancel and ends what runs: a parked {@code
 * nextPublisher} throws {@code CLOSED}, every session's connection is closed, the
 * listener stops accepting, multicast senders stop, and every publish mount's
 * receiver reads end of stream. A later {@link RtspServer#stop()} or {@link
 * RtspServer#close()} is not graceful either. A cancel fired while a {@code stop()}
 * or {@code close()} runs does not shorten that call (its Notice writes, its drain
 * wait and its connection shutdowns do not read the cancel): for an end without the
 * Notice, cancel before {@code close()}. Mirrors tst-py {@code
 * tstrans.rtp.RtspServerCancelHandle}. All handles obtained from one server share the
 * same backing flag.
 *
 * <p>The native handle is an {@link java.util.concurrent.atomic.AtomicLong}
 * registry key; {@link #close()} claims it atomically with {@code getAndSet(0)},
 * and the leased {@code HandleRegistry} guarantees no use-after-free or
 * double-free for any native call concurrent with {@code close()} — a
 * use-after-close is a clean {@link IllegalStateException}, never UB. The methods
 * remain {@code synchronized} only to keep the per-handle {@code isCancelled()}
 * observation flag consistent.
 */
public final class RtspServerCancelHandle extends NativeHandle {
    static { org.tstrans.NativeLoader.load(); }

    RtspServerCancelHandle(long h) { setHandle(h); }

    /** Signal hard cancellation. Idempotent. */
    public synchronized void cancel() {
        nCancel(requireOpen("RtspServerCancelHandle is closed"));
    }

    /** True once {@link #cancel()} was called on the backing flag. */
    public synchronized boolean isCancelled() {
        return nIsCancelled(requireOpen("RtspServerCancelHandle is closed"));
    }

    // Preserve synchronized semantics for the cancel/isCancelled/close coordination contract.
    @Override public synchronized void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nCancel(long handle);
    private static native boolean nIsCancelled(long handle);
    private static native void nClose(long handle);
}
