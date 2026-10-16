package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.SrtException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;

/**
 * Wraps the native keys the {@code org.tstrans.srt} classes obtain into the
 * {@code org.tstrans.shells} classes their methods return, and narrows the
 * shells' {@link BindingException} to {@link SrtException} for the deprecated
 * {@code fromUrl} factories. The shell constructors are {@code protected}, so
 * each wrapper is an anonymous subclass created here.
 */
final class ShellAdapters {
    private ShellAdapters() {}

    static CancelHandle cancelHandle(long key) { return new CancelHandle(key) {}; }

    static Sender sender(long handle) { return new Sender(handle) {}; }

    static Receiver receiver(long handle) { return new Receiver(handle) {}; }

    static MuxSender muxSender(long handle) { return new MuxSender(handle) {}; }

    static DemuxReceiver demuxReceiver(long handle) { return new DemuxReceiver(handle) {}; }

    /**
     * The {@link SrtException} an {@code srt://} open raised. An {@code srt://}
     * URL fails only in the SRT domain, so any other exception is a binding bug.
     */
    static SrtException srt(BindingException e, String url) {
        if (e instanceof SrtException s) {
            return s;
        }
        throw new IllegalStateException(
            "unexpected " + e.getClass().getSimpleName() + " opening " + url, e);
    }
}
