package org.tstrans.rtp;

import org.tstrans.BindingException;
import org.tstrans.RtpException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;

/**
 * Wraps the native keys the {@code org.tstrans.rtp} classes obtain into the
 * {@code org.tstrans.shells} classes their methods return, and narrows the
 * shells' {@link BindingException} to {@link RtpException} for the deprecated
 * {@code fromUrl} factories, which accept only {@code rtp://} URLs. The shell constructors are {@code protected}, so
 * each wrapper is an anonymous subclass created here.
 */
final class ShellAdapters {
    private ShellAdapters() {}

    static CancelHandle cancelHandle(long key) { return new CancelHandle(key) {}; }

    static DemuxReceiver demuxReceiver(long handle) { return new DemuxReceiver(handle) {}; }

    /**
     * Throws {@link RtpException} with kind {@code CONFIG_INVALID} unless {@code url}
     * is an {@code rtp://} URL, as the 0.8 factories did. {@code cls} is the
     * deprecated class's simple name, used in the message.
     */
    static void requireRtp(String url, String cls) throws RtpException {
        if (!url.startsWith("rtp://")) {
            throw new RtpException(RtpException.Kind.CONFIG_INVALID,
                "org.tstrans.rtp." + cls + ".fromUrl requires an rtp:// URL; use org.tstrans.shells."
                    + cls + ".open for other transports");
        }
    }

    /**
     * The {@link RtpException} an {@code rtp://} open raised. An {@code rtp://}
     * URL fails only in the RTP domain, so any other exception is a binding bug.
     */
    static RtpException rtp(BindingException e, String url) {
        if (e instanceof RtpException s) {
            return s;
        }
        throw new IllegalStateException(
            "unexpected " + e.getClass().getSimpleName() + " opening " + org.tstrans.Redact.url(url), e);
    }
}
