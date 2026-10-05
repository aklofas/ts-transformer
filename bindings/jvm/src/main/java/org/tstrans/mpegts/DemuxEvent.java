package org.tstrans.mpegts;

import java.nio.ByteBuffer;
import java.util.List;
import org.tstrans.CodecParseException;
import org.tstrans.DemuxException;
import org.tstrans.codec.AudioFrame;
import org.tstrans.codec.VideoUnit;

/**
 * A demuxed event. Sealed sum type mirroring
 * {@code tst_core::mpegts::demux::DemuxEvent}. The full top-level
 * event set is now surfaced: {@link ProgramMap}, the four sample records
 * ({@link Video} / {@link Audio} / {@link Subtitle} / {@link UnknownSample},
 * mirroring tst-py), {@link Metadata} (KLV), {@link NonConformant}
 * (stream-conformance diagnostics), {@link Discontinuity} (continuity /
 * PES-reassembly discontinuity, carrying a {@link StreamId} and a typed
 * {@link DiscontinuityKind}), and {@link ReconnectDiscontinuity} (transport-level
 * reconnect). No variant is skipped.
 */
public sealed interface DemuxEvent
        permits DemuxEvent.ProgramMap, DemuxEvent.Video, DemuxEvent.Audio,
                DemuxEvent.Subtitle, DemuxEvent.UnknownSample, DemuxEvent.Metadata,
                DemuxEvent.NonConformant, DemuxEvent.Discontinuity,
                DemuxEvent.ReconnectDiscontinuity {

    /**
     * PSI program map for one program (mirrors tst-py mpegts.ProgramMap).
     *
     * @param programNumber  the MPEG-TS program number, from the PAT entry
     * @param pcrPid         PID carrying the program's PCR (the PMT's PCR_PID field)
     * @param pmtPid         PID carrying this program's PMT, from the PAT entry
     *                       that declared the program
     * @param elementaryPids PIDs of the program's elementary streams, in PMT order
     */
    record ProgramMap(int programNumber, int pcrPid, int pmtPid, List<Integer> elementaryPids) implements DemuxEvent {}

    /**
     * A video access unit, carrying the raw encoded bytes. {@code raw} is the
     * exact encoded access unit; call {@link #parse()} to obtain the typed
     * {@link List} of {@link VideoUnit} on demand — {@code NalUnit}s for
     * H.264/H.265/H.266, {@code Obu}s for AV1. The {@code codec} field
     * disambiguates which. Mirrors tst-py's raw-first model where
     * {@code DemuxEvent.Video.parse()} is the opt-in split call.
     *
     * @param stream                the elementary stream this sample belongs to
     * @param pts                   presentation timestamp in 90&nbsp;kHz ticks
     * @param dts                   decode timestamp in 90&nbsp;kHz ticks, or
     *                              {@code null} when absent
     * @param codec                 the video codec (also on {@code stream.kind()})
     * @param raw                   the exact encoded access unit — Annex-B byte
     *                              stream for H.264/H.265/H.266, on-wire PES
     *                              payload for AV1 — as a heap (JVM-owned)
     *                              {@link ByteBuffer} copy, safe to retain (true
     *                              zero-copy is deferred to a JDK&nbsp;22+ FFM
     *                              path). For H.264/H.265/H.266, feed it back to
     *                              {@link Muxer#pushVideo(byte[], long, boolean)}
     *                              for byte-faithful transmux. For AV1, use
     *                              {@link Muxer#pushVideoWire(byte[], long, boolean)}
     *                              instead (with a destination muxer configured to
     *                              the same {@link #av1Carriage()} mode) — {@code
     *                              pushVideo} would re-wrap the wire bytes and
     *                              corrupt the stream. Mirrors tst-py's {@code .raw}.
     * @param randomAccessIndicator whether this access unit is a random-access
     *                              point (keyframe)
     * @param av1Carriage           AV1 carriage provenance
     *                              ({@link Av1CarriageMode#MPEG2_TS_BINDING} or
     *                              {@link Av1CarriageMode#INTEROP_RAW_OBU});
     *                              {@code null} for non-AV1 codecs. For
     *                              byte-faithful re-mux, configure the destination
     *                              muxer to this carriage mode and push {@code raw}
     *                              via {@link Muxer#pushVideoWire}.
     */
    record Video(StreamId stream, long pts, Long dts, VideoCodec codec,
                 ByteBuffer raw, boolean randomAccessIndicator,
                 Av1CarriageMode av1Carriage) implements DemuxEvent {

        /**
         * Opt-in: parse the raw access unit into typed codec units —
         * {@link org.tstrans.codec.NalUnit} for H.264/H.265/H.266,
         * {@link org.tstrans.codec.Obu} for AV1. Mirrors Python's
         * {@code DemuxEvent.Video.parse()}.
         *
         * <p>Each invocation calls the native {@code split_video}; cache the
         * result if you need it more than once. Position-independent: the
         * stored {@code raw} buffer's current position/limit are ignored, so
         * a consumer that read {@code raw()} directly beforehand does not
         * truncate the parse.
         *
         * @return the typed codec units (never {@code null}; empty only when
         *         the access unit contained no parseable units)
         * @throws DemuxException on an internal binding failure (e.g. JNI
         *         allocation); never for merely-malformed access units, which
         *         yield an empty list
         */
        public List<VideoUnit> parse() throws DemuxException {
            // clear() on the duplicate resets position=0/limit=capacity on the
            // VIEW only (the record's buffer is untouched) — guards against a
            // consumer having advanced the shared buffer's position via raw().
            ByteBuffer r = raw().duplicate().clear();
            byte[] bytes;
            if (r.hasArray() && r.arrayOffset() == 0 && r.array().length == r.remaining()) {
                // Common case: the demuxer wraps an exact-size heap array —
                // hand it to the native directly (it only reads; the JNI side
                // copies into Rust) instead of paying a second Java-side copy.
                bytes = r.array();
            } else {
                bytes = new byte[r.remaining()];
                r.get(bytes);
            }
            // av1Carriage() null (non-AV1 codec) → ordinal 0 (MPEG2_TS_BINDING default;
            // split_video ignores the carriage arg for non-AV1 codecs).
            int carriageOrd = av1Carriage() != null ? av1Carriage().ordinal() : 0;
            return DemuxEventVideoNatives.nSplitVideo(bytes, codec().ordinal(), carriageOrd);
        }
    }

    /**
     * An audio access unit, carrying the raw encoded elementary-stream bytes.
     * {@code raw} is the exact encoded audio ES; call {@link #parse()} to obtain
     * the typed {@link List} of {@link AudioFrame} on demand — {@code AdtsFrame}s
     * for AAC, {@code Mpeg2AudioFrame}s for MP2, and an <b>empty list</b> for
     * codecs with no typed parser (AAC-LATM, AC-3 — read {@link #raw()} directly).
     * The {@code codec} field disambiguates which. Mirrors tst-py's raw-first
     * model where {@code DemuxEvent.Audio.parse()} is the opt-in parse call, and
     * the {@link Video} shape.
     *
     * @param stream the elementary stream this sample belongs to
     * @param pts    presentation timestamp in 90&nbsp;kHz ticks
     * @param dts    decode timestamp in 90&nbsp;kHz ticks, or {@code null} when absent
     * @param codec  the audio codec (also on {@code stream.kind()})
     * @param raw    the exact encoded audio elementary-stream bytes as a heap
     *               (JVM-owned) {@link ByteBuffer} copy, safe to retain (true
     *               zero-copy is deferred to a JDK&nbsp;22+ FFM path). Mirrors
     *               tst-py's {@code .raw}.
     */
    record Audio(StreamId stream, long pts, Long dts, AudioCodec codec,
                 ByteBuffer raw) implements DemuxEvent {

        /**
         * Opt-in: parse the raw audio elementary-stream bytes into typed audio
         * frames — {@link org.tstrans.codec.AdtsFrame} for AAC,
         * {@link org.tstrans.codec.Mpeg2AudioFrame} for MP2; an <b>empty list</b>
         * for codecs with no typed parser (AAC-LATM, AC-3 — read {@link #raw()}
         * directly). Lenient: skips past corruption to the next valid frame.
         * Mirrors Python's {@code DemuxEvent.Audio.parse()}.
         *
         * <p>Each invocation re-parses {@code raw}; cache the result if you need
         * it more than once. Position-independent: the stored {@code raw}
         * buffer's current position/limit are ignored, so a consumer that read
         * {@code raw()} directly beforehand does not truncate the parse.
         *
         * @return the typed audio frames (never {@code null}; empty for codecs
         *         with no typed parser, or when the ES held no parseable frame)
         * @throws CodecParseException never in this lenient mode (kept in the
         *         signature so {@link #parse(boolean)} and this overload share
         *         one call shape)
         * @throws DemuxException on an internal binding failure (e.g. enum
         *         drift in the native {@code nParseAudio}); mirrors
         *         {@link DemuxEvent.Video#parse()}
         */
        public List<AudioFrame> parse() throws CodecParseException, DemuxException {
            return parse(false);
        }

        /**
         * Strict variant: throws {@link CodecParseException} on the first
         * malformed frame instead of resyncing past it. Codecs with no typed
         * parser (AAC-LATM, AC-3) still return an empty list. Mirrors Python's
         * {@code DemuxEvent.Audio.parse(strict=True)}.
         *
         * @param strict {@code true} to throw on the first malformed frame,
         *               {@code false} to skip past corruption (as {@link #parse()})
         * @return the typed audio frames (never {@code null})
         * @throws CodecParseException in strict mode, on the first malformed frame
         * @throws DemuxException on an internal binding failure (e.g. enum
         *         drift in the native {@code nParseAudio}); mirrors
         *         {@link DemuxEvent.Video#parse()}
         */
        public List<AudioFrame> parse(boolean strict) throws CodecParseException, DemuxException {
            // clear() on the duplicate resets position=0/limit=capacity on the
            // VIEW only (the record's buffer is untouched) — guards against a
            // consumer having advanced the shared buffer's position via raw().
            ByteBuffer r = raw().duplicate().clear();
            byte[] bytes;
            if (r.hasArray() && r.arrayOffset() == 0 && r.array().length == r.remaining()) {
                // Common case: the demuxer wraps an exact-size heap array — hand
                // it to the native directly (it only reads; the JNI side copies
                // into Rust) instead of paying a second Java-side copy.
                bytes = r.array();
            } else {
                bytes = new byte[r.remaining()];
                r.get(bytes);
            }
            return DemuxEventAudioNatives.nParseAudio(bytes, codec().ordinal(), strict);
        }
    }

    /**
     * A subtitle access unit.
     *
     * @param stream  the elementary stream this sample belongs to
     * @param pts     presentation timestamp in 90&nbsp;kHz ticks
     * @param dts     decode timestamp in 90&nbsp;kHz ticks, or {@code null} when absent
     * @param codec   the subtitle codec (also on {@code stream.kind()})
     * @param payload the access-unit bytes as a heap (JVM-owned) {@link ByteBuffer}
     *                — a copy, safe to retain and read at any time (including
     *                after the next {@link Demuxer#nextEvent()} or
     *                {@link Demuxer#close()}). True zero-copy is deferred to a
     *                future JDK&nbsp;22+ Foreign Function &amp; Memory path; this
     *                JDK&nbsp;17 baseline copies.
     */
    record Subtitle(StreamId stream, long pts, Long dts, SubtitleCodec codec,
                    ByteBuffer payload) implements DemuxEvent {}

    /**
     * An access unit on a stream whose codec the demuxer does not recognize.
     * {@code streamType} is the raw PMT {@code stream_type} byte (0..=255).
     *
     * @param stream     the elementary stream this sample belongs to
     * @param pts        presentation timestamp in 90&nbsp;kHz ticks
     * @param dts        decode timestamp in 90&nbsp;kHz ticks, or {@code null} when absent
     * @param streamType the raw PMT {@code stream_type} byte (0..=255)
     * @param payload    the access-unit bytes as a heap (JVM-owned)
     *                   {@link ByteBuffer} — a copy, safe to retain and read at
     *                   any time (including after the next
     *                   {@link Demuxer#nextEvent()} or {@link Demuxer#close()}).
     *                   True zero-copy is deferred to a future JDK&nbsp;22+
     *                   Foreign Function &amp; Memory path; this JDK&nbsp;17
     *                   baseline copies.
     */
    record UnknownSample(StreamId stream, long pts, Long dts, int streamType,
                         ByteBuffer payload) implements DemuxEvent {}

    /**
     * Standalone metadata — KLV (sync AU-cell or async) or an unrecognized
     * metadata stream. Mirrors tst-py's metadata event.
     *
     * @param stream         the elementary stream this metadata belongs to
     * @param pts            presentation timestamp in 90&nbsp;kHz ticks
     * @param kind           the metadata classification (see {@link MetadataKind})
     * @param payload        Raw KLV LS bytes; the H.222.0 §2.12.4.2 AU-cell header
     *                       is already stripped. Heap-copied / JVM-owned (safe to
     *                       retain; FFM zero-copy deferred to a JDK-22+ path).
     *                       Decode with the (future) {@code org.tstrans.klv} module.
     * @param wasReassembled {@code true} if a multi-cell AU was reassembled from
     *                       First + 0..n Middle + Last cells (always {@code false}
     *                       for async / unknown metadata)
     * @param cellCount      number of AU cells that contributed to this event
     *                       ({@code 1} for single-cell / async / unknown)
     */
    record Metadata(StreamId stream, long pts, MetadataKind kind, ByteBuffer payload,
                    boolean wasReassembled, int cellCount) implements DemuxEvent {}

    /**
     * A stream-conformance diagnostic emitted by the demuxer. Mirrors tst-py's
     * NonConformant event: Rust's 30+ {@code NonConformantIssue} variants are
     * collapsed to a {@link NonConformantKind} discriminator plus the
     * human-readable {@code issue} detail string.
     *
     * @param stream            the elementary stream the diagnostic concerns
     * @param issue             the human-readable detail string (Rust
     *                          {@code NonConformantIssue}'s {@code Display})
     * @param kind              the collapsed diagnostic discriminator
     * @param multiCellAuReason the multi-cell AU reassembly failure reason —
     *                          non-{@code null} only when
     *                          {@code kind == MULTI_CELL_AU}, {@code null} otherwise
     * @param observedCfi       the wire {@code cell_fragment_indication} bits —
     *                          non-{@code null} only when
     *                          {@code kind == CFI_TOLERATED}, {@code null} otherwise
     * @param treatedAs         the {@code cell_fragment_indication} the demuxer
     *                          substituted — non-{@code null} only when
     *                          {@code kind == CFI_TOLERATED}, {@code null} otherwise
     */
    record NonConformant(StreamId stream, String issue, NonConformantKind kind,
                         MultiCellAuReason multiCellAuReason, CellFragmentIndication observedCfi,
                         CellFragmentIndication treatedAs) implements DemuxEvent {}

    /**
     * A continuity-counter / PES-reassembly discontinuity on a stream. Mirrors
     * {@code tst_core::...::DemuxEvent::Discontinuity}.
     *
     * @param stream the elementary stream the discontinuity concerns
     * @param kind   the discontinuity classification (see {@link DiscontinuityKind})
     */
    record Discontinuity(StreamId stream, DiscontinuityKind kind) implements DemuxEvent {}

    /** Transport-level reconnect occurred between the prior event and this one; all per-stream state was dropped (re-derived from the next PAT/PMT). Mirrors {@code tst_core::...::DemuxEvent::ReconnectDiscontinuity}. */
    record ReconnectDiscontinuity() implements DemuxEvent {}
}

