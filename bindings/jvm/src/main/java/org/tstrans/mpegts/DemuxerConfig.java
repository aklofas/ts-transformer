package org.tstrans.mpegts;

/**
 * Configuration for {@link Demuxer}. Mirrors {@code tstrans.mpegts.DemuxerConfig} (9 knobs).
 *
 * <p>An immutable value object built via {@link #builder()}. Defaults match
 * {@code tst_core::mpegts::demux::DemuxerConfig::default()}.
 *
 * <p>The cap knobs ({@code pesCapPerPid}, {@code pesCapTotal}, {@code auCellCapPerPid},
 * {@code syncBufCap}) use {@code 0} as the sentinel meaning "use the Rust default" — the
 * JNI bridge maps {@code 0} to Rust {@code None} (4 MiB / 64 MiB / 1 MiB / 4 MiB
 * respectively).
 *
 * <p>{@code klv_link_overrides}/{@code stream_kind_overrides} are Rust-only and
 * deferred — not exposed here.
 */
public final class DemuxerConfig {
    private final StrictMode strictMode;
    private final long pesCapPerPid;       // 0 = use Rust default (4 MiB)
    private final long pesCapTotal;        // 0 = use Rust default (64 MiB)
    private final boolean cfiTolerance;    // default true
    private final Av1CarriageMode av1Carriage;
    private final long auCellCapPerPid;    // 0 = use Rust default (1 MiB)
    private final boolean lenientPsiReassembly;
    private final long syncBufCap;         // 0 = use Rust default (4 MiB)
    private final boolean unwrapTimestamps; // default false

    private DemuxerConfig(Builder b) {
        this.strictMode = b.strictMode;
        this.pesCapPerPid = b.pesCapPerPid;
        this.pesCapTotal = b.pesCapTotal;
        this.cfiTolerance = b.cfiTolerance;
        this.av1Carriage = b.av1Carriage;
        this.auCellCapPerPid = b.auCellCapPerPid;
        this.lenientPsiReassembly = b.lenientPsiReassembly;
        this.syncBufCap = b.syncBufCap;
        this.unwrapTimestamps = b.unwrapTimestamps;
    }

    public static Builder builder() { return new Builder(); }

    // Accessors read by the Demuxer ctor and the srt DemuxReceiver/Socket marshalling
    // to push primitives across the JNI boundary. Public for cross-package
    // org.tstrans.srt callers; the ordinal/sentinel shape is not a stable user API.
    public StrictMode strictMode() { return strictMode; }
    public long pesCapPerPid() { return pesCapPerPid; }
    public long pesCapTotal() { return pesCapTotal; }
    public boolean cfiTolerance() { return cfiTolerance; }
    public Av1CarriageMode av1Carriage() { return av1Carriage; }
    public long auCellCapPerPid() { return auCellCapPerPid; }
    public boolean lenientPsiReassembly() { return lenientPsiReassembly; }
    public long syncBufCap() { return syncBufCap; }
    public boolean unwrapTimestamps() { return unwrapTimestamps; }

    /** Fluent builder for {@link DemuxerConfig}. Defaults match {@code tst_core}'s. */
    public static final class Builder {
        private StrictMode strictMode = StrictMode.OFF;
        private long pesCapPerPid = 0;
        private long pesCapTotal = 0;
        private boolean cfiTolerance = true;
        private Av1CarriageMode av1Carriage = Av1CarriageMode.MPEG2_TS_BINDING;
        private long auCellCapPerPid = 0;
        private boolean lenientPsiReassembly = false;
        private long syncBufCap = 0;
        private boolean unwrapTimestamps = false;

        public Builder strictMode(StrictMode v) { this.strictMode = v; return this; }
        /** Per-PID PES cap in bytes; {@code 0} = use the Rust default. Rejects negatives. */
        public Builder pesCapPerPid(long v) { this.pesCapPerPid = requireNonNegativeCap(v, "pesCapPerPid"); return this; }
        /** Aggregate PES cap in bytes; {@code 0} = use the Rust default. Rejects negatives. */
        public Builder pesCapTotal(long v) { this.pesCapTotal = requireNonNegativeCap(v, "pesCapTotal"); return this; }
        public Builder cfiTolerance(boolean v) { this.cfiTolerance = v; return this; }
        public Builder av1Carriage(Av1CarriageMode v) { this.av1Carriage = v; return this; }
        /** Per-PID AU-cell cap in bytes; {@code 0} = use the Rust default. Rejects negatives. */
        public Builder auCellCapPerPid(long v) { this.auCellCapPerPid = requireNonNegativeCap(v, "auCellCapPerPid"); return this; }
        public Builder lenientPsiReassembly(boolean v) { this.lenientPsiReassembly = v; return this; }
        /**
         * Pre-sync ingress buffer ceiling in bytes; {@code 0} = use the Rust default (4 MiB).
         * A single {@code feed()} call larger than this ceiling throws {@link org.tstrans.DemuxException}
         * (kind {@code SYNC_LOSS}) before any bytes are consumed; feed in smaller chunks,
         * or raise this ceiling. Rejects negatives.
         */
        public Builder syncBufCap(long v) { this.syncBufCap = requireNonNegativeCap(v, "syncBufCap"); return this; }
        /**
         * Unwrap the raw 33-bit 90 kHz PTS/DTS onto a continuous timeline that carries
         * across the rollover ({@code 1 << 33} ticks, ~26.5 h; ITU-T H.222.0 §2.4.3.6).
         * Default {@code false} — {@link DemuxEvent} carries the raw wire value. When
         * {@code true}, a per-PID accumulator carries each sample's signed wrap-aware
         * delta from the previous raw value onto the previous unwrapped value, so
         * crossing the rollover grows the PID's offset by a full {@code 1 << 33} while
         * a genuine backward step — an out-of-order arrival, including a pre-wrap PTS
         * delivered <i>after</i> the wrap — steps back by its true distance instead of
         * being mistaken for another wrap (the emitted value is then allowed to be
         * non-monotonic, correctly reflecting the reorder). The timeline is never
         * rebased to zero, so PIDs <b>of the same program</b> stay directly comparable
         * for the whole session (this is what lets a consumer pair KLV to video by PTS
         * across the rollover); independent programs are never cross-anchored. The
         * accumulators reset alongside all other per-PID parse state on a sync reset, so
         * a reconnect or topology change restarts the unwrap timeline.
         */
        public Builder unwrapTimestamps(boolean v) { this.unwrapTimestamps = v; return this; }

        public DemuxerConfig build() { return new DemuxerConfig(this); }

        // 0 is the "use the Rust default" sentinel; >0 is an explicit cap. A
        // negative would be silently coerced back to the default by the JNI
        // bridge (which maps only >0 to Some), so reject it to keep the contract
        // unambiguous (mirrors tst-py, whose usize marshal rejects negative ints).
        private static long requireNonNegativeCap(long v, String name) {
            if (v < 0) {
                throw new IllegalArgumentException(
                    name + " must be >= 0 (0 = use the default cap), got " + v);
            }
            return v;
        }
    }
}
