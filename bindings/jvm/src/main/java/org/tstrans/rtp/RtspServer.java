package org.tstrans.rtp;

import java.util.Optional;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;
import org.tstrans.mpegts.MuxerConfig;

/**
 * Sync RTSP server. Construct via {@link #start(RtspServerConfig)}. Mirrors tst-py
 * {@code tstrans.rtp.RtspServer}. The underlying {@code tst_rtp::RtspServer} owns a
 * tokio Runtime for its lifetime, held inside the native box — there is no
 * JNI-side async handling.
 *
 * <p><b>Closing:</b> {@link #close()} performs a graceful stop (RFC 7826 §13.5.1
 * Notice 5402 server-initiated teardown of active sessions) then frees the native
 * server. Use try-with-resources. {@link #stop(long)} is the explicit graceful
 * shutdown; {@link #cancelHandle()} returns a cross-thread hard-cancel. Only
 * {@code stop()} and {@code close()} end publish mounts' receivers and wake a
 * parked {@link #nextPublisher(long)}; the hard cancel does neither.
 *
 * <p><b>Publisher role:</b> {@link #addPublishMount(String)}, {@link
 * #nextPublisher(long)} and {@link #removeMount(String)} accept encoders that push
 * into the server with ANNOUNCE / RECORD; see {@link PublishMount}.
 *
 * <p><b>Failure isolation (rare):</b> if an internal panic occurs during a mutating
 * server operation (e.g. {@link #addUnicastMount} / {@link #stop}), the server entry
 * is invalidated and subsequent server calls throw {@link IllegalStateException}.
 * However, {@link MountHandle} objects already returned by a prior
 * {@code addUnicastMount} / {@code addMulticastMount} call are held in a separate
 * registry and are <em>not</em> immediately invalidated — their next push operation
 * will fail with a closed-channel error rather than throwing at the moment the server
 * is torn down. This is memory-safe; the failure surfaces at the next push call rather
 * than at server-invalidation time. This scenario requires an internal panic
 * mid-mutation, which is rare in normal operation.
 */
public final class RtspServer extends NativeHandle {
    static { NativeLoader.load(); }

    RtspServer(long h) { setHandle(h); }

    /**
     * Build, bind, and start a server from {@code config}.
     *
     * @throws RtspException {@code SERVER} on bind/start failure, {@code PROTOCOL}
     *     on bind-URL parse failure, {@code TLS} if {@code config.tlsCert()}/{@code
     *     tlsKey()} name a bad PEM path (an {@code rtsps://} bind now works)
     * @throws IllegalArgumentException if {@code config.auth()} is set without a realm
     */
    public static RtspServer start(RtspServerConfig config) throws RtspException {
        int authScheme = -1;
        String realm = null, user = null, password = null;
        Object auth = config.auth().orElse(null);
        if (auth instanceof BasicAuth b) {
            authScheme = 0;
            realm = b.realm().orElseThrow(() -> new IllegalArgumentException(
                "server-side BasicAuth requires a realm"));
            user = b.user();
            password = b.password();
        } else if (auth instanceof DigestAuth d) {
            authScheme = (d.algorithm() == DigestAlgorithm.SHA256) ? 2 : 1;
            realm = d.realm().orElseThrow(() -> new IllegalArgumentException(
                "server-side DigestAuth requires a realm"));
            user = d.user();
            password = d.password();
        }
        long h = nStart(
            config.bindAddr(),
            config.maxSessions(), config.sessionTimeoutSecs(),
            config.fanoutCapacity(), config.gracefulShutdownDrainMs(),
            authScheme, realm, user, password,
            config.tlsCert().orElse(null), config.tlsKey().orElse(null),
            config.acceptUnregisteredPublishers());
        if (h == 0) {
            throw new RtspException(RtspException.Kind.SERVER,
                "nStart returned 0 without throwing");
        }
        return new RtspServer(h);
    }

    /** Aggregate server stats snapshot. @throws IllegalStateException if closed. */
    public ServerStats stats() { ensureOpen(); return nStats(peekHandle()); }

    /** Bound listener address as {@code "ip:port"}, or {@code null} before bind. */
    public String localAddr() { ensureOpen(); return nLocalAddr(peekHandle()); }

    /**
     * Graceful shutdown — fires the Notice 5402 path on each active session, waits
     * the builder's drain window. Idempotent. {@code drainMs} is accepted for API
     * stability but the configured {@code gracefulShutdownDrainMs} governs the wait.
     *
     * @throws RtspException {@code SERVER} if the server was never started
     */
    public void stop(long drainMs) throws RtspException { ensureOpen(); nStop(peekHandle(), drainMs); }

