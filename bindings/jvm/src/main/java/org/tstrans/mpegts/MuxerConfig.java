package org.tstrans.mpegts;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/**
 * Single-program muxer configuration. Mirrors the user-facing shape of
 * {@code tstrans.mpegts.MuxerConfigBuilder} + {@code MuxerProgramConfigBuilder},
 * collapsed to one program. Built via {@link #builder()}; the resulting
 * immutable value exposes parallel-array accessors (public for cross-package
 * marshalling; each returns a copy) that the {@link Muxer} constructor marshals
 * across the JNI boundary in a single {@code nOpen} call.
 *
 * <p>The builder performs only cheap Java-side sanity checks (≥1 stream, PID
 * range). Deep validation (PID collisions, PMT-size budget, PCR-on-KLV, sync-KLV
 * needs carriesPts, etc.) runs Rust-side in {@code Muxer::new} and surfaces as
 * {@link org.tstrans.MuxException} ({@code CONFIG_INVALID}).
 *
 * <p><b>Deferred (documented):</b> multi-program configs; per-stream/program
 * descriptors for the typed kinds (data streams get theirs via
 * {@link Builder#streamDescriptorsForData}); the typed kinds'
 * {@code *_to(handle, ...)} multi-stream variants (data streams have them:
 * {@link Muxer#pushDataTo} + {@link Muxer#dataHandles()}); stats.
 * DVB subtitle codecs (which need language + page-ID config) are also
 * deferred — {@link Builder#addSubtitle} accepts only the no-config codecs
 * (CEA-708 / WebVTT).
 */
public final class MuxerConfig {
    // Internal stream-kind codes — the parallel-array contract the Muxer JNI
    // nOpen relies on (same 0..4 mapping is hardcoded Rust-side).
    static final int KIND_VIDEO = 0;
    static final int KIND_KLV = 1;
    static final int KIND_AUDIO = 2;
    static final int KIND_SUBTITLE = 3;
    static final int KIND_DATA = 4;

    private final int programNumber;
    private final int pmtPid;
    private final int pcrPid;            // -1 = auto-resolve (Rust None)
    private final int pcrIntervalMs;
    private final int psiIntervalMs;
    private final int bufferPackets;
    private final Av1CarriageMode av1Carriage;
    private final int[] streamPids;
    private final int[] streamKinds;     // KIND_* per stream
    private final int[] streamCodecs;    // codec ordinal per kind; -1 for KLV/DATA
    // KlvStreamType ordinal for KLV; raw PMT stream_type byte for DATA; -1 otherwise.
    private final int[] streamTypeCodes;
    private final boolean[] streamCarriesPts; // KLV + DATA; false otherwise
    // Per-stream PMT descriptor loops for DATA streams, flattened: dataDescBytes
    // is every descriptor TLV concatenated in stream order; dataDescLens[i] is
    // that stream's byte count within the blob (0 for non-data/descriptor-less).
    private final byte[] dataDescBytes;
    private final int[] dataDescLens;

    private MuxerConfig(Builder b) {
        this.programNumber = b.programNumber;
        this.pmtPid = b.pmtPid;
        this.pcrPid = b.pcrPid;
        this.pcrIntervalMs = b.pcrIntervalMs;
        this.psiIntervalMs = b.psiIntervalMs;
        this.bufferPackets = b.bufferPackets;
        this.av1Carriage = b.av1Carriage;
        int n = b.pids.size();
        this.streamPids = new int[n];
        this.streamKinds = new int[n];
        this.streamCodecs = new int[n];
        this.streamTypeCodes = new int[n];
        this.streamCarriesPts = new boolean[n];
        for (int i = 0; i < n; i++) {
            this.streamPids[i] = b.pids.get(i);
            this.streamKinds[i] = b.kinds.get(i);
            this.streamCodecs[i] = b.codecs.get(i);
            this.streamTypeCodes[i] = b.typeCodes.get(i);
            this.streamCarriesPts[i] = b.carriesPts.get(i);
        }
        this.dataDescLens = new int[n];
        java.io.ByteArrayOutputStream blob = new java.io.ByteArrayOutputStream();
        int dataIdx = 0;
        for (int i = 0; i < n; i++) {
            if (b.kinds.get(i) == KIND_DATA) {
                byte[][] descs = b.dataDescs.get(dataIdx);
                if (descs != null) {
                    int total = 0;
                    for (byte[] d : descs) { blob.write(d, 0, d.length); total += d.length; }
                    this.dataDescLens[i] = total;
                }
                dataIdx++;
            }
        }
        this.dataDescBytes = blob.toByteArray();
    }

