package org.tstrans;

import java.util.concurrent.atomic.AtomicLong;

/**
 * Abstract base for all JVM objects that own a native registry handle. Centralises the
 * double-free-critical {@code getAndSet(0)}-on-close idiom that was previously hand-copied
 * into every resource class.
 *
 * <p><b>Implementation detail.</b> This class is not part of the stable public API surface
 * and may change between releases. Users should program to the concrete subclass types
 * ({@link org.tstrans.mpegts.Muxer}, {@link org.tstrans.mpegts.Demuxer}, etc.).
 *
 * <p><b>Handle field shape.</b> The backing field is an {@link AtomicLong}: all concrete
 * subclasses are documented to support at least one cross-thread operation ({@code close()}
 * or an explicit cross-thread cancel), so {@code AtomicLong} is the appropriate minimum
 * visibility shape. {@link #consumeHandle()} and {@link #close()} use
 * {@link AtomicLong#getAndSet} so exactly one caller claims the handle; all subsequent
 * callers see 0 and either get a clean {@link IllegalStateException} or a no-op.
 *
 * <p><b>Subclass contract.</b>
 * <ol>
 *   <li>Call {@link #setHandle(long)} once from the constructor with the native registry key.
 *   <li>Guard every public method with {@link #ensureOpen(String)} or
 *       {@link #requireOpen(String)}, then read the value via {@link #peekHandle()}.
 *   <li>For consuming-native sites (ownership transfer — e.g. {@code intoSender()}):
 *       call {@link #consumeHandle()} to atomically zero-and-capture the handle
 *       <em>before</em> the native call; a subsequent {@link #close()} then finds 0
 *       and is a harmless no-op. A consuming native that validates its other
 *       arguments before it takes the registry entry (so a refusal leaves the
 *       resource with the caller) instead passes {@link #peekHandle()} and calls
 *       {@code consumeHandle()} after it returns; the registry hands the entry to
 *       exactly one taker and never reuses a key, so a racing {@link #close()}
 *       stays a no-op or wins the entry outright. Conversely, a native that only <em>borrows</em>
 *       the handle without transferring ownership (a <b>sanctioned lease</b> —
 *       e.g. {@code RtspSession.intoDemuxReceiver()}, which moves the internal
 *       data plane but leaves the session's control plane usable) reads via
 *       {@link #peekHandle()} and does NOT consume; single-use enforcement for
 *       such sites lives in the Rust layer (a second call throws), not in the
 *       handle lifecycle.
 *   <li>Implement {@link #nativeClose(long)} by delegating to the subclass's own
 *       {@code private static native void nClose(long)} — JNI export names remain
 *       per-subclass, preserving the Maven ABI. A subclass whose native teardown
 *       must also hand back a close-time value it can no longer query afterward
 *       (the handle is 0 by the time {@link #nativeClose} runs — e.g.
 *       {@code org.tstrans.rtp}'s receivers snapshotting their end-of-stream
 *       reason) declares {@code nClose} to return that value instead of
 *       {@code void} and caches it in the override; this stays within the
 *       "exactly one native call, {@code null}-checked" spirit of this hook.
 * </ol>
 */
public abstract class NativeHandle implements AutoCloseable {

    // registry key; 0 = closed or consumed
    private final AtomicLong handle = new AtomicLong();

    /**
     * Set the registry key. Call exactly once from the subclass constructor, after the
     * native open call that produces the key.
     */
    protected final void setHandle(long h) {
        handle.set(h);
    }

    /**
     * Return the current handle without consuming it. Does not check for closed state;
     * callers must guard with {@link #ensureOpen(String)} first (or use
     * {@link #requireOpen(String)} when the value is needed in the same expression).
     */
    protected final long peekHandle() {
        return handle.get();
    }

    /**
     * Atomically claim the handle value (write 0, return the previous value). Used by
     * {@link #close()} and by consuming-native sites that transfer handle ownership
     * to a native API. Returns 0 if the handle was already consumed or closed.
     *
     * <p>At consuming-native sites, call this <em>before</em> the native call so the Java
     * field is zeroed first, unless the native validates before its take (see the
     * class contract).
     */
    protected final long consumeHandle() {
        return handle.getAndSet(0);
    }

    /**
     * Throw {@link IllegalStateException} if the handle is 0 (closed or consumed).
     *
     * @param closedMessage the exception message, typically {@code "ClassName is closed"}
     */
    protected final void ensureOpen(String closedMessage) {
        if (handle.get() == 0) throw new IllegalStateException(closedMessage);
    }

    /**
     * Return the current handle value, or throw {@link IllegalStateException} if closed.
     * Useful when the handle value is needed in the same expression as the open check
     * (e.g. {@code nCancel(requireOpen("CancelHandle is closed"))}).
     *
     * @param closedMessage the exception message
     * @return the current non-zero handle value
     */
    protected final long requireOpen(String closedMessage) {
        long h = handle.get();
        if (h == 0) throw new IllegalStateException(closedMessage);
        return h;
    }

    /**
     * Called by {@link #close()} with the non-zero handle value after it has been
     * atomically claimed. Subclasses forward to their own {@code nClose(long)} native.
     */
    protected abstract void nativeClose(long h);

    /**
     * Claim the handle atomically and invoke {@link #nativeClose} if it was non-zero.
     * Idempotent: a second call is always a no-op (the handle is already 0). Safe to
     * call concurrently with any other operation that reads the handle through the
     * leased {@code HandleRegistry} — a racing read either completes normally or throws
     * a clean {@link IllegalStateException}; neither outcome is undefined behaviour.
     *
     * @implSpec Subclasses that override this method (e.g. to add {@code synchronized})
     *           must call {@code super.close()} exactly once and add no native
     *           interaction that claims or frees the handle — the atomic claim in this
     *           base implementation is the double-free guarantee, and bypassing or
     *           duplicating it breaks the idempotency contract above. A native call
     *           that claims and frees nothing is allowed. One exception to calling
     *           {@code super.close()}: a close that is deferred because the resource is
     *           held further up the calling thread may return without it, provided
     *           the close is later completed through {@code super.close()} (see
     *           {@code org.tstrans.shells.DemuxReceiver#close()}).
     */
    @Override
    public void close() {
        long h = handle.getAndSet(0);
        if (h != 0) nativeClose(h);
    }
}