    /** {@link #stop(long)} with the default drain hint (1000). */
    public void stop() throws RtspException { stop(1000L); }

    /**
     * Register a unicast mount under {@code path}. The returned {@link MountHandle}
     * is the push surface; it is shareable across producer threads.
     *
     * @throws RtspException {@code MOUNT} for an invalid/duplicate mount path; {@code SERVER}
     *     if the server is stopped
     * @throws MuxException if the muxer rejects {@code programConfig}
     */
    public MountHandle addUnicastMount(String path, MuxerConfig programConfig)
            throws RtspException, MuxException {
        ensureOpen();
        long h = nAddUnicastMount(peekHandle(),
            path,
            programConfig.programNumber(), programConfig.pmtPid(), programConfig.pcrPid(),
            programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(),
            programConfig.streamCodecs(), programConfig.streamTypeCodes(),
            programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens());
        if (h == 0) {
            throw new RtspException(RtspException.Kind.MOUNT,
                "nAddUnicastMount returned 0 without throwing");
        }
        return new MountHandle(h);
    }

    /** {@link #addMulticastMount(String, String, int, int, String, MuxerConfig)} with ttl=1, no iface. */
    public MountHandle addMulticastMount(String path, String group, int port,
            MuxerConfig programConfig) throws RtspException, MuxException {
        return addMulticastMount(path, group, port, 1, null, programConfig);
    }

    /**
     * Register a multicast mount. {@code group} is a literal multicast IP; {@code ttl}
     * defaults to 1 (link-local); {@code iface} pins the NIC (IPv4 literal / IPv6 iface
     * name), or {@code null}.
     *
     * @throws RtspException {@code MOUNT} for an invalid/duplicate mount path or invalid
     *     group/address; {@code SERVER} if the server is stopped
     * @throws MuxException if the muxer rejects {@code programConfig}
     */
    public MountHandle addMulticastMount(String path, String group, int port, int ttl,
            String iface, MuxerConfig programConfig) throws RtspException, MuxException {
        ensureOpen();
        long h = nAddMulticastMount(peekHandle(),
            path, group, port, ttl, iface,
            programConfig.programNumber(), programConfig.pmtPid(), programConfig.pcrPid(),
            programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(),
            programConfig.streamCodecs(), programConfig.streamTypeCodes(),
            programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens());
        if (h == 0) {
            throw new RtspException(RtspException.Kind.MOUNT,
                "nAddMulticastMount returned 0 without throwing");
        }
        return new MountHandle(h);
    }

    /**
     * Register a publish mount under {@code path}: the server accepts ANNOUNCE /
     * RECORD on it, and the returned {@link PublishMount} hands the received MPEG-TS
     * to the application ({@link PublishMount#intoDemuxReceiver()}) while PLAY
     * readers on the same path are re-served from it.
     *
     * @throws RtspException {@code MOUNT} for an invalid or duplicate path; {@code
     *     SERVER} if the server is stopped
     */
    public PublishMount addPublishMount(String path) throws RtspException {
        ensureOpen();
        long h = nAddPublishMount(peekHandle(), java.util.Objects.requireNonNull(path, "path"));
        if (h == 0) {
            throw new RtspException(RtspException.Kind.MOUNT,
                "nAddPublishMount returned 0 without throwing");
        }
        return new PublishMount(h);
    }

    /**
     * Wait up to {@code timeoutMs} for the next publish mount an ANNOUNCE created on
     * demand ({@link RtspServerConfig#acceptUnregisteredPublishers()}). The
     * announcing publisher already holds the returned mount. Mounts come out in
     * ANNOUNCE order, each to exactly one caller.
     *
     * <p>Returns empty when the timeout passes, and always does when on-demand
     * publishers are off. The wait holds no lock on this object: {@link #stats()},
     * {@link #close()} and every other method answer while it is parked, and {@link
     * #stop()} or {@link #close()} from another thread wakes it with {@code SERVER}.
     *
     * <p>Concurrent callers are served one at a time, so a call made while another
     * waits can return later than its own {@code timeoutMs}. The returned mount can
     * already have been removed by {@link #removeMount(String)} while it waited in
     * the queue: its receiver then reads end of stream at once; treat it as expired.
     *
     * @param timeoutMs how long to wait, in milliseconds ({@code 0} polls)
     * @throws IllegalArgumentException if {@code timeoutMs} is negative
     * @throws RtspException {@code SERVER} once the server has stopped, including
     *     while this call waited
     */
    public Optional<PublishMount> nextPublisher(long timeoutMs) throws RtspException {
        if (timeoutMs < 0) {
            throw new IllegalArgumentException("timeoutMs must be >= 0; got " + timeoutMs);
        }
        ensureOpen();
        long h = nNextPublisher(peekHandle(), timeoutMs);
        return h == 0 ? Optional.empty() : Optional.of(new PublishMount(h));
    }

