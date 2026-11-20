package org.tstrans.srt;

/**
 * Internal flattening of a {@link ReconnectPolicy} into the primitive arguments
 * the {@code Managed*.nFromUrl} natives accept. Not a public API. {@code maxAttempts}
 * is split into a present-flag + value because a JNI primitive signature cannot
 * carry a nullable {@code Integer} and 0 is a valid attempt count (so no sentinel).
 */
record PolicyArgs(
        boolean maxAttemptsPresent,
        int maxAttempts,
        int backoffKind,      // 0 = constant, 1 = exponential
        long backoffBaseMs,
        long backoffMaxMs,
        int gapBufferCapacity,
        int overflowPolicy,   // OverflowPolicy.code(): 0 = DROP_OLDEST, 1 = REJECT
        int mode) {           // ReconnectMode.code(): 0 = BLOCKING, 1 = BACKGROUND

    /** Flatten {@code policy} (or {@link ReconnectPolicy#defaults()} when null). */
    static PolicyArgs from(ReconnectPolicy policy) {
        ReconnectPolicy p = (policy == null) ? ReconnectPolicy.defaults() : policy;
        BackoffStrategy b = p.backoff();
        boolean present = p.maxAttempts().isPresent();
        return new PolicyArgs(
            present,
            present ? p.maxAttempts().get() : 0,
            b.kind().equals("constant") ? 0 : 1,
            b.baseMs(),
            b.maxMs(),
            p.gapBufferCapacity(),
            p.overflowPolicy().code(),
            p.mode().code());
    }
}
