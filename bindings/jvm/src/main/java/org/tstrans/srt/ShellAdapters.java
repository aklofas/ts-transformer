package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.Redact;
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
 * {@code fromUrl} factories, which accept only {@code srt://} URLs. The shell constructors are {@code protected}, so
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
     * Throws {@link SrtException} with kind {@code CONFIG_INVALID} unless {@code url}
     * is an {@code srt://} URL, as the 0.8 factories did. {@code cls} is the
     * deprecated class's simple name, used in the message.
     */
    static void requireSrt(String url, String cls) throws SrtException {
        if (!url.startsWith("srt://")) {
            throw new SrtException(SrtException.Kind.CONFIG_INVALID,
                "org.tstrans.srt." + cls + ".fromUrl requires an srt:// URL; use org.tstrans.shells."
                    + cls + ".open for other transports");
        }
    }

    /**
     * The {@link SrtException} an {@code srt://} open raised. An {@code srt://}
     * URL fails only in the SRT domain, so any other exception is a binding bug.
     */
    static SrtException srt(BindingException e, String url) {
        if (e instanceof SrtException s) {
            return s;
        }
        throw new IllegalStateException(
            "unexpected " + e.getClass().getSimpleName() + " opening " + Redact.url(url), e);
    }
}
