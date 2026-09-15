package org.tstrans.srt;

import java.util.Optional;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;
import org.tstrans.mpegts.AudioStreamHandle;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.KlvStreamHandle;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.SubtitleStreamHandle;
import org.tstrans.mpegts.VideoStreamHandle;

/**
 * Single-call convenience wrapper that owns a {@code Muxer} plus a managed
 * (auto-reconnect) SRT transport. Construct with a caller-mode SRT URL
 * ({@code srt://host:port?mode=caller&...}) and a built {@link MuxerConfig};
 * send elementary streams via the {@code send*} family and the wrapper assembles
 * MPEG-TS packets and sends them through the SRT socket in one step. On any
 * Broken/Closed event the captured (URL, socket config) is replayed through the
 * reconnect factory under the configured {@link ReconnectPolicy}.
 *
 * <p>Mirrors {@code tstrans.srt.ManagedMuxSender}. Wraps
 * {@code tst_pipeline::MuxSender<ManagedTransport<SrtTransport>>}.
 *
 * <p><b>Thread safety:</b> the underlying Rust shell serialises sends through
 * an internal mutex, so concurrent sends are safe, but for predictable PTS
 * ordering callers typically send from one thread.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Stats:</b> {@link #stats()} returns the combined
 * {@code (SocketStats, MuxerStats)} {@link TransportStats}; there is NO
 * {@code srtStats()} on this wrapper. {@link #reconnectAttempts()} counts every
 * reconnect-factory invocation since construction (an ATTEMPT counter).
 *
 * <p><b>Byte-copy posture (JDK 17):</b> every {@code send*} method copies the
 * supplied {@code byte[]} across the JNI boundary; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 *
 * <pre>{@code
 * MuxerConfig program = MuxerConfig.builder()
 *     .addVideo(0x101, VideoCodec.H264)
 *     .build();
 * try (ManagedMuxSender s = ManagedMuxSender.fromUrl(
 *         "srt://127.0.0.1:7000?mode=caller", program)) {
 *     s.sendVideo(annexBNal, 0L, true);
 * }
 * }</pre>
 */
public final class ManagedMuxSender extends NativeHandle {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle. */
    ManagedMuxSender(long h) { setHandle(h); }

    /**
     * Build a {@code ManagedMuxSender} targeting {@code url} for the single-program
     * configuration {@code programConfig} with the default {@link ReconnectPolicy}.
     *
     * @param url           {@code srt://host:port[?key=value&...]} with {@code mode=caller}
     * @param programConfig the muxer program configuration
     * @return a connected {@code ManagedMuxSender}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or uses
     *     a non-caller mode; {@code CONNECT_FAILED} on initial-connect failure
     * @throws MuxException {@code CONFIG_INVALID} if the muxer config is rejected
     */
    public static ManagedMuxSender fromUrl(String url, MuxerConfig programConfig)
            throws SrtException, MuxException {
        return fromUrl(url, programConfig, null);
    }

    /**
     * Build a {@code ManagedMuxSender} targeting {@code url} for the single-program
     * configuration {@code programConfig}.
     *
     * <p>The URL must use {@code mode=caller} (the default when omitted). The
     * {@code policy} (or {@link ReconnectPolicy#defaults()} when {@code null})
     * governs the initial connect and every subsequent reconnect.
     *
     * @param url           {@code srt://host:port[?key=value&...]} with {@code mode=caller}
     * @param programConfig the muxer program configuration
     * @param policy        reconnect tuning; {@code null} applies the defaults
     * @return a connected {@code ManagedMuxSender}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or uses
     *     a non-caller mode; {@code CONNECT_FAILED} on initial-connect failure
     * @throws MuxException {@code CONFIG_INVALID} if the muxer config is rejected
     */
    public static ManagedMuxSender fromUrl(String url, MuxerConfig programConfig, ReconnectPolicy policy)
            throws SrtException, MuxException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrl(
            url,
            programConfig.programNumber(), programConfig.pmtPid(), programConfig.pcrPid(),
            programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(),
            programConfig.streamCodecs(), programConfig.streamTypeCodes(),
            programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens(),
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode());
        if (h == 0) {
            // nFromUrl throws a pending SrtException/MuxException; JNI re-raises.
            // Unreachable in practice, but satisfies the compiler.
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new ManagedMuxSender(h);
    }

    // ── Send family — single-stream variants ──────────────────────────────