    /**
     * Remove the mount at {@code path}, of any kind, and free the path. A publish
     * mount's publisher is sent the RTSP Notice 5402 and disconnected, and its
     * {@link DemuxReceiver} reads end of stream ({@code recvEvent()} returns {@code
     * null}); the {@link PublishMount} stays usable for {@code stats()}. A local
     * mount's {@link MountHandle} keeps accepting pushes that reach nobody. This is
     * also how idle on-demand mounts are removed: they stay registered after their
     * publisher leaves. Blocks for the Notice writes (at most 1 s per session), and
     * other calls on this server object wait behind it meanwhile, as they do behind
     * {@link #stop()}.
     *
     * @throws RtspException {@code MOUNT} when no mount is registered at {@code
     *     path}; {@code SERVER} if the server is stopped
     */
    public void removeMount(String path) throws RtspException {
        ensureOpen();
        nRemoveMount(peekHandle(), java.util.Objects.requireNonNull(path, "path"));
    }

    /** Cross-thread hard-cancel handle. @throws IllegalStateException if closed. */
    public RtspServerCancelHandle cancelHandle() {
        ensureOpen();
        long h = nCancelHandle(peekHandle());
        if (h == 0) throw new IllegalStateException("RtspServer is closed");
        return new RtspServerCancelHandle(h);
    }

    /**
     * Graceful stop (best-effort) then free the native server. Idempotent. The stop
     * ends every publish mount's receiver (end of stream) and wakes a parked {@link
     * #nextPublisher(long)} with {@code SERVER}.
     */
    @Override public void close() { super.close(); }

    // Package-private: preserves the accessibility level expected by same-package tests
    // (RtspPanicPoisoningTest calls this before routing panics through mutating natives).
    void ensureOpen() { ensureOpen("RtspServer is closed"); }

    /**
     * Test-only: the raw native handle, for routing a panic through the real
     * {@code REGISTRY_SERVER} in {@code RtspPanicPoisoningTest} (proves the server
     * mutators are wired to {@code with_server_poisoning}). Package-private,
     * mirrors the {@code *ForTest} convention in {@code Klv}.
     */
    long nativeHandleForTest() { return peekHandle(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long nStart(String bindAddr, long maxSessions, long sessionTimeoutSecs,
        long fanoutCapacity, long gracefulShutdownDrainMs, int authScheme, String authRealm,
        String authUser, String authPassword, String tlsCert, String tlsKey,
        boolean acceptUnregisteredPublishers)
        throws RtspException;
    private static native ServerStats nStats(long handle);
    private static native String nLocalAddr(long handle);
    private static native void nStop(long handle, long drainMs) throws RtspException;
    private static native long nCancelHandle(long handle);
    private static native long nAddPublishMount(long serverHandle, String path)
        throws RtspException;
    private static native long nNextPublisher(long serverHandle, long timeoutMs)
        throws RtspException;
    private static native void nRemoveMount(long serverHandle, String path)
        throws RtspException;
    private static native void nClose(long handle);
    private static native long nAddUnicastMount(long serverHandle, String path,
        int programNumber, int pmtPid, int pcrPid, int pcrIntervalMs, int psiIntervalMs,
        int bufferPackets, int av1Carriage, int[] streamPids, int[] streamKinds,
        int[] streamCodecs, int[] streamTypeCodes, boolean[] streamCarriesPts,
        byte[] dataDescBytes, int[] dataDescLens)
        throws RtspException, MuxException;
    private static native long nAddMulticastMount(long serverHandle, String path, String group,
        int port, int ttl, String iface, int programNumber, int pmtPid, int pcrPid,
        int pcrIntervalMs, int psiIntervalMs, int bufferPackets, int av1Carriage,
        int[] streamPids, int[] streamKinds, int[] streamCodecs, int[] streamTypeCodes,
        boolean[] streamCarriesPts,
        byte[] dataDescBytes, int[] dataDescLens) throws RtspException, MuxException;
}
