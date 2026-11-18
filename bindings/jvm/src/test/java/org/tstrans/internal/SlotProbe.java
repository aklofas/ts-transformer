package org.tstrans.internal;

/**
 * jni-test-hooks probe: puts a native object into the state a racing {@code close()}
 * leaves for a call that already looked its handle up — the object taken, the handle
 * still registered.
 */
public final class SlotProbe {
    private SlotProbe() {}
    static { org.tstrans.NativeLoader.load(); }

    /**
     * Take the receiver out of an {@code H264Receiver}'s native slot and keep its handle
     * registered; {@code true} if a receiver was taken.
     */
    public static native boolean nEmptyH264ReceiverSlot(long handle);
}