    /** Start a new builder. Single program for now (multi-program deferred). */
    public static Builder builder() {
        return new Builder();
    }

    public int programNumber() {
        return programNumber;
    }

    public int streamCount() {
        return streamPids.length;
    }

    // Accessors read by the Muxer ctor and the srt MuxSender/Socket marshalling to
    // build the nOpen parallel-array contract. Public for cross-package
    // org.tstrans.srt callers; the parallel-array shape is not a stable user API.
    // The array accessors return copies so the immutable value cannot be mutated through them.
    public int pmtPid() { return pmtPid; }
    public int pcrPid() { return pcrPid; }
    public int pcrIntervalMs() { return pcrIntervalMs; }
    public int psiIntervalMs() { return psiIntervalMs; }
    public int bufferPackets() { return bufferPackets; }
    public Av1CarriageMode av1Carriage() { return av1Carriage; }
    public int[] streamPids() { return streamPids.clone(); }
    public int[] streamKinds() { return streamKinds.clone(); }
    public int[] streamCodecs() { return streamCodecs.clone(); }
    /** KlvStreamType ordinal for KLV streams; raw PMT stream_type byte for DATA streams; -1 otherwise. */
    public int[] streamTypeCodes() { return streamTypeCodes.clone(); }
    /** {@code true} if this stream carries PTS (KLV + DATA streams); {@code false} otherwise. */
    public boolean[] streamCarriesPts() { return streamCarriesPts.clone(); }
    public byte[] dataDescBytes() { return dataDescBytes.clone(); }
    public int[] dataDescLens() { return dataDescLens.clone(); }

    /**
     * Fluent builder for {@link MuxerConfig}. Defaults mirror
     * {@code tst_core::mpegts::mux::MuxerConfig::default()} scalars
     * (pcrIntervalMs=40, psiIntervalMs=100, bufferPackets=10000,
     * av1Carriage=MPEG2_TS_BINDING). {@code programNumber}/{@code pmtPid} are
     * single-valued (one program).
     */
    public static final class Builder {
        private int programNumber = 1;
        private int pmtPid = 0x1000;
        private int pcrPid = -1; // auto-resolve
        private int pcrIntervalMs = 40;
        private int psiIntervalMs = 100;
        private int bufferPackets = 10_000;
        private Av1CarriageMode av1Carriage = Av1CarriageMode.MPEG2_TS_BINDING;
        private final List<Integer> pids = new ArrayList<>();
        private final List<Integer> kinds = new ArrayList<>();
        private final List<Integer> codecs = new ArrayList<>();
        private final List<Integer> typeCodes = new ArrayList<>();
        private final List<Boolean> carriesPts = new ArrayList<>();
        private final java.util.Map<Integer, byte[][]> dataDescs = new java.util.LinkedHashMap<>();

        public Builder programNumber(int v) { this.programNumber = v; return this; }
        public Builder pmtPid(int v) { this.pmtPid = v; return this; }
        /** Pin the PCR PID to a configured stream's PID. Default: auto-resolve (first video, else KLV/audio). */
        public Builder pcrPid(int v) { this.pcrPid = v; return this; }
        public Builder pcrIntervalMs(int v) { this.pcrIntervalMs = v; return this; }
        public Builder psiIntervalMs(int v) { this.psiIntervalMs = v; return this; }
        public Builder bufferPackets(int v) { this.bufferPackets = v; return this; }
        public Builder av1Carriage(Av1CarriageMode v) {
            this.av1Carriage = Objects.requireNonNull(v, "av1Carriage");
            return this;
        }

        /** Add an H.264/H.265/H.266/AV1 video elementary stream. */
        public Builder addVideo(int pid, VideoCodec codec) {
            Objects.requireNonNull(codec, "codec");
            addStream(pid, KIND_VIDEO, codec.ordinal(), -1, false);
            return this;
        }

        /**
         * Add a KLV metadata stream. Pass raw KLV LS bytes at push time — for
         * {@code SYNCHRONOUS_METADATA} the muxer auto-wraps the AU cell header.
         */
        public Builder addKlv(int pid, KlvStreamType type, boolean carriesPts) {
            Objects.requireNonNull(type, "type");
            addStream(pid, KIND_KLV, -1, type.ordinal(), carriesPts);
            return this;
        }

