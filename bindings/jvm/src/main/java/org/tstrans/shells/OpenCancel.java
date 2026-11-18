package org.tstrans.shells;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;

/**
 * A cancel for opens that have not returned a shell yet. A listener-mode {@code srt://}
 * open ({@link Receiver#open(String, OpenCancel)}, {@link DemuxReceiver#open(String,
 * org.tstrans.mpegts.DemuxerConfig, OpenCancel)}, {@code ManagedReceiver.fromUrl},
 * {@code ManagedDemuxReceiver.fromUrl}) blocks in its first accept until a peer connects,
 * and no {@link CancelHandle} exists until it returns. Make an {@code OpenCancel} first,
 * pass it to the open, and call {@link #cancel()} from another thread: the open throws
 * {@code SrtException} with kind {@code CLOSED} and releases its port.
 *
 * <p>The rules:
 * <ul>
 *   <li>One object may be given to any number of opens, one after another or at the same
 *       time; one {@code cancel()} reaches all of them.</li>
 *   <li>{@code cancel()} is sticky. Every open given the object after it throws the URL's
 *       transport exception with kind {@code CLOSED} before the URL is parsed, and opens
 *       nothing: no port is bound, no peer is dialled. The one exception is a URL whose
 *       scheme no shell opens: it still throws {@code IllegalArgumentException}, because
 *       there is no transport whose {@code CLOSED} could be thrown.</li>
 *   <li>It ends a listener-mode first accept at any time. It does not interrupt a
 *       caller-mode dial (every sender, and a caller-mode {@code ManagedDemuxReceiver}):
 *       the dial runs to its end, and an open that finishes after the cancel throws
 *       {@code CLOSED} whatever the dial did. A connection it had just made is closed
 *       again and nothing is handed out.</li>
 *   <li>It is not the shell's cancel handle. Once an open has returned, {@code cancel()}
 *       does nothing to the shell it returned (a managed receiver keeps reconnecting); use
 *       the shell's {@code cancelHandle()} from then on.</li>
 *   <li>{@link #close()} frees this Java object and nothing else: an open that is parked
 *       with it keeps running (a peer still completes it, and the object can no longer
 *       cancel it). After {@code close()}, {@link #cancel()} and {@link #isCancelled()}
 *       throw {@code IllegalStateException}, and an open given the closed object throws
 *       {@code IllegalStateException} too.</li>
 * </ul>
 *
 * <p>{@code cancel()} wakes each parked accept by closing its listening socket, which
 * can take a while; {@link #isCancelled()} and {@link #close()} on the same object do
 * not wait for a {@code cancel()} still running on another thread. Every method is safe
 * from any thread.
 *
 * <p>An open given {@code null} in place of the object behaves as the same open
 * without the argument.
 *
 * <p>Mirrors {@code tstrans.shells.OpenCancel} in tst-py.
 */
public final class OpenCancel extends NativeHandle {
    static { NativeLoader.load(); }

    /** A new object, not cancelled. */
    public OpenCancel() { setHandle(nNew()); }

    /**
     * End every open in progress with this object and refuse every later one. Idempotent.
     *
     * @throws IllegalStateException if this object is closed
     */
    public void cancel() { nCancel(requireOpen("OpenCancel is closed")); }

    /**
     * {@code true} once {@link #cancel()} has run.
     *
     * @return whether the object is cancelled
     * @throws IllegalStateException if this object is closed
     */
    public boolean isCancelled() { return nIsCancelled(requireOpen("OpenCancel is closed")); }

    /**
     * Free this object. Idempotent. An open parked with the object is not ended by it.
     */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    // Read by the open natives, which take the object as an argument.
    @SuppressWarnings("unused")
    private long openKey() { return requireOpen("OpenCancel is closed"); }

    private static native long nNew();
    private static native void nCancel(long handle);
    private static native boolean nIsCancelled(long handle);
    private static native void nClose(long handle);
}
