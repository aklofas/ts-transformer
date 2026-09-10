package org.tstrans.srt;

/**
 * Why a {@link ManagedDemuxReceiver}'s stream ended. Mirrors
 * {@code tst_pipeline::RecvEndReason} one-for-one.
 *
 * <p>Returned by {@link ManagedDemuxReceiver#endReason()}; {@code null} means
 * the stream either has not ended yet or ended through a path {@code
 * tst-pipeline} does not instrument. Recorded once, first-writer-wins, at
 * whichever site actually observed the terminal condition, so a later,
 * less-specific signal cannot clobber it.
 *
 * <p><b>Distinct from {@code org.tstrans.rtp.StreamEndReason}.</b> That enum
 * describes an RTSP/RTP session; this one describes a managed-reconnect SRT
 * receive. They are never interchanged. (The C ABI has no separate type and
 * folds these three into {@code TstStreamEndReason} lossily — {@code
 * RECONNECT_EXHAUSTED} arrives there as the vaguer {@code TransportFailed}.
 * This binding keeps the distinction.)
 *
 * <p><b>Which reasons are reachable today.</b> On the managed-SRT path a peer
 * FIN surfaces as a recoverable transport break that the reconnect decorator
 * retries, so a stream ends only when the reconnect budget runs out
 * ({@link #RECONNECT_EXHAUSTED}) or the caller stops it
 * ({@link #CANCELLED}). {@link #END_OF_STREAM} exists for a future receive
 * transport that can signal a clean end distinct from budget exhaustion; no
 * managed-SRT receiver produces it. Treat the set as open — this mirrors a
 * {@code #[non_exhaustive]} Rust enum, and {@link #fromWireOrdinal} maps any
 * value it does not recognize to {@code null} rather than failing.
 *
 * <p>The native ordinals ({@code 0}/{@code 1}/{@code 2}, in Rust declaration
 * order, with {@code -1} for "nothing recorded") are a private contract between
 * this enum and the {@code org.tstrans.srt} natives — unlike {@code
 * StreamEndReason}'s values, they are not pinned across the C and Python
 * surfaces. {@link #fromWireOrdinal} switches on them EXPLICITLY — never
 * {@code values()[ordinal]} / {@link Enum#ordinal()} — so reordering this
 * enum's declaration can never silently break the native contract.
 *
 * <p>Every reason is a bare cause with no free-text detail (the Rust variants
 * carry no message data), so there is no {@code endDetail()} companion the way
 * {@code org.tstrans.rtp} has one.
 */
public enum RecvEndReason {
    /** The underlying transport reported a genuine clean end-of-stream that was
     *  not the reconnect-budget-exhausted path. Not produced by the managed-SRT
     *  receiver today — see this enum's javadoc. */
    END_OF_STREAM,
    /** The reconnect decorator gave up after exhausting its
     *  {@link ReconnectPolicy} budget: the peer never came back within
     *  {@code maxAttempts}. */
    RECONNECT_EXHAUSTED,
    /** The caller fired the receiver's {@link CancelHandle} — not a wire-level
     *  failure. A bare {@link ManagedDemuxReceiver#close()} does not record this:
     *  srt {@code close()} waits for a parked recv instead of waking it, so the
     *  cancel handle is what actually stops an iteration in flight. */
    CANCELLED;

    /**
     * Map the native wire ordinal (0-2) to its constant. {@code -1} — or any
     * value this binding does not recognize, e.g. a future
     * {@code tst_pipeline::RecvEndReason} variant — maps to {@code null}.
     *
     * <p>Note {@code 0} is a real reason here, so {@code -1} (never {@code 0})
     * is the "nothing recorded" sentinel.
     */
    static RecvEndReason fromWireOrdinal(int ordinal) {
        switch (ordinal) {
            case 0: return END_OF_STREAM;
            case 1: return RECONNECT_EXHAUSTED;
            case 2: return CANCELLED;
            default: return null;
        }
    }
}