/**
 * Package-private JNI entry point for {@link DemuxEvent.Video#parse()}.
 *
 * <p>Records cannot declare {@code native} methods (JLS §8.10.3), so the
 * native declaration lives here and {@code Video.parse()} delegates to it.
 * The JNI symbol is {@code Java_org_tstrans_mpegts_DemuxEventVideoNatives_nSplitVideo}.
 */
final class DemuxEventVideoNatives {
    private DemuxEventVideoNatives() {}

    static { org.tstrans.NativeLoader.load(); }

    static native java.util.List<org.tstrans.codec.VideoUnit> nSplitVideo(
            byte[] raw, int codecOrdinal, int av1CarriageOrdinal)
            throws org.tstrans.DemuxException;
}

/**
 * Package-private JNI entry point for {@link DemuxEvent.Audio#parse()}.
 *
 * <p>Records cannot declare {@code native} methods (JLS §8.10.3), so the
 * native declaration lives here and {@code Audio.parse()} delegates to it.
 * The JNI symbol is {@code Java_org_tstrans_mpegts_DemuxEventAudioNatives_nParseAudio}.
 */
final class DemuxEventAudioNatives {
    private DemuxEventAudioNatives() {}

    static { org.tstrans.NativeLoader.load(); }

    static native java.util.List<org.tstrans.codec.AudioFrame> nParseAudio(
            byte[] raw, int codecOrdinal, boolean strict)
            throws org.tstrans.CodecParseException, org.tstrans.DemuxException;
}