    /**
     * Send one video access unit to the lone configured video stream.
     * Annex-B framing for H.264/H.265/H.266; raw OBU stream for AV1.
     *
     * @param nal      the access unit bytes
     * @param pts      90&nbsp;kHz presentation timestamp
     * @param keyFrame whether this access unit is a key frame
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer/framing failure
     * @throws SrtException on transport failure
     */
    public void sendVideo(byte[] nal, long pts, boolean keyFrame)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendVideo(peekHandle(), nal, pts, keyFrame);
    }

    /**
     * Send one KLV blob onto the lone configured KLV stream. Pass raw KLV LS
     * bytes — for {@code SYNCHRONOUS_METADATA} streams the muxer auto-wraps the
     * AU-cell header; do not pre-wrap.
     *
     * @param klv               raw KLV LS bytes
     * @param pts               90&nbsp;kHz presentation timestamp
     * @param metadataServiceId AU-cell metadata service id (0..=255; default 0)
     * @throws IllegalStateException if the sender is closed
     * @throws IllegalArgumentException if {@code metadataServiceId} is out of 0..=255
     * @throws MuxException on muxer failure
     * @throws SrtException on transport failure
     */
    public void sendKlv(byte[] klv, long pts, int metadataServiceId)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendKlv(peekHandle(), klv, pts, metadataServiceId);
    }

    /**
     * Send one encoded audio frame onto the lone configured audio stream.
     *
     * @param frames the encoded audio bytes
     * @param pts    90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer failure
     * @throws SrtException on transport failure
     */
    public void sendAudio(byte[] frames, long pts) throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendAudio(peekHandle(), frames, pts);
    }

    /**
     * Send one subtitle payload onto the lone configured subtitle stream.
     *
     * @param payload the subtitle access-unit bytes
     * @param pts     90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer failure
     * @throws SrtException on transport failure
     */
    public void sendSubtitle(byte[] payload, long pts) throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        // Native arg order is (handle, pts, payload); reorder here.
        nSendSubtitle(peekHandle(), pts, payload);
    }

    /**
     * Send one private-data payload onto the lone configured data stream.
     * Pass-through: the muxer applies no AU-cell wrap and no framing (UNLIKE
     * {@link #sendKlv}) — {@code data} lands verbatim as the PES payload, and
     * one send produces exactly one PES packet on stream_id {@code 0xBD}
     * ({@code private_stream_1}). {@code pts} is written into the PES header
     * only when the stream was configured with {@code carriesPts = true}, but
     * it ALWAYS drives PSI/PCR pacing.
     *
     * @param data raw payload bytes (caller's framing convention; at most
     *             65527 bytes with PTS, 65532 without)
     * @param pts  90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException {@code INPUT_MALFORMED} (payload over the PES
     *     ceiling), or {@code INVALID_USAGE} (zero data streams, or &gt;1 —
     *     ambiguous, use {@link #sendDataTo})
     * @throws SrtException on transport failure
     */
    public void sendData(byte[] data, long pts) throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendData(peekHandle(), data, pts);
    }

    // ── Send family — handle-targeted variants ────────────────────────────

    /**
     * Send one video access unit to a specific configured video stream.
     *
     * @param h        the target stream handle (from {@link #videoHandle()})
     * @param nal      the access unit bytes
     * @param pts      90&nbsp;kHz presentation timestamp
     * @param keyFrame whether this access unit is a key frame
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code CONFIG_INVALID} if the handle is invalid
     * @throws MuxException on muxer/framing failure
     */
    public void sendVideoTo(VideoStreamHandle h, byte[] nal, long pts, boolean keyFrame)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendVideoTo(peekHandle(), h.raw(), nal, pts, keyFrame);
    }

    /**
     * Send one KLV blob to a specific configured KLV stream. Pass raw KLV LS
     * bytes — the muxer auto-wraps the AU-cell header for synchronous-metadata
     * streams; do not pre-wrap.
     *
     * @param h                 the target stream handle (from {@link #klvHandle()})
     * @param klv               raw KLV LS bytes
     * @param pts               90&nbsp;kHz presentation timestamp
     * @param metadataServiceId AU-cell metadata service id (0..=255; default 0)
     * @throws IllegalStateException if the sender is closed
     * @throws IllegalArgumentException if {@code metadataServiceId} is out of 0..=255
     * @throws SrtException {@code CONFIG_INVALID} if the handle is invalid
     * @throws MuxException on muxer failure
     */
    public void sendKlvTo(KlvStreamHandle h, byte[] klv, long pts, int metadataServiceId)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendKlvTo(peekHandle(), h.raw(), klv, pts, metadataServiceId);
    }

    /**
     * Send one encoded audio frame to a specific configured audio stream.
     *
     * @param h      the target stream handle (from {@link #audioHandle()})
     * @param frames the encoded audio bytes
     * @param pts    90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code CONFIG_INVALID} if the handle is invalid
     * @throws MuxException on muxer failure
     */
    public void sendAudioTo(AudioStreamHandle h, byte[] frames, long pts)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendAudioTo(peekHandle(), h.raw(), frames, pts);
    }

    /**
     * Send one subtitle payload to a specific configured subtitle stream.
     *
     * @param h       the target stream handle (from {@link #subtitleHandle()})
     * @param payload the subtitle access-unit bytes
     * @param pts     90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code CONFIG_INVALID} if the handle is invalid
     * @throws MuxException on muxer failure
     */
    public void sendSubtitleTo(SubtitleStreamHandle h, byte[] payload, long pts)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendSubtitleTo(peekHandle(), h.raw(), pts, payload);
    }

    /**
     * Send one private-data payload to a specific configured data stream. Same
     * pass-through and PTS semantics as {@link #sendData}.
     *
     * @param h    the target stream handle (from {@link #dataHandle()})
     * @param data raw payload bytes (caller's framing convention; at most
     *             65527 bytes with PTS, 65532 without)
     * @param pts  90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code CONFIG_INVALID} if the handle is invalid
     * @throws MuxException on muxer failure
     */
    public void sendDataTo(DataStreamHandle h, byte[] data, long pts)
            throws MuxException, SrtException {
        ensureOpen("ManagedMuxSender is closed");
        nSendDataTo(peekHandle(), h.raw(), data, pts);
    }

    // ── Handle getters ────────────────────────────────────────────────────

    /**
     * First configured video stream handle, or {@link Optional#empty()}.
     *
     * @return the first video handle, if any
     */
    public Optional<VideoStreamHandle> videoHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long raw = nVideoHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(VideoStreamHandle.fromRaw(raw));
    }

    /**
     * First configured KLV stream handle, or {@link Optional#empty()}.
     *
     * @return the first KLV handle, if any
     */
    public Optional<KlvStreamHandle> klvHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long raw = nKlvHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(KlvStreamHandle.fromRaw(raw));
    }

    /**
     * First configured audio stream handle, or {@link Optional#empty()}.
     *
     * @return the first audio handle, if any
     */
    public Optional<AudioStreamHandle> audioHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long raw = nAudioHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(AudioStreamHandle.fromRaw(raw));
    }

    /**
     * First configured subtitle stream handle, or {@link Optional#empty()}.
     *
     * @return the first subtitle handle, if any
     */
    public Optional<SubtitleStreamHandle> subtitleHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long raw = nSubtitleHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(SubtitleStreamHandle.fromRaw(raw));
    }

    /**
     * First configured data stream handle, or {@link Optional#empty()}.
     *
     * @return the first data handle, if any
     */
    public Optional<DataStreamHandle> dataHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long raw = nDataHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(DataStreamHandle.fromRaw(raw));
    }

    // ── Stats + lifecycle ─────────────────────────────────────────────────

    /**
     * Combined {@code (SocketStats, MuxerStats)} snapshot. The socket stats
     * reflect the SRT transport's wire-level counters (zeros while mid-reconnect);
     * the muxer stats reflect the inner muxer's program / packets-emitted totals.
     *
     * @return the combined stats snapshot
     * @throws IllegalStateException if the sender is closed
     */
    public TransportStats stats() {
        ensureOpen("ManagedMuxSender is closed");
        return nStats(peekHandle());
    }

    /**
     * Total number of times the reconnect factory has been invoked since
     * construction. {@code 0} means the initial connect is still live; rising
     * values mean the inner SRT socket has been rebuilt (or a rebuild attempt
     * failed and was retried).
     *
     * <p>For the accurate attempts/successes split (plus gap-buffer telemetry),
     * see {@link #reconnectStats()} — this counter is preserved unchanged for
     * backward compatibility.
     *
     * @return the reconnect-attempt counter
     * @throws IllegalStateException if the sender is closed
     */
    public long reconnectAttempts() {
        ensureOpen("ManagedMuxSender is closed");
        return nReconnectAttempts(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * latches the managed wrapper's close flag (preventing further reconnects)
     * and wakes a thread parked in any {@code send*} — a live send, the
     * reconnect backoff, or a re-dial; that send then throws
     * {@code SrtException(CLOSED)}.
     *
     * <p>Safe to call from another thread at any time while the sender is open,
     * including while a send is parked and while a reconnect is in flight: the
     * cancel target is captured when the sender is opened and follows the
     * managed transport across reconnects, so this call never waits for an
     * in-flight native send.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the sender is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("ManagedMuxSender is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Reconnect/gap telemetry: attempts, successes, current gap-buffer depth, and
     * drop counters. Always readable — unlike {@link #stats()}, it does not
     * require a live inner transport (the counters live in a side channel that
     * survives reconnect cycles), but it DOES require the sender itself not be
     * closed.
     *
     * <p>{@link ManagedTransportStats#reconnecting()} is only ever {@code true}
     * under {@link ReconnectMode#BACKGROUND}.
     *
     * @return the reconnect/gap telemetry snapshot
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code IO} if the internal gap-buffer lock is poisoned
     */
    public ManagedTransportStats reconnectStats() throws SrtException {
        ensureOpen("ManagedMuxSender is closed");
        return nReconnectStats(peekHandle());
    }

    /**
     * Close the sender. Cancels the managed transport first, so a {@code send*}
     * parked on another thread — in a live send, the reconnect backoff, or a
     * re-dial — ends with {@code SrtException(CLOSED)} and this call returns
     * promptly instead of waiting out the reconnect budget; then best-effort
     * drains any pending bytes and drops the transport. Idempotent —
     * subsequent calls are no-ops.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the sender owns a live transport.
     *
     * @return liveness state of the underlying managed transport
     */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    @Override protected void nativeClose(long h) { nClose(h); }

    // --- Natives ---

    private static native long nFromUrl(String url, int programNumber, int pmtPid, int pcrPid,
        int pcrIntervalMs, int psiIntervalMs, int bufferPackets, int av1Carriage,
        int[] streamPids, int[] streamKinds, int[] streamCodecs, int[] streamTypeCodes,
        boolean[] streamCarriesPts,
        byte[] dataDescBytes, int[] dataDescLens,
        boolean maxAttemptsPresent, int maxAttempts,
        int backoffKind, long backoffBaseMs, long backoffMaxMs,
        int gapBufferCapacity, int overflowPolicy, int mode) throws SrtException, MuxException;

    private static native void nSendVideo(long handle, byte[] nal, long pts, boolean keyFrame)
        throws MuxException, SrtException;
    private static native void nSendKlv(long handle, byte[] klv, long pts, int metadataServiceId)
        throws MuxException, SrtException;
    private static native void nSendAudio(long handle, byte[] frames, long pts)
        throws MuxException, SrtException;
    private static native void nSendSubtitle(long handle, long pts, byte[] payload)
        throws MuxException, SrtException;
    private static native void nSendData(long handle, byte[] data, long pts)
        throws MuxException, SrtException;

    private static native void nSendVideoTo(long handle, long streamHandleRaw, byte[] nal,
        long pts, boolean keyFrame) throws MuxException, SrtException;
    private static native void nSendKlvTo(long handle, long streamHandleRaw, byte[] klv,
        long pts, int metadataServiceId) throws MuxException, SrtException;
    private static native void nSendAudioTo(long handle, long streamHandleRaw, byte[] frames,
        long pts) throws MuxException, SrtException;
    private static native void nSendSubtitleTo(long handle, long streamHandleRaw, long pts,
        byte[] payload) throws MuxException, SrtException;
    private static native void nSendDataTo(long handle, long streamHandleRaw, byte[] data,
        long pts) throws MuxException, SrtException;

    private static native long nVideoHandle(long handle);
    private static native long nKlvHandle(long handle);
    private static native long nAudioHandle(long handle);
    private static native long nSubtitleHandle(long handle);
    private static native long nDataHandle(long handle);

    private static native TransportStats nStats(long handle);
    private static native long nReconnectAttempts(long handle);
    private static native ManagedTransportStats nReconnectStats(long handle) throws SrtException;
    private static native long nCancelHandle(long handle);
    private static native void nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
