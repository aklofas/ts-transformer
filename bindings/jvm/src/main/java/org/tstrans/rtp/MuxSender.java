package org.tstrans.rtp;

import java.util.Optional;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;
import org.tstrans.mpegts.AudioStreamHandle;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.KlvStreamHandle;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.SubtitleStreamHandle;
import org.tstrans.mpegts.VideoStreamHandle;

/**
 * Single-call convenience wrapper that owns a {@code Muxer} + an RTP
 * {@code RtpTransport}. Construct with an {@code rtp://host:port} URL and a built
 * {@link MuxerConfig}; send elementary streams via the {@code send*} family and
 * the wrapper assembles MPEG-TS packets and sends them over RTP/UDP in one step.
 *
 * <p>Mirrors {@code tstrans.rtp.MuxSender}. Wraps
 * {@code tst_pipeline::MuxSender<tst_rtp::RtpTransport>}.
 *
 * <p><b>Thread safety:</b> the underlying Rust shell serialises sends through an
 * internal mutex, so concurrent sends are safe, but for predictable PTS ordering
 * callers typically send from one thread.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> every {@code send*} method copies the
 * supplied {@code byte[]} across the JNI boundary; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 *
 * <pre>{@code
 * MuxerConfig program = MuxerConfig.builder()
 *     .programNumber(1).pmtPid(0x1000)
 *     .addVideo(0x1011, VideoCodec.H264)
 *     .build();
 * try (MuxSender s = MuxSender.fromUrl("rtp://127.0.0.1:5004", program)) {
 *     s.sendVideo(annexBNal, 0L, true);
 * }
 * }</pre>
 */
public final class MuxSender extends NativeHandle {
    static { NativeLoader.load(); }

    /** Default UDP datagram payload size (7 × 188 TS packets). Matches tst-py. */
    public static final int DEFAULT_PKT_SIZE = 1316;

    MuxSender(long h) { setHandle(h); }

    /**
     * Build a {@code MuxSender} targeting {@code url} with the default packet size
     * ({@value #DEFAULT_PKT_SIZE}).
     *
     * @param url           {@code rtp://host:port}
     * @param programConfig the muxer program configuration
     * @return an open {@code MuxSender}
     * @throws RtpException {@code IO} on URL-parse / socket-bind failure
     * @throws MuxException {@code CONFIG_INVALID} if the muxer rejects the config
     */
    public static MuxSender fromUrl(String url, MuxerConfig programConfig)
            throws RtpException, MuxException {
        return fromUrl(url, programConfig, DEFAULT_PKT_SIZE);
    }

    /**
     * Build a {@code MuxSender} targeting {@code url} with an explicit
     * {@code pktSize}.
     *
     * @param url           {@code rtp://host:port}
     * @param programConfig the muxer program configuration
     * @param pktSize       the UDP datagram payload size; must be &ge; 0
     * @return an open {@code MuxSender}
     * @throws RtpException {@code IO} on URL-parse / socket-bind failure
     * @throws MuxException {@code CONFIG_INVALID} if the muxer rejects the config
     * @throws IllegalArgumentException if {@code pktSize} is negative
     */
    public static MuxSender fromUrl(String url, MuxerConfig programConfig, int pktSize)
            throws RtpException, MuxException {
        if (pktSize < 0) throw new IllegalArgumentException("pktSize must be >= 0: " + pktSize);
        long h = nFromUrl(
            url,
            programConfig.programNumber(), programConfig.pmtPid(), programConfig.pcrPid(),
            programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(),
            programConfig.streamCodecs(), programConfig.streamTypeCodes(),
            programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens(),
            pktSize);
        if (h == 0) {
            // nFromUrl throws a pending RtpException/MuxException; JNI re-raises.
            // Unreachable in practice, but satisfies the compiler.
            throw new RtpException(RtpException.Kind.IO,
                "nFromUrl returned 0 without throwing");
        }
        return new MuxSender(h);
    }

    // ── Send family — single-stream variants ──────────────────────────────

