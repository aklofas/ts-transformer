package org.tstrans.srt;

import java.util.Objects;

/**
 * What a {@link Listener} does with a caller during the SRT handshake: install
 * one per stream ID with {@link ListenRules#set}, or for every other caller with
 * {@link ListenRules#setDefault}. Immutable.
 */
public final class ListenRule {

    // Native wire values for the rule kind (see ListenRules.nSet).
    static final int KIND_ACCEPT = 0;
    static final int KIND_ACCEPT_WITH = 1;
    static final int KIND_REJECT = 2;

    private static final ListenRule ACCEPT = new ListenRule(KIND_ACCEPT, null, null);

    final int kind;
    final String passphrase;
    final RejectReason reason;

    private ListenRule(int kind, String passphrase, RejectReason reason) {
        this.kind = kind;
        this.passphrase = passphrase;
        this.reason = reason;
    }

    /** Accept with the listener's own settings (libsrt's default behaviour). */
    public static ListenRule accept() { return ACCEPT; }

    /**
     * Accept, applying this passphrase to the connection. A caller whose key
     * material does not match is refused, and reads {@code BadSecret}. The key
     * length follows the caller's key material. The passphrase is validated
     * when the rule is installed ({@code CONFIG_INVALID} unless it is 10 to 80
     * bytes of UTF-8).
     */
    public static ListenRule acceptWith(String passphrase) {
        return new ListenRule(KIND_ACCEPT_WITH, Objects.requireNonNull(passphrase, "passphrase"), null);
    }

    /**
     * Refuse the caller during the handshake; it reads {@code reason}.
     *
     * @throws IllegalArgumentException if {@code reason.code() < 1000}: codes
     *         below 1000 belong to libsrt
     */
    public static ListenRule reject(RejectReason reason) {
        Objects.requireNonNull(reason, "reason");
        if (reason.code() < 1000) {
            throw new IllegalArgumentException(
                "reject reason " + reason.code() + " is reserved for libsrt (codes below 1000)");
        }
        return new ListenRule(KIND_REJECT, null, reason);
    }

    /** Never shows the passphrase. */
    @Override public String toString() {
        switch (kind) {
            case KIND_ACCEPT: return "ListenRule.accept()";
            case KIND_ACCEPT_WITH: return "ListenRule.acceptWith(passphrase=<redacted>)";
            default: return "ListenRule.reject(" + reason + ")";
        }
    }
}
