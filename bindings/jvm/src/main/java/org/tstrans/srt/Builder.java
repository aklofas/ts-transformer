package org.tstrans.srt;

import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * Fluent SRT URL constructor. Accumulates knob overrides, then finalizes via
 * {@link #connect()} (caller mode) or {@link #listen()} (listener mode).
 *
 * <p>All chainable setters return {@code this} so calls can be chained:
 * <pre>{@code
 * Socket s = new Builder("srt://127.0.0.1:9000")
 *     .caller()
 *     .latencyMs(200)
 *     .passphrase("hunter2hunter2")
 *     .connect();
 * }</pre>
 *
 * <p><strong>URL wins on conflict.</strong> Query parameters embedded in the URL
 * (e.g. {@code ?latency=120}) are applied AFTER the chainable setters, so the
 * URL has final say on any knob that appears in both places. This mirrors
 * tst-py's {@code Builder} semantics (Q4-A precedence rule: URL overlay
 * unconditionally overwrites).
 *
 * <p><strong>Rendezvous mode.</strong> {@link #rendezvous()} is provided for
 * forward compatibility but {@link #connect()} raises
 * {@code SrtException(CONFIG_INVALID)} if rendezvous mode is set — tst-srt
 * does not yet support rendezvous. Update this note when it lands.
 */
public final class Builder {

    static { NativeLoader.load(); }

    /**
     * Builder mode. Tracks the caller's explicit mode choice.
     * {@code URL_CHOICE} (default) defers to the URL's {@code ?mode=} parameter.
     * Ordinal mapping: URL_CHOICE=0, CALLER=1, LISTENER=2, RENDEZVOUS=3 —
     * passed as {@code mode.ordinal()} to the native layer.
     */
    enum Mode { URL_CHOICE, CALLER, LISTENER, RENDEZVOUS }

    private final String url;
    private Mode mode = Mode.URL_CHOICE;

    // Nullable boxed knobs — null means "use the URL/default".
    private Integer latencyMs;
    private String passphrase;
    private String streamId;
    private String congestion;
    private Integer connectTimeoutMs;
    private Integer recvTimeoutMs;
    private Integer sendTimeoutMs;
    private Integer peerLatencyMs;
    private Integer recvLatencyMs;
    private Long maxBandwidthBps;
    private Integer mss;
    private Integer payloadSize;

    /** Construct a Builder for the given SRT URL. No native call until {@link #connect()} or {@link #listen()}. */
    public Builder(String url) {
        this.url = url;
    }

    // --- Mode setters ---

    /** Use caller mode. Chainable. */
    public Builder caller() { this.mode = Mode.CALLER; return this; }

    /** Use listener mode. Chainable. */
    public Builder listener() { this.mode = Mode.LISTENER; return this; }

    /**
     * Use rendezvous mode. Chainable. <strong>Not yet supported</strong> —
     * {@link #connect()} will throw {@code SrtException(CONFIG_INVALID)}.
     * The setter is provided for forward compatibility.
     */
    public Builder rendezvous() { this.mode = Mode.RENDEZVOUS; return this; }

    // --- Knob setters ---

    /**
     * Set {@code SRTO_LATENCY} in milliseconds. Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder latencyMs(int ms) { this.latencyMs = requireNonNegative(ms, "latencyMs"); return this; }

    /** Set the passphrase (AES encryption). Chainable. */
    public Builder passphrase(String p) { this.passphrase = p; return this; }

    /** Set the stream ID negotiated at handshake. Chainable. */
    public Builder streamId(String id) { this.streamId = id; return this; }

    /** Set the congestion controller (e.g. {@code "live"} or {@code "file"}). Chainable. */
    public Builder congestion(String c) { this.congestion = c; return this; }

    /**
     * Set {@code SRTO_CONNTIMEO} in milliseconds. Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder connectTimeoutMs(int ms) { this.connectTimeoutMs = requireNonNegative(ms, "connectTimeoutMs"); return this; }

    /**
     * Set {@code SRTO_RCVTIMEO} in milliseconds. Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder recvTimeoutMs(int ms) { this.recvTimeoutMs = requireNonNegative(ms, "recvTimeoutMs"); return this; }

    /**
     * Set {@code SRTO_SNDTIMEO} in milliseconds. Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder sendTimeoutMs(int ms) { this.sendTimeoutMs = requireNonNegative(ms, "sendTimeoutMs"); return this; }

    /**
     * Set {@code SRTO_PEERLATENCY} in milliseconds (caller side only). Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder peerLatencyMs(int ms) { this.peerLatencyMs = requireNonNegative(ms, "peerLatencyMs"); return this; }

    /**
     * Set {@code SRTO_RCVLATENCY} in milliseconds. Chainable.
     * @throws IllegalArgumentException if {@code ms} is negative.
     */
    public Builder recvLatencyMs(int ms) { this.recvLatencyMs = requireNonNegative(ms, "recvLatencyMs"); return this; }

    /**
     * Set {@code SRTO_MAXBW} as an absolute cap in <b>bytes</b> per second.
     * {@code 0} means relative to the input rate (libsrt's {@code SRTO_INPUTBW}
     * rule), not unlimited. Chainable.
     * @throws IllegalArgumentException if {@code bps} is negative.
     */
    public Builder maxBandwidthBps(long bps) { this.maxBandwidthBps = requireNonNegative(bps, "maxBandwidthBps"); return this; }

    /**
     * Set {@code SRTO_MSS} (Maximum Segment Size, bytes). Must be in 0..=65535.
     * Values outside this range throw {@code IllegalArgumentException} at
     * {@link #connect()} / {@link #listen()} time (before any network call).
     * Chainable.
     */
    public Builder mss(int value) { this.mss = value; return this; }

    /**
     * Set {@code SRTO_PAYLOADSIZE} (bytes). Must be in 0..=65535.
     * Values outside this range throw {@code IllegalArgumentException} at
     * {@link #connect()} / {@link #listen()} time (before any network call).
     * Chainable.
     */
    public Builder payloadSize(int value) { this.payloadSize = value; return this; }

    // --- Finalizers ---

    /**
     * Resolve this builder to a connected {@link Socket}.
     *
     * <p>Mode must be {@code caller} (either via {@link #caller()} or the URL
     * default — the URL must not contain {@code ?mode=listener}). Blocks until
     * the SRT handshake completes.
     *
     * @throws SrtException if rendezvous mode is set ({@code CONFIG_INVALID}),
     *                      if the URL specifies a non-caller mode ({@code CONFIG_INVALID}),
     *                      if the URL fails to parse ({@code CONFIG_INVALID}),
     *                      if any knob value is invalid ({@code CONFIG_INVALID}),
     *                      if the handshake times out ({@code TIMEOUT}),
     *                      or if the connection is refused / rejected ({@code CONNECT_FAILED}).
     * @throws IllegalArgumentException if {@code mss} or {@code payloadSize} is out of u16 range.
     */
    public Socket connect() throws SrtException {
        return new Socket(nConnect(
            url, mode.ordinal(),
            latencyMs, passphrase, streamId, congestion,
            connectTimeoutMs, recvTimeoutMs, sendTimeoutMs,
            peerLatencyMs, recvLatencyMs,
            maxBandwidthBps, mss, payloadSize
        ));
    }

    /**
     * Resolve this builder to a bound {@link Listener}.
     *
     * <p>The SRT URL <strong>must</strong> include {@code ?mode=listener}
     * (e.g. {@code srt://:9000?mode=listener}). {@link #listener()} guards against
     * calling the wrong finalizer but does NOT inject the URL parameter — the URL
     * itself must carry it. Canonical form:
     * {@code new Builder("srt://:9000?mode=listener").listener().listen()}.
     * Binds and starts listening immediately.
     *
     * @throws SrtException if rendezvous mode is set ({@code CONFIG_INVALID}),
     *                      if the URL specifies a non-listener mode ({@code CONFIG_INVALID}),
     *                      if the URL fails to parse ({@code CONFIG_INVALID}),
     *                      if any knob value is invalid ({@code CONFIG_INVALID}),
     *                      or if the bind fails (address in use, permission denied — {@code CONNECT_FAILED}).
     * @throws IllegalArgumentException if {@code mss} or {@code payloadSize} is out of u16 range.
     */
    public Listener listen() throws SrtException {
        return new Listener(nListen(
            url, mode.ordinal(),
            latencyMs, passphrase, streamId, congestion,
            connectTimeoutMs, recvTimeoutMs, sendTimeoutMs,
            peerLatencyMs, recvLatencyMs,
            maxBandwidthBps, mss, payloadSize
        ));
    }

    // --- Helpers ---

    // Java int/long are signed, but these knobs map to unsigned Rust quantities
    // (Duration millis / bps). A negative would cast to a near-infinite value on
    // the Rust side; reject it at the boundary, mirroring tst-py's u32/u64 typing
    // which makes PyO3 reject negatives.
    private static int requireNonNegative(int v, String name) {
        if (v < 0) throw new IllegalArgumentException(name + " must be non-negative, got " + v);
        return v;
    }

    private static long requireNonNegative(long v, String name) {
        if (v < 0) throw new IllegalArgumentException(name + " must be non-negative, got " + v);
        return v;
    }

    // --- Native ---

    private static native long nConnect(
        String url, int mode,
        Integer latencyMs, String passphrase, String streamId, String congestion,
        Integer connectTimeoutMs, Integer recvTimeoutMs, Integer sendTimeoutMs,
        Integer peerLatencyMs, Integer recvLatencyMs,
        Long maxBandwidthBps, Integer mss, Integer payloadSize
    ) throws SrtException;

    private static native long nListen(
        String url, int mode,
        Integer latencyMs, String passphrase, String streamId, String congestion,
        Integer connectTimeoutMs, Integer recvTimeoutMs, Integer sendTimeoutMs,
        Integer peerLatencyMs, Integer recvLatencyMs,
        Long maxBandwidthBps, Integer mss, Integer payloadSize
    ) throws SrtException;
}