        /** Add an audio elementary stream. */
        public Builder addAudio(int pid, AudioCodec codec) {
            Objects.requireNonNull(codec, "codec");
            addStream(pid, KIND_AUDIO, codec.ordinal(), -1, false);
            return this;
        }

        /**
         * Add a subtitle elementary stream. Only the no-config codecs
         * {@link SubtitleCodec#CEA708_STANDALONE} and
         * {@link SubtitleCodec#WEBVTT_IN_TS} are supported; the DVB codecs need
         * language + page-ID configuration not yet exposed by the JVM binding
         * (deferred), so passing one throws {@link IllegalArgumentException}.
         */
        public Builder addSubtitle(int pid, SubtitleCodec codec) {
            Objects.requireNonNull(codec, "codec");
            if (codec == SubtitleCodec.DVB_SUBTITLING || codec == SubtitleCodec.DVB_TELETEXT) {
                throw new IllegalArgumentException(
                    "DVB subtitle codecs need language/page-ID config not yet exposed in the "
                        + "JVM binding (deferred); use CEA708_STANDALONE or WEBVTT_IN_TS");
            }
            addStream(pid, KIND_SUBTITLE, codec.ordinal(), -1, false);
            return this;
        }

        /**
         * Add a private/data elementary stream (PES on stream_id 0xBD,
         * private_stream_1). {@code streamType} is the raw PMT stream_type byte
         * (0..=255). It must not classify as a typed kind (e.g. 0x1B H.264), and a
         * 0x06 stream must not carry a classifying descriptor (e.g. KLVA
         * registration) — both are rejected at {@code new Muxer(...)} with
         * {@code CONFIG_INVALID}. Push-time payloads pass through verbatim — no
         * AU-cell wrap, no framing (unlike {@link #addKlv KLV}).
         *
         * @param carriesPts whether pushed PTS values are written to PES headers
         *     (false: payloads re-demux with {@code pts == 0}; the push-time pts
         *     still drives PSI/PCR pacing)
         */
        public Builder addData(int pid, int streamType, boolean carriesPts) {
            if (streamType < 0 || streamType > 255) {
                throw new IllegalArgumentException(
                    "streamType must be in 0..=255, got " + streamType);
            }
            addStream(pid, KIND_DATA, -1, streamType, carriesPts);
            return this;
        }

        /**
         * Set the PMT descriptor loop for the {@code dataIdx}-th data stream
         * (zero-indexed among {@link #addData} calls, in call order). Each element
         * is one complete descriptor TLV (tag, length, payload), emitted verbatim.
         * {@code dataIdx} is range-checked at {@link #build()}; TLV well-formedness
         * and classification rules are validated at {@code new Muxer(...)}.
         */
        public Builder streamDescriptorsForData(int dataIdx, byte[][] descs) {
            Objects.requireNonNull(descs, "descs");
            for (byte[] d : descs) Objects.requireNonNull(d, "descs element");
            if (dataIdx < 0) {
                throw new IllegalArgumentException("dataIdx must be >= 0, got " + dataIdx);
            }
            dataDescs.put(dataIdx, descs.clone());
            return this;
        }

        private void addStream(int pid, int kind, int codec, int typeCode, boolean carriesPtsFlag) {
            if (pid < 0x0010 || pid > 0x1FFE) {
                throw new IllegalArgumentException(
                    "pid must be in 0x0010..=0x1FFE, got 0x" + Integer.toHexString(pid));
            }
            pids.add(pid);
            kinds.add(kind);
            codecs.add(codec);
            typeCodes.add(typeCode);
            carriesPts.add(carriesPtsFlag);
        }

        /** Finalize. Requires ≥1 stream. Deep validation happens in {@code Muxer::new}. */
        public MuxerConfig build() {
            if (pids.isEmpty()) {
                throw new IllegalArgumentException("at least one stream is required");
            }
            int dataCount = 0;
            for (int k : kinds) {
                if (k == KIND_DATA) dataCount++;
            }
            for (int idx : dataDescs.keySet()) {
                if (idx >= dataCount) {
                    throw new IllegalArgumentException("streamDescriptorsForData dataIdx " + idx
                        + " out of range (" + dataCount + " data streams)");
                }
            }
            return new MuxerConfig(this);
        }
    }
}
