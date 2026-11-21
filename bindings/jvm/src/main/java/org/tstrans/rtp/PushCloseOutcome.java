package org.tstrans.rtp;

import java.util.Objects;
import java.util.OptionalInt;

/**
 * How a push's close ended, as {@link RtspPushSession#closeOutcome()} reads it. A sender shell
 * that owns the push ({@code intoMuxSender}, {@code intoSender}) ends it in its
 * {@code finish()} or {@code close()}, which have no place for the server's answer; this is
 * where that answer is kept. Mirrors {@code tst_rtp::PushCloseOutcome}.
 *
 * <p>Immutable; two outcomes are equal when their kind, status and detail are.
 */
public final class PushCloseOutcome {
    /** The case of the close. */
    public enum Kind {
        /**
         * The server accepted the TEARDOWN (a {@code 2xx}, or {@code 454} for a session it no
         * longer knew) and no queued media is known lost.
         */
        CLEAN(0),
        /**
         * The push's cancel had fired: what was still queued was dropped and the TEARDOWN's
         * reply was not awaited. A sender shell's {@code close()} fires the cancel itself; its
         * {@code finish()} does not.
         */
        CANCELLED(1),
        /**
         * The server had forgotten the session and media queued by earlier data calls was
         * dropped before it was written, whatever the TEARDOWN was answered.
         */
        MEDIA_LOST(2),
        /** The server refused the TEARDOWN: {@link #status()} holds its status code. */
        TEARDOWN_REFUSED(3),
        /** The TEARDOWN was not written and answered within its 500&nbsp;ms bound. */
        TEARDOWN_TIMED_OUT(4),
        /**
         * The close failed another way, for instance on a connection that had ended:
         * {@link #detail()} holds the error's text.
         */
        FAILED(5);

        private final int code;

        Kind(int code) {
            this.code = code;
        }

        /** @return the value native code passes for this constant; stable across releases. */
        public int code() {
            return code;
        }

        static Kind fromCode(int code) {
            for (Kind k : values()) {
                if (k.code == code) return k;
            }
            throw new IllegalArgumentException("no PushCloseOutcome.Kind with code " + code);
        }
    }

    private final Kind kind;
    private final int status;
    private final String detail;

    // Built by the natives only: status -1 is none.
    private PushCloseOutcome(int kindCode, int status, String detail) {
        this.kind = Kind.fromCode(kindCode);
        this.status = status;
        this.detail = detail;
    }

    /** @return the case of the close */
    public Kind kind() { return kind; }

    /**
     * @return the RTSP status code of a refused TEARDOWN ({@link Kind#TEARDOWN_REFUSED});
     *     empty for every other kind
     */
    public OptionalInt status() {
        return status < 0 ? OptionalInt.empty() : OptionalInt.of(status);
    }

    /**
     * The server's reason phrase of a refused TEARDOWN, or the error's text of a
     * {@link Kind#FAILED} close; empty for every other kind. At most 256 code points,
     * each control character replaced by U+FFFD; the server chooses a reason phrase.
     *
     * @return the text, never {@code null}
     */
    public String detail() { return detail; }

    @Override public boolean equals(Object o) {
        return o instanceof PushCloseOutcome p && p.kind == kind && p.status == status
            && p.detail.equals(detail);
    }

    @Override public int hashCode() { return Objects.hash(kind, status, detail); }

    @Override public String toString() {
        StringBuilder sb = new StringBuilder("PushCloseOutcome[").append(kind);
        if (status >= 0) sb.append(", status=").append(status);
        if (!detail.isEmpty()) sb.append(", detail=").append(detail);
        return sb.append(']').toString();
    }
}