    /**
     * Send one video access unit onto the lone configured video stream.
     * Annex-B framing for H.264/H.265/H.266; raw OBU stream for AV1.
     *
     * @param nal      the access unit bytes
     * @param pts      90&nbsp;kHz presentation timestamp
     * @param keyFrame whether this access unit is a key frame
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer/framing failure
     * @throws RtpException on transport failure
     */
    public void sendVideo(byte[] nal, long pts, boolean keyFrame)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendVideo(peekHandle(), nal, pts, keyFrame);
    }

    /**
     * Send one KLV blob onto the lone configured KLV stream. Pass raw KLV LS
     * bytes — the muxer auto-wraps the AU-cell header for synchronous-metadata
     * streams; do not pre-wrap.
     *
     * @param klv               raw KLV LS bytes
     * @param pts               90&nbsp;kHz presentation timestamp
     * @param metadataServiceId AU-cell metadata service id (0..=255; default 0)
     * @throws IllegalStateException if the sender is closed
     * @throws IllegalArgumentException if {@code metadataServiceId} is out of 0..=255
     * @throws MuxException on muxer failure
     * @throws RtpException on transport failure
     */
    public void sendKlv(byte[] klv, long pts, int metadataServiceId)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendKlv(peekHandle(), klv, pts, metadataServiceId);
    }

    /**
     * Send one encoded audio frame onto the lone configured audio stream.
     * {@code frames} is one or more pre-framed audio frames concatenated by the
     * caller (ADTS for AAC, MPEG-2 audio frames for MP2).
     *
     * @param frames the encoded audio bytes
     * @param pts    90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer failure
     * @throws RtpException on transport failure
     */
    public void sendAudio(byte[] frames, long pts) throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendAudio(peekHandle(), frames, pts);
    }

    /**
     * Send one subtitle payload onto the lone configured subtitle stream.
     *
     * @param payload the subtitle access-unit bytes
     * @param pts     90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws MuxException on muxer failure
     * @throws RtpException on transport failure
     */
    public void sendSubtitle(byte[] payload, long pts) throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
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
     * @throws RtpException on transport failure
     */
    public void sendData(byte[] data, long pts) throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
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
     * @throws RtpException {@code IO} on transport failure, or if the
     *     stream handle is out of range / forged
     * @throws MuxException on muxer/framing failure (incl. a handle not configured
     *     for this muxer)
     */
    public void sendVideoTo(VideoStreamHandle h, byte[] nal, long pts, boolean keyFrame)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendVideoTo(peekHandle(), h.raw(), nal, pts, keyFrame);
    }

    /**
     * Send one KLV blob to a specific configured KLV stream. Pass raw KLV LS
     * bytes — the muxer auto-wraps the AU-cell header; do not pre-wrap.
     *
     * @param h                 the target stream handle (from {@link #klvHandle()})
     * @param klv               raw KLV LS bytes
     * @param pts               90&nbsp;kHz presentation timestamp
     * @param metadataServiceId AU-cell metadata service id (0..=255; default 0)
     * @throws IllegalStateException if the sender is closed
     * @throws IllegalArgumentException if {@code metadataServiceId} is out of 0..=255
     * @throws RtpException {@code IO} on transport failure, or if the
     *     stream handle is out of range / forged
     * @throws MuxException on muxer failure (incl. a handle not configured for
     *     this muxer)
     */
    public void sendKlvTo(KlvStreamHandle h, byte[] klv, long pts, int metadataServiceId)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendKlvTo(peekHandle(), h.raw(), klv, pts, metadataServiceId);
    }

    /**
     * Send one encoded audio frame to a specific configured audio stream.
     *
     * @param h      the target stream handle (from {@link #audioHandle()})
     * @param frames the encoded audio bytes
     * @param pts    90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws RtpException {@code IO} on transport failure, or if the
     *     stream handle is out of range / forged
     * @throws MuxException on muxer failure (incl. a handle not configured for
     *     this muxer)
     */
    public void sendAudioTo(AudioStreamHandle h, byte[] frames, long pts)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendAudioTo(peekHandle(), h.raw(), frames, pts);
    }

    /**
     * Send one subtitle payload to a specific configured subtitle stream.
     *
     * @param h       the target stream handle (from {@link #subtitleHandle()})
     * @param payload the subtitle access-unit bytes
     * @param pts     90&nbsp;kHz presentation timestamp
     * @throws IllegalStateException if the sender is closed
     * @throws RtpException {@code IO} on transport failure, or if the
     *     stream handle is out of range / forged
     * @throws MuxException on muxer failure (incl. a handle not configured for
     *     this muxer)
     */
    public void sendSubtitleTo(SubtitleStreamHandle h, byte[] payload, long pts)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
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
     * @throws RtpException {@code IO} on transport failure, or if the
     *     stream handle is out of range / forged
     * @throws MuxException on muxer failure (incl. a handle not configured for
     *     this muxer)
     */
    public void sendDataTo(DataStreamHandle h, byte[] data, long pts)
            throws MuxException, RtpException {
        ensureOpen("MuxSender is closed");
        nSendDataTo(peekHandle(), h.raw(), data, pts);
    }

    // ── Handle getters ────────────────────────────────────────────────────

    /** First configured video stream handle, or {@link Optional#empty()}.
     *  @throws IllegalStateException if the sender is closed */
    public Optional<VideoStreamHandle> videoHandle() {
        ensureOpen("MuxSender is closed");
        long raw = nVideoHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(VideoStreamHandle.fromRaw(raw));
    }

    /** First configured KLV stream handle, or {@link Optional#empty()}.
     *  @throws IllegalStateException if the sender is closed */
    public Optional<KlvStreamHandle> klvHandle() {
        ensureOpen("MuxSender is closed");
        long raw = nKlvHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(KlvStreamHandle.fromRaw(raw));
    }

    /** First configured audio stream handle, or {@link Optional#empty()}.
     *  @throws IllegalStateException if the sender is closed */
    public Optional<AudioStreamHandle> audioHandle() {
        ensureOpen("MuxSender is closed");
        long raw = nAudioHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(AudioStreamHandle.fromRaw(raw));
    }

    /** First configured subtitle stream handle, or {@link Optional#empty()}.
     *  @throws IllegalStateException if the sender is closed */
    public Optional<SubtitleStreamHandle> subtitleHandle() {
        ensureOpen("MuxSender is closed");
        long raw = nSubtitleHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(SubtitleStreamHandle.fromRaw(raw));
    }

    /** First configured data stream handle, or {@link Optional#empty()}.
     *  @throws IllegalStateException if the sender is closed */
    public Optional<DataStreamHandle> dataHandle() {
        ensureOpen("MuxSender is closed");
        long raw = nDataHandle(peekHandle());
        return raw < 0 ? Optional.empty() : Optional.of(DataStreamHandle.fromRaw(raw));
    }

    // ── Stats + lifecycle ─────────────────────────────────────────────────

    /**
     * Combined {@code (SocketStats, MuxerStats)} snapshot. The socket stats reflect
     * the RTP transport's wire-level counters; the muxer stats reflect the inner
     * muxer's program / packets-emitted totals.
     *
     * @return the combined stats snapshot
     * @throws IllegalStateException if the sender is closed
     */
    public TransportStats stats() {
        ensureOpen("MuxSender is closed");
        return nStats(peekHandle());
    }

    /** Close the sender. Best-effort drains pending bytes, then drops the RTP
     * transport. Idempotent. */
    @Override public void close() { super.close(); }

    /**
     * Drain every byte the muxer still holds to the transport, report the first
     * drain error, then close the transport — the lossless counterpart to
     * {@link #close()}. The sender is closed either way; a second call is a
     * no-op, and so is a call after {@link #close()}.
     *
     * <p>Unlike {@link #close()} this does NOT cancel first: a {@code send*}
     * parked on another thread holds the sender and {@code finish()} waits
     * behind it. The handle still needs a {@link #close()} afterwards.
     *
     * @throws RtpException with the first drain error's kind
     */
    public void finish() throws RtpException {
        long h = peekHandle();
        if (h == 0) return;            // already closed: quiet, like a second finish
        nFinish(h);
    }

    /** Whether the sender owns a live transport. */
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
        byte[] dataDescBytes, int[] dataDescLens, int pktSize) throws RtpException, MuxException;

    private static native void nSendVideo(long handle, byte[] nal, long pts, boolean keyFrame)
        throws MuxException, RtpException;
    private static native void nSendKlv(long handle, byte[] klv, long pts, int metadataServiceId)
        throws MuxException, RtpException;
    private static native void nSendAudio(long handle, byte[] frames, long pts)
        throws MuxException, RtpException;
    private static native void nSendSubtitle(long handle, long pts, byte[] payload)
        throws MuxException, RtpException;
    private static native void nSendData(long handle, byte[] data, long pts)
        throws MuxException, RtpException;

    private static native void nSendVideoTo(long handle, long streamHandleRaw, byte[] nal,
        long pts, boolean keyFrame) throws MuxException, RtpException;
    private static native void nSendKlvTo(long handle, long streamHandleRaw, byte[] klv,
        long pts, int metadataServiceId) throws MuxException, RtpException;
    private static native void nSendAudioTo(long handle, long streamHandleRaw, byte[] frames,
        long pts) throws MuxException, RtpException;
    private static native void nSendSubtitleTo(long handle, long streamHandleRaw, long pts,
        byte[] payload) throws MuxException, RtpException;
    private static native void nSendDataTo(long handle, long streamHandleRaw, byte[] data,
        long pts) throws MuxException, RtpException;

    private static native long nVideoHandle(long handle);
    private static native long nKlvHandle(long handle);
    private static native long nAudioHandle(long handle);
    private static native long nSubtitleHandle(long handle);
    private static native long nDataHandle(long handle);

    private static native TransportStats nStats(long handle);
    private static native void nClose(long handle);
    private static native void nFinish(long handle) throws RtpException;
    private static native boolean nIsAlive(long handle);
}
