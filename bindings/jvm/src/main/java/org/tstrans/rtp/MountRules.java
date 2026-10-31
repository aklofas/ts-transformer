package org.tstrans.rtp;

import java.util.Objects;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;

/**
 * One {@link RtspServer}'s live per-path rules, from {@link RtspServer#mountRules()}. An
 * edit applies from the next request on. Matching is exact on the mount path; a path
 * with no rule uses the default rule, which starts as the config's {@link
 * RtspServerConfig#defaultMountRule()} or {@link MountPolicy#inherit()} for both
 * directions.
 *
 * <p><b>Threading.</b> Every method is safe from any thread while the server runs.
 *
 * <p><b>Lifetime.</b> This object shares the server's table but not its lifetime: closing
 * the server does not close it. Once the server has stopped, the editing methods throw
 * {@link RtspException} with {@code Kind.SERVER}, while {@link #size()} and {@link
 * #isClosed()} keep answering. Close it to free its native handle.
 */
public final class MountRules extends NativeHandle {
    static { NativeLoader.load(); }

    MountRules(long h) { setHandle(h); }

    /**
     * Install {@code rule} for {@code path}, replacing any previous rule for it.
     *
     * @throws RtspException {@code MOUNT} for an invalid path or a {@link
     *     MountPolicy#require} credential with an empty username; {@code SERVER} once
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
     * @return {@code true} if a rule was installed for it
     * @throws RtspException {@code MOUNT} for an invalid path; {@code SERVER} once the
     *     server has stopped
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
     * @throws RtspException {@code SERVER} once the server has stopped
     */
    public void clear() throws RtspException {
        ensureOpen("MountRules is closed");
        nClear(peekHandle());
    }

    /**
     * Choose the challenge a {@link MountPolicy#require} policy sends.
     *
     * @throws RtspException {@code SERVER} once the server has stopped
     */
    public void setChallenge(AuthScheme scheme, String realm) throws RtspException {
        Objects.requireNonNull(scheme, "scheme");
        Objects.requireNonNull(realm, "realm");
        ensureOpen("MountRules is closed");
        nSetChallenge(peekHandle(), scheme.code(), realm);
    }

    /** Number of path rules (the default rule is not counted). */
    public int size() {
        ensureOpen("MountRules is closed");
        return nSize(peekHandle());
    }

    /** Whether the server has stopped, so that edits throw {@code Kind.SERVER}. */
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
    private static native int nSize(long rules);
    private static native boolean nIsClosed(long rules);
    private static native void nClose(long rules);
}
