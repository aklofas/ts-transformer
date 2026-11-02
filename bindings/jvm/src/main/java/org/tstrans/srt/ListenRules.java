package org.tstrans.srt;

import java.util.Objects;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * One {@link Listener}'s per-stream-ID rules, from {@link Listener#rules()}.
 * The listener consults them during each SRT handshake; an edit takes effect
 * for the next handshake. Matching is exact on the stream ID; a caller with no
 * stream ID, or one that no rule names, gets the default rule, which starts as
 * {@link ListenRule#accept()}.
 *
 * <p><b>Threading.</b> Every method is safe from any thread, including while
 * another thread is parked in {@link Listener#accept(Integer)}: none of them
 * takes the listener's slot.
 *
 * <p><b>After close.</b> This is a view of the listener, not a resource of its
 * own (nothing to close). Once the listener is closed (or cancelled through
 * {@code cancelHandle()}), every method throws
 * {@link SrtException} with {@code Kind.CLOSED}. Argument faults (a null, a
 * malformed UTF-16 string such as an unpaired surrogate, a reject code below
 * 1000) are refused in Java with {@code NullPointerException} /
 * {@code IllegalArgumentException} before the listener is consulted; every
 * other call on a closed listener throws {@code SrtException(CLOSED)},
 * whatever the rule.
 */
public final class ListenRules {

    static { NativeLoader.load(); }

    private final long listener;

    ListenRules(long listener) { this.listener = listener; }

    /**
     * Install {@code rule} for callers whose stream ID is exactly {@code streamId},
     * replacing any previous rule for it.
     *
     * @throws NullPointerException if an argument is null
     * @throws IllegalArgumentException if {@code streamId} contains an unpaired
     *         UTF-16 surrogate
     * @throws SrtException {@code CONFIG_INVALID} for an invalid
     *         {@link ListenRule#acceptWith} passphrase; {@code CLOSED} once the
     *         listener is closed (reported before the rule is validated)
     */
    public void set(String streamId, ListenRule rule) throws SrtException {
        Objects.requireNonNull(streamId, "streamId");
        requireWellFormed(streamId, "streamId");
        Objects.requireNonNull(rule, "rule");
        nSet(listener, streamId, rule.kind, rule.passphrase, reasonCode(rule));
    }

    /**
     * Remove the rule for {@code streamId}.
     *
     * @return {@code true} if a rule was installed for it
     * @throws NullPointerException if {@code streamId} is null
     * @throws IllegalArgumentException if {@code streamId} contains an unpaired
     *         UTF-16 surrogate
     * @throws SrtException {@code CLOSED} once the listener is closed
     */
    public boolean remove(String streamId) throws SrtException {
        Objects.requireNonNull(streamId, "streamId");
        requireWellFormed(streamId, "streamId");
        return nRemove(listener, streamId);
    }

    /**
     * Replace the rule for callers no stream-ID rule matches.
     *
     * @throws SrtException as {@link #set}
     */
    public void setDefault(ListenRule rule) throws SrtException {
        Objects.requireNonNull(rule, "rule");
        nSetDefault(listener, rule.kind, rule.passphrase, reasonCode(rule));
    }

    /**
     * Remove every stream-ID rule; the default rule stays.
     *
     * @throws SrtException {@code CLOSED} once the listener is closed
     */
    public void clear() throws SrtException { nClear(listener); }

    /**
     * Number of stream-ID rules (the default rule is not counted).
     *
     * @throws SrtException {@code CLOSED} once the listener is closed
     */
    public int size() throws SrtException { return nSize(listener); }

    /**
     * JNI decodes a string with an unpaired surrogate lossily (to U+FFFD), so
     * distinct Java strings would name one rule; refuse it before the native.
     */
    static String requireWellFormed(String s, String what) {
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (Character.isHighSurrogate(c) && i + 1 < s.length()
                    && Character.isLowSurrogate(s.charAt(i + 1))) {
                i++;
            } else if (Character.isSurrogate(c)) {
                throw new IllegalArgumentException(
                    what + " contains an unpaired UTF-16 surrogate at index " + i);
            }
        }
        return s;
    }

    private static int reasonCode(ListenRule rule) {
        return rule.reason == null ? 0 : rule.reason.code();
    }

    // --- Natives: `kind` is ListenRule.KIND_*; `passphrase` is null unless acceptWith. ---

    private static native void nSet(long listener, String streamId, int kind, String passphrase,
                                    int rejectCode) throws SrtException;

    private static native void nSetDefault(long listener, int kind, String passphrase,
                                           int rejectCode) throws SrtException;

    private static native boolean nRemove(long listener, String streamId) throws SrtException;

    private static native void nClear(long listener) throws SrtException;

    private static native int nSize(long listener) throws SrtException;
}
