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
 * <p>Program and per-stream PMT descriptors are set with
 * {@link Builder#programDescriptors} and the {@code streamDescriptorsFor*}
 * setters (two {@code SYNCHRONOUS_METADATA} KLV streams in one TS need
 * distinct {@code metadata_service_id}s, which only a caller descriptor sets).
 *
 * <p><b>Deferred (documented):</b> multi-program configs; stats. The handle-targeted
 * pushes ({@link Muxer#pushVideoTo}, {@link Muxer#pushVideoWireTo},
 * {@link Muxer#pushKlvTo}, {@link Muxer#pushAudioTo},
 * {@link Muxer#pushSubtitleTo}, {@link Muxer#pushDataTo}) are available.
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
    private final int pcrLeadMs;
    private final int psiIntervalMs;
    private final int bufferPackets;
    private final Av1CarriageMode av1Carriage;
    private final int[] streamPids;
    private final int[] streamKinds;     // KIND_* per stream
    private final int[] streamCodecs;    // codec ordinal per kind; -1 for KLV/DATA
    // KlvStreamType ordinal for KLV; raw PMT stream_type byte for DATA; -1 otherwise.
    private final int[] streamTypeCodes;
    private final boolean[] streamCarriesPts; // KLV + DATA; false otherwise
    // PMT descriptor loops, flattened: dataDescBytes is every descriptor TLV
    // concatenated, stream by stream in stream order and the program loop last;
    // dataDescLens has one entry per stream (its byte count within the blob, 0
    // for none) and a last entry for the program loop.
    private final byte[] dataDescBytes;
    private final int[] dataDescLens;

    private MuxerConfig(Builder b) {
        this.programNumber = b.programNumber;
        this.pmtPid = b.pmtPid;
        this.pcrPid = b.pcrPid;
        this.pcrIntervalMs = b.pcrIntervalMs;
        this.pcrLeadMs = b.pcrLeadMs;
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
        byte[][][] perStream = b.resolveStreamDescriptors();
        this.dataDescLens = new int[n + 1];
        java.io.ByteArrayOutputStream blob = new java.io.ByteArrayOutputStream();
        for (int i = 0; i <= n; i++) {
            byte[][] descs = i < n ? perStream[i] : b.programDescs;
            if (descs == null) continue;
            int total = 0;
            for (byte[] d : descs) { blob.write(d, 0, d.length); total += d.length; }
            this.dataDescLens[i] = total;
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
    /**
     * How far the PCR runs behind each access unit's decode time, in
     * milliseconds (default 100, valid 0..1000). The PCR is the decode time (the
     * DTS when the push supplies one, else the PTS) minus this lead and steps
     * back only at a signalled restart of the timeline. 0 writes PCR = decode
     * time.
     */
    public int pcrLeadMs() { return pcrLeadMs; }
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
    /** Every PMT descriptor TLV, stream by stream in stream order, then the program loop's. */
    public byte[] dataDescBytes() { return dataDescBytes.clone(); }
    /**
     * Each stream's byte count within {@link #dataDescBytes()}, then the program loop's:
     * {@code streamCount() + 1} entries.
     */
    public int[] dataDescLens() { return dataDescLens.clone(); }

    /**
     * Fluent builder for {@link MuxerConfig}. Defaults mirror
     * {@code tst_core::mpegts::mux::MuxerConfig::default()} scalars
     * (pcrIntervalMs=40, pcrLeadMs=100, psiIntervalMs=100, bufferPackets=10000,
     * av1Carriage=MPEG2_TS_BINDING). {@code programNumber}/{@code pmtPid} are
     * single-valued (one program).
     */
    public static final class Builder {
        private int programNumber = 1;
        private int pmtPid = 0x1000;
        private int pcrPid = -1; // auto-resolve
        private int pcrIntervalMs = 40;
        private int pcrLeadMs = 100;
        private int psiIntervalMs = 100;
        private int bufferPackets = 10_000;
        private Av1CarriageMode av1Carriage = Av1CarriageMode.MPEG2_TS_BINDING;
        private final List<Integer> pids = new ArrayList<>();
        private final List<Integer> kinds = new ArrayList<>();
        private final List<Integer> codecs = new ArrayList<>();
        private final List<Integer> typeCodes = new ArrayList<>();
        private final List<Boolean> carriesPts = new ArrayList<>();
        private byte[][] programDescs;
        // Every streamDescriptorsFor* call in call order; a later call naming
        // the same stream replaces the earlier one, as in the Rust builder.
        private final List<DescriptorTarget> streamDescs = new ArrayList<>();

        /** One {@code streamDescriptorsFor*} call: {@code kind} -1 is the absolute index. */
        private record DescriptorTarget(int kind, int index, byte[][] descs, String setter) {}

        /**
         * @throws IllegalArgumentException if {@code v} is outside 0..=0xFFFF
         */
        public Builder programNumber(int v) {
            if (v < 0 || v > 0xFFFF) {
                throw new IllegalArgumentException(
                    "programNumber must be in 0..=0xFFFF, got " + v);
            }
            this.programNumber = v;
            return this;
        }
        /**
         * @throws IllegalArgumentException if {@code v} is outside 0x0010..=0x1FFE
         */
        public Builder pmtPid(int v) {
            requirePid(v, "pmtPid");
            this.pmtPid = v;
            return this;
        }
        /**
         * Pin the PCR PID to a configured stream's PID. Default: auto-resolve (first video, else KLV/audio).
         *
         * @param v -1 (auto-resolve) or a PID in 0x0010..=0x1FFE
         * @throws IllegalArgumentException if {@code v} is neither
         */
        public Builder pcrPid(int v) {
            if (v != -1) requirePid(v, "pcrPid");
            this.pcrPid = v;
            return this;
        }

        private static void requirePid(int pid, String name) {
            if (pid < 0x0010 || pid > 0x1FFE) {
                throw new IllegalArgumentException(
                    name + " must be in 0x0010..=0x1FFE, got 0x" + Integer.toHexString(pid));
            }
        }
        public Builder pcrIntervalMs(int v) { this.pcrIntervalMs = v; return this; }
        /**
         * How far the PCR runs behind each access unit's decode time, in
         * milliseconds (default 100, valid 0..1000). The PCR is the decode time
         * (the DTS when the push supplies one, else the PTS) minus this lead and
         * steps back only at a signalled restart of the timeline. 0 writes PCR =
         * decode time. The range is checked at {@code new Muxer(...)}
         * ({@code CONFIG_INVALID}).
         */
        public Builder pcrLeadMs(int v) { this.pcrLeadMs = v; return this; }
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
         * Set the program-level PMT descriptors (the program info loop, before
         * the per-stream entries). Each element is one complete descriptor TLV
         * (tag, length, payload), emitted verbatim; one that is not is refused at
         * {@link #build()} ({@code IllegalArgumentException}).
         */
        public Builder programDescriptors(byte[][] descs) {
            this.programDescs = checkedDescriptors(descs);
            return this;
        }

        /**
         * Set the PMT descriptor loop for the {@code videoIdx}-th video stream
         * (zero-indexed among {@link #addVideo} calls, in call order); the
         * caller's descriptors follow the muxer's own (an AV1 stream's AV01
         * registration is not repeated when the caller supplies one).
         * {@code videoIdx} and each element's TLV form (one descriptor) are
         * checked at {@link #build()}.
         */
        public Builder streamDescriptorsForVideo(int videoIdx, byte[][] descs) {
            return addTarget(KIND_VIDEO, videoIdx, descs, "streamDescriptorsForVideo", "videoIdx");
        }

        /**
         * Set the PMT descriptor loop for the {@code klvIdx}-th KLV stream
         * (zero-indexed among {@link #addKlv} calls, in call order). A caller
         * {@code metadata_descriptor} (tag 0x26) replaces the muxer's own,
         * which declares {@code metadata_service_id} 0; two
         * {@code SYNCHRONOUS_METADATA} streams in one TS must declare distinct
         * service ids. {@code klvIdx} and each element's TLV form (one
         * descriptor) are checked at {@link #build()}; the service-id rule at
         * {@code new Muxer(...)}.
         */
        public Builder streamDescriptorsForKlv(int klvIdx, byte[][] descs) {
            return addTarget(KIND_KLV, klvIdx, descs, "streamDescriptorsForKlv", "klvIdx");
        }

        /**
         * Set the PMT descriptor loop for the {@code audioIdx}-th audio stream
         * (zero-indexed among {@link #addAudio} calls, in call order).
         * {@code audioIdx} and each element's TLV form (one descriptor) are
         * checked at {@link #build()}.
         */
        public Builder streamDescriptorsForAudio(int audioIdx, byte[][] descs) {
            return addTarget(KIND_AUDIO, audioIdx, descs, "streamDescriptorsForAudio", "audioIdx");
        }

        /**
         * Set the PMT descriptor loop for the {@code subtitleIdx}-th subtitle
         * stream (zero-indexed among {@link #addSubtitle} calls, in call
         * order). A recognised subtitle codec marker among them replaces the
         * muxer's own. {@code subtitleIdx} and each element's TLV form (one
         * descriptor) are checked at {@link #build()}.
         */
        public Builder streamDescriptorsForSubtitle(int subtitleIdx, byte[][] descs) {
            return addTarget(
                KIND_SUBTITLE, subtitleIdx, descs, "streamDescriptorsForSubtitle", "subtitleIdx");
        }

        /**
         * Set the PMT descriptor loop for the {@code dataIdx}-th data stream
         * (zero-indexed among {@link #addData} calls, in call order). Each element
         * is one complete descriptor TLV (tag, length, payload), emitted verbatim.
         * {@code dataIdx} and each element's TLV form (one descriptor) are checked
         * at {@link #build()}; the classification rules at {@code new Muxer(...)}.
         */
        public Builder streamDescriptorsForData(int dataIdx, byte[][] descs) {
            return addTarget(KIND_DATA, dataIdx, descs, "streamDescriptorsForData", "dataIdx");
        }

        /**
         * Set the PMT descriptor loop for a stream by its absolute index among
         * every {@code add*} call of this program, in call order. A later
         * {@code streamDescriptorsFor*} call naming the same stream replaces
         * this one, and this one replaces an earlier one.
         * {@code streamIdx} and each element's TLV form (one descriptor) are
         * checked at {@link #build()}.
         */
        public Builder streamDescriptorsForStream(int streamIdx, byte[][] descs) {
            return addTarget(-1, streamIdx, descs, "streamDescriptorsForStream", "streamIdx");
        }

        private Builder addTarget(int kind, int idx, byte[][] descs, String setter, String idxName) {
            byte[][] checked = checkedDescriptors(descs);
            if (idx < 0) {
                throw new IllegalArgumentException(idxName + " must be >= 0, got " + idx);
            }
            streamDescs.add(new DescriptorTarget(kind, idx, checked, setter));
            return this;
        }

        private static byte[][] checkedDescriptors(byte[][] descs) {
            Objects.requireNonNull(descs, "descs");
            for (byte[] d : descs) Objects.requireNonNull(d, "descs element");
            return descs.clone();
        }

        /**
         * Each stream's descriptor list (null for none), the calls applied in
         * call order.
         *
         * @throws IllegalArgumentException for an index past its kind's count
         */
        private byte[][][] resolveStreamDescriptors() {
            byte[][][] out = new byte[kinds.size()][][];
            for (DescriptorTarget t : streamDescs) {
                int abs = -1;
                int count = 0;
                for (int i = 0; i < kinds.size(); i++) {
                    if (t.kind() != -1 && kinds.get(i) != t.kind()) continue;
                    if (count == t.index()) abs = i;
                    count++;
                }
                if (abs < 0) {
                    throw new IllegalArgumentException(t.setter() + " index " + t.index()
                        + " out of range (" + count + " such streams)");
                }
                out[abs] = t.descs();
            }
            return out;
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

        /**
         * Finalize. Requires ≥1 stream, every descriptor index in range and every
         * descriptor element one well-formed descriptor. Deep validation happens in
         * {@code Muxer::new}.
         *
         * @throws IllegalArgumentException for no stream, an index past its kind's
         *     count, or an element that is not exactly one descriptor (tag, length,
         *     that many bytes), naming the setter and the element's index
         */
        public MuxerConfig build() {
            if (pids.isEmpty()) {
                throw new IllegalArgumentException("at least one stream is required");
            }
            if (programDescs != null) requireOneDescriptorEach("programDescriptors", programDescs);
            for (DescriptorTarget t : streamDescs) requireOneDescriptorEach(t.setter(), t.descs());
            resolveStreamDescriptors();
            return new MuxerConfig(this);
        }

        /**
         * The elements are concatenated on their way to the native, which splits them
         * again by their length bytes, so a malformed element would merge with its
         * neighbour; each is checked here on its own, as the core checks a list.
         */
        private static void requireOneDescriptorEach(String setter, byte[][] descs) {
            for (int i = 0; i < descs.length; i++) {
                byte[] d = descs[i];
                if (d.length < 2) {
                    throw new IllegalArgumentException(setter + " element " + i
                        + ": a descriptor is at least 2 bytes (tag, length), got " + d.length);
                }
                int declared = d[1] & 0xFF;
                if (declared != d.length - 2) {
                    throw new IllegalArgumentException(setter + " element " + i
                        + ": the length byte declares " + declared + " payload bytes, "
                        + (d.length - 2) + " follow");
                }
            }
        }
    }
}
