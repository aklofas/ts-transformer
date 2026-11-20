package org.tstrans.internal;

import java.lang.reflect.Method;
import java.util.Set;
import org.tstrans.NativeHandle;

/**
 * jni-test-hooks probe: puts a native object into the state a racing {@code close()}
 * leaves for a call that already looked its handle up — the object taken, the handle
 * still registered — and reports whether a call holds a shell's slot.
 */
public final class SlotProbe {
    private SlotProbe() {}
    static { org.tstrans.NativeLoader.load(); }

    /**
     * Take the receiver out of an {@code H264Receiver}'s native slot and keep its handle
     * registered; {@code true} if a receiver was taken.
     */
    public static native boolean nEmptyH264ReceiverSlot(long handle);

    /**
     * Whether a call holds the slot of the shell registered as {@code handle} in the registry
     * of the Java class named {@code type} (handles are numbered per class). Never waits.
     * Throws {@code IllegalArgumentException} for a class with no probe.
     */
    public static native boolean nHeld(String type, long handle);

    /** The classes {@link #nHeld} has a registry for. */
    private static final Set<String> PROBED = Set.of("org.tstrans.shells.Sender",
        "org.tstrans.shells.Receiver", "org.tstrans.shells.MuxSender",
        "org.tstrans.shells.DemuxReceiver", "org.tstrans.tcp.Listener");

    /**
     * {@link #nHeld} for {@code shell}: whether a call holds its slot right now. A subclass (a
     * shell made from an accepted connection) is probed in its probed superclass's registry.
     */
    public static boolean held(NativeHandle shell) {
        Class<?> type = shell.getClass();
        while (type != null && !PROBED.contains(type.getName())) type = type.getSuperclass();
        if (type == null) throw new IllegalArgumentException("no slot probe for " + shell.getClass().getName());
        try {
            Method peek = NativeHandle.class.getDeclaredMethod("peekHandle");
            peek.setAccessible(true);
            return nHeld(type.getName(), (long) peek.invoke(shell));
        } catch (ReflectiveOperationException e) {
            throw new AssertionError("cannot read the handle of " + shell, e);
        }
    }
}
