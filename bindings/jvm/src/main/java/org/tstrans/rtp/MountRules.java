package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;

/**
 * One {@link RtspServer}'s live per-path rules, from {@link RtspServer#mountRules()}. An
 * edit applies from the next request on. A rule's path must be canonical ({@code /amber},
 * never {@code /amber/}); {@code set} refuses anything else.
 * A path with no rule uses the default rule, which starts as the config's {@link
 * RtspServerConfig#defaultMountRule()} or {@link MountPolicy#inherit()} for both
 * directions.
 *
 * <p><b>Threading.</b> Every method is safe from any thread while the server runs.
 *
 * <p><b>Lifetime.</b> This object shares the server's table but not its lifetime: closing
 * the server does not close it. Once the server has stopped, been cancelled or been
 * closed, the editing methods throw
 * {@link RtspException} with {@code Kind.CLOSED}, while {@link #size()}, {@link
 * #isClosed()} and the refused-credential counts keep answering. Close it to free its
 * native handle.
 */
public final class MountRules extends NativeHandle {
    static { NativeLoader.load(); }

    MountRules(long h) { setHandle(h); }

    /**
     * Install {@code rule} for {@code path}, replacing any previous rule for it.
     *
     * @throws RtspException {@code MOUNT} for an invalid path, or a {@link
     *     MountPolicy#require} credential with an empty username or a username listed
     *     twice; {@code CLOSED} once
     *     the server has stopped
     */
    public void set(String path, MountRule rule) throws RtspException {
        Objects.requireNonNull(path, "path");
        Objects.requireNonNull(rule, "rule");
        ensureOpen("MountRules is closed");
        MountPolicy p = rule.publish(), r = rule.read();
        nSet(peekHandle(), path, p.kind(), p.statusCode(), p.usernames(), p.passwords(),
            r.kind(), r.statusCode(), r.usernames(), r.passwords());
    }

    /**
     * Remove the rule for {@code path}.
     *
     * @return {@code true} if a rule was installed for it; a path that is not a valid
     *     mount path never has one, so it returns {@code false}
     * @throws RtspException {@code CLOSED} once the server has stopped
     */
    public boolean remove(String path) throws RtspException {
        Objects.requireNonNull(path, "path");
        ensureOpen("MountRules is closed");
        return nRemove(peekHandle(), path);
    }

    /**
     * Replace the rule for paths no rule names.
     *
     * @throws RtspException as {@link #set}, apart from the path check
     */
    public void setDefault(MountRule rule) throws RtspException {
        Objects.requireNonNull(rule, "rule");
        ensureOpen("MountRules is closed");
        MountPolicy p = rule.publish(), r = rule.read();
        nSet(peekHandle(), null, p.kind(), p.statusCode(), p.usernames(), p.passwords(),
            r.kind(), r.statusCode(), r.usernames(), r.passwords());
    }

    /**
     * Remove every path rule; the default rule stays.
     *
     * @throws RtspException {@code CLOSED} once the server has stopped
     */
    public void clear() throws RtspException {
        ensureOpen("MountRules is closed");
        nClear(peekHandle());
    }

    /**
     * Choose the challenge every 401 of the server sends, and the scheme and realm a
     * credential is checked in: a {@link MountPolicy#require} rule's, and on a path whose
     * rule resolves to {@link MountPolicy#inherit()} the config's own {@link
     * RtspServerConfig#auth()} credential too. It takes effect on every server, one whose
     * config carries a credential included (the challenge starts as that credential's
     * scheme and realm). A user name that contains {@code :} cannot authenticate while the
     * challenge is Basic, the server's own credential included.
     *
     * @throws RtspException {@code CLOSED} once the server has stopped
     */
    public void setChallenge(AuthScheme scheme, String realm) throws RtspException {
        Objects.requireNonNull(scheme, "scheme");
        Objects.requireNonNull(realm, "realm");
        ensureOpen("MountRules is closed");
        nSetChallenge(peekHandle(), scheme.code(), realm);
    }

    /**
     * The requests refused for their credential on {@code path}'s own rule (see {@link
     * MountRuleAuthFailures}). A request for a path without a rule counts on the default
     * rule. A rule's counts are kept when {@link #set} replaces it and dropped by {@link
     * #remove} and {@link #clear}. Answers after the server has stopped.
     *
     * @return empty when {@code path} has no rule of its own
     */
    public Optional<MountRuleAuthFailures> authFailures(String path) {
        Objects.requireNonNull(path, "path");
        ensureOpen("MountRules is closed");
        long[] c = nAuthFailures(peekHandle(), path);
        return c == null ? Optional.empty() : Optional.of(new MountRuleAuthFailures(c[0], c[1]));
    }

    /**
     * The requests refused for their credential on the default rule: those for every path
     * without a rule of its own (see {@link MountRuleAuthFailures}). Answers after the server
     * has stopped.
     */
    public MountRuleAuthFailures defaultAuthFailures() {
        ensureOpen("MountRules is closed");
        long[] c = nDefaultAuthFailures(peekHandle());
        return new MountRuleAuthFailures(c[0], c[1]);
    }

    /** Number of path rules (the default rule is not counted). */
    public int size() {
        ensureOpen("MountRules is closed");
        return nSize(peekHandle());
    }

    /** Whether the server has shut down, so that edits throw {@code Kind.CLOSED}. */
    public boolean isClosed() {
        ensureOpen("MountRules is closed");
        return nIsClosed(peekHandle());
    }

    /** Free the native handle. Idempotent. The server is unaffected. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    // `pubKind`/`readKind` are MountPolicy.KIND_*; a null `path` sets the default rule.
    private static native void nSet(long rules, String path,
        int pubKind, int pubStatus, String[] pubUsers, String[] pubPasswords,
        int readKind, int readStatus, String[] readUsers, String[] readPasswords)
        throws RtspException;
    private static native boolean nRemove(long rules, String path) throws RtspException;
    private static native void nClear(long rules) throws RtspException;
    private static native void nSetChallenge(long rules, int scheme, String realm)
        throws RtspException;
    // `{publish, read}`; null when the path has no rule of its own.
    private static native long[] nAuthFailures(long rules, String path);
    private static native long[] nDefaultAuthFailures(long rules);
    private static native int nSize(long rules);
    private static native boolean nIsClosed(long rules);
    private static native void nClose(long rules);
}
