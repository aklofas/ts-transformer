package org.tstrans.srt;

import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;

/**
 * Wraps the native keys the {@code org.tstrans.srt} classes obtain into the
 * {@code org.tstrans.shells} classes their methods return. The shell constructors are {@code protected}, so
 * each wrapper is an anonymous subclass created here.
 */
final class ShellAdapters {
    private ShellAdapters() {}

    static CancelHandle cancelHandle(long key) { return new CancelHandle(key) {}; }

    static Sender sender(long handle) { return new Sender(handle) {}; }

    static Receiver receiver(long handle) { return new Receiver(handle) {}; }

    static MuxSender muxSender(long handle) { return new MuxSender(handle) {}; }

    static DemuxReceiver demuxReceiver(long handle) { return new DemuxReceiver(handle) {}; }
}
