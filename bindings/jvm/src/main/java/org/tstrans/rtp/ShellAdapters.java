package org.tstrans.rtp;

import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;

/**
 * Wraps the native keys the {@code org.tstrans.rtp} classes obtain into the
 * {@code org.tstrans.shells} classes their methods return. The shell constructors are {@code protected}, so
 * each wrapper is an anonymous subclass created here.
 */
final class ShellAdapters {
    private ShellAdapters() {}

    static CancelHandle cancelHandle(long key) { return new CancelHandle(key) {}; }

    static DemuxReceiver demuxReceiver(long handle) { return new DemuxReceiver(handle) {}; }
}
