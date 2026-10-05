package org.tstrans.scenarios;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.sha256Units;

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.codec.AdtsFrame;
import org.tstrans.codec.AudioFrame;
import org.tstrans.codec.NalUnit;
import org.tstrans.codec.VideoUnit;
import org.tstrans.io.Io;
import org.tstrans.klv.Klv;
import org.tstrans.klv.UasDatalinkLs;
import org.tstrans.mpegts.AudioCodec;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.VideoCodec;

/**
 * Java adapter for the cross-binding scenario harness shared by Rust, Python,
 * C and Java, mirroring the
 * Python in-process adapter at {@code bindings/python/tests/test_scenarios.py}.
 *
 * <p>It reads the SHARED committed fixtures under
 * {@code crates/tst-integration/tests/fixtures/scenarios/h264-st0601-mp/}
 * ({@code input.ts} + {@code golden.json}) — the same artifacts the Rust and
 * Python adapters consume — demuxes the bytes through the JNI
 * {@link org.tstrans.mpegts.Demuxer} keystone, and asserts the result against
 * the committed golden.
 *
 * <h2>What this test compares</h2>
 * <ul>
 *   <li><b>video.pid</b> — the elementary-stream PID of the video sample.</li>
 *   <li><b>video.pts</b> — the 90&nbsp;kHz presentation timestamp.</li>
 *   <li><b>video.payload_sha256</b> — SHA-256 of the concatenated NAL RBSP
 *       payload bytes. {@link DemuxEvent.Video#parse()} returns the typed
 *       {@code List<VideoUnit>} on demand; this test concatenates every
 *       {@code ((NalUnit) unit).payload()} (Annex-B start codes already stripped
 *       by the demuxer) the same way the Rust/Python normalisers do
 *       (see {@code video_payload_bytes()} in the Rust normaliser), so the digest
 *       matches byte-for-byte. This is the strong cross-binding proof.</li>
 *   <li><b>klv.pid</b> — the elementary-stream PID of the KLV metadata stream
 *       ({@code DemuxEvent.Metadata.stream().pid()}).</li>
 *   <li><b>klv.set</b> — the MISB set identity derived from the first 13 bytes
 *       of the raw KLV payload using the ST&nbsp;0601 UAS Datalink LS UL prefix
 *       ({@code "st0601"} or {@code "unknown"}). This mirrors {@code _klv_set_from_ul}
 *       in the Python adapter and {@code klv_set_from_ul()} in the Rust normaliser.</li>
 * </ul>
 * The test reproduces BOTH the video subset AND the klv core event (pid +
 * UL-derived set) — {@code DemuxEvent.Metadata} surfaces in the binding.
 *
 * <h2>What is out of scope here</h2>
 * <ul>
 *   <li>video {@code stream_type} / {@code key} — the {@code DemuxEvent.Video}
 *       record exposes the stream PID, PTS, and payload (asserted here) plus the
 *       random-access flag, but the cross-binding video subset compares only
 *       pid / pts / payload_sha256; the golden's {@code stream_type} and
 *       {@code key} fields stay out of scope for this proof.</li>
 *   <li>the klv {@code MetadataKind} — the golden records only {@code set}; the
 *       {@code KLV_SYNC_AU_CELL} kind is covered by {@code MetadataEventTest} on
 *       the synchronous-KLV fixture. This test's klv contract is pid + set parity,
 *       matching the Python adapter.</li>
 * </ul>
 */
class ScenarioReproductionTest {

    private static final String SCENARIO_ID = "h264-st0601-mp";

    /**
     * The 13-byte MISB ST 0601 UAS Datalink LS Universal Label prefix. A KLV
     * payload whose first 13 bytes equal this prefix is the ST 0601 set. Mirrors
     * {@code klv_set_from_ul()} in the Rust normaliser and {@code _ST0601_UL_PREFIX}
     * in the Python adapter.
     */
    private static final byte[] ST0601_UL_PREFIX = {
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01,
    };

    /** Workspace-relative shared scenario dir; resolved from Gradle's user.dir (bindings/jvm). */
    private static Path scenarioDir() {
        return Path.of(
                System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios", SCENARIO_ID)
            .normalize();
    }

    @Test
    void reproducesH264St0601MpVideoSubset() throws Exception {
        Path dir = scenarioDir();
        Path inputPath = dir.resolve("input.ts");
        Path goldenPath = dir.resolve("golden.json");

        // Skip-guard: the shared fixtures ARE committed; their absence is a hard
        // failure (the cross-binding contract relies on the single-sourced golden).
        assertTrue(Files.isRegularFile(inputPath),
            "shared scenario input missing (expected committed fixture): " + inputPath);
        assertTrue(Files.isRegularFile(goldenPath),
            "shared scenario golden missing (expected committed fixture): " + goldenPath);

        byte[] tsBytes = Files.readAllBytes(inputPath);
        String goldenJson = Files.readString(goldenPath, StandardCharsets.UTF_8);

        // Extract the single `video` core event's pid, pts, payload_sha256 from
        // the golden. No JSON lib is on the test classpath (build.gradle.kts has
        // only junit) — hand-roll a minimal extraction over the one video object
        // rather than add a dependency just for this.
        GoldenVideo expected = extractVideoEvent(goldenJson);
        GoldenKlv expectedKlv = extractKlvEvent(goldenJson);

        // Demux the shared input through the JNI binding, collecting Video events
        // and Metadata (KLV) events in the same pass. Video.raw and the KLV
        // payload are JVM-owned heap copies, so reading them inside the loop (or
        // later) is equally safe.
        List<VideoSample> videoSamples = new ArrayList<>();
        List<MetadataSample> metadataSamples = new ArrayList<>();
        // Keep the matched Video EVENT (not just the projected sample) so we can
        // assert the typed payload structure below via parse().
        DemuxEvent.Video matchedVideoEvent = null;
        try (Demuxer d = new Demuxer()) {
            d.feed(tsBytes);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v) {
                    videoSamples.add(new VideoSample(v.stream().pid(), v.pts(), sha256Units(v.parse())));
                    if (matchedVideoEvent == null
                            && v.stream().pid() == expected.pid && v.pts() == expected.pts) {
                        matchedVideoEvent = v;
                    }
                } else if (e instanceof DemuxEvent.Metadata m) {
                    metadataSamples.add(
                        new MetadataSample(m.stream().pid(), klvSetFromUl(m.payload())));
                }
            }
        }

        // Assert the keystone subset: a VIDEO sample exists matching the golden's
        // pid AND pts.
        VideoSample match = videoSamples.stream()
            .filter(v -> v.pid == expected.pid && v.pts == expected.pts)
            .findFirst()
            .orElse(null);
        assertNotNull(match,
            "no VIDEO Sample matched golden pid=" + expected.pid + " pts=" + expected.pts
                + "; observed=" + videoSamples);

        // payload_sha256 cross-binding proof: the JNI keystone's Video.payload is
        // the concatenated NAL RBSP bytes (same derivation as the Rust/Python
        // golden builders), so the digest matches the committed golden.
        assertEquals(expected.payloadSha256, match.payloadSha256,
            "video payload_sha256 mismatch — the JNI keystone Video.payload bytes "
                + "must equal the concatenated NAL RBSP bytes the golden hashes");

        // Typed-structure cross-binding proof: the raw-sha above proves
        // the bytes agree; these assertions prove the JVM typed split agrees
        // STRUCTURALLY with Rust/Python — same codec discriminant, same VideoUnit
        // taxonomy, same NAL typing.
        assertNotNull(matchedVideoEvent, "matched Video event should have been captured");
        assertEquals(VideoCodec.H264, matchedVideoEvent.codec(),
            "the h264-st0601-mp scenario's video stream must be tagged H264");
        List<VideoUnit> units = matchedVideoEvent.parse();
        assertFalse(units.isEmpty(),
            "H264 Video.parse() must return a non-empty List<VideoUnit>");
        // First unit is the IDR NAL: synthetic_h264_idr() is `00 00 00 01 65 …`,
        // nal_type = 0x65 & 0x1F = 5 (IDR slice). Downcast via instanceof (no
        // switch-on-sealed; JDK 17).
        VideoUnit first = units.get(0);
        assertTrue(first instanceof NalUnit,
            "H264 VideoUnit must be a NalUnit, was " + first.getClass().getSimpleName());
        NalUnit firstNal = (NalUnit) first;
        assertEquals("H264", firstNal.kind(),
            "first NAL unit must carry the H264 codec discriminant");
        assertEquals(5, firstNal.nalType(),
            "first NAL unit must be the IDR slice (nal_type 5)");

        // Assert the klv core subset: a Metadata event exists whose stream PID
        // matches the golden's klv pid AND whose ST0601-UL-derived set matches the
        // golden's set. This mirrors the Python adapter's klv projection (pid + set).
        MetadataSample klvMatch = metadataSamples.stream()
            .filter(m -> m.pid == expectedKlv.pid && m.set.equals(expectedKlv.set))
            .findFirst()
            .orElse(null);
        assertNotNull(klvMatch,
            "no Metadata event matched golden klv pid=" + expectedKlv.pid
                + " set=" + expectedKlv.set + "; observed=" + metadataSamples);
    }

    /**
     * Cross-binding parity for the FILE path: feed the same {@code h264-st0601-mp}
     * shared golden through {@link org.tstrans.io.Io#parseFile} (reading the committed
     * {@code input.ts} straight off disk) and assert the SAME video subset the
     * in-memory feed path reproduces. Proves file-path ≡ feed-path through the JVM.
     */
    @Test
    void reproducesH264St0601MpViaParseFile() throws Exception {
        Path dir = scenarioDir();
        Path inputPath = dir.resolve("input.ts");
        Path goldenPath = dir.resolve("golden.json");
        assertTrue(Files.isRegularFile(inputPath),
            "shared scenario input missing: " + inputPath);
        assertTrue(Files.isRegularFile(goldenPath),
            "shared scenario golden missing: " + goldenPath);
        GoldenVideo expected =
            extractVideoEvent(Files.readString(goldenPath, StandardCharsets.UTF_8));

        VideoSample match = null;
        // Explicit iterator loop (not findFirst().map(...)): sha256Units throws a
        // checked Exception, which a Stream lambda can't propagate cleanly. Iterating
        // stream.iterator() keeps the digest call in a context that can throw, and
        // drops the puzzling (Iterable) cast the enhanced-for-loop otherwise needs.
        try (var stream = Io.parseFile(inputPath)) {
            for (Iterator<DemuxEvent> it = stream.iterator(); it.hasNext(); ) {
                DemuxEvent e = it.next();
                if (e instanceof DemuxEvent.Video v
                        && v.stream().pid() == expected.pid && v.pts() == expected.pts) {
                    match = new VideoSample(v.stream().pid(), v.pts(), sha256Units(v.parse()));
                    break;
                }
            }
        }
        assertNotNull(match,
            "parseFile produced no VIDEO sample matching golden pid=" + expected.pid
                + " pts=" + expected.pts);
        assertEquals(expected.payloadSha256, match.payloadSha256,
            "parseFile video payload_sha256 must equal the committed golden "
                + "(file path must reproduce the feed path byte-for-byte)");
    }

    /**
     * Typed-decode cross-binding parity: feed the KLV Metadata payload from the
     * {@code h264-st0601-mp} shared golden through the JVM KLV surface and verify
     * behavior agrees with the Rust and Python adapters.
     *
     * <p>The {@code h264-st0601-mp} scenario carries {@code minimal_st0601_ls()} —
     * the 16-byte ST 0601 UAS Datalink LS UL + a single BER length byte {@code 0x00}
     * (empty body, NO populated tags, NO checksum). This is a structurally degenerate
     * KLV record: the empty body means the mandatory Tag-1 checksum is absent, so
     * {@code tst_core::klv::st0601::decode} (and tst-py's {@code decode_uas_datalink})
     * both throw / raise a TRUNCATED_SET error at offset 17 ("needed 3 bytes, have 0").
     *
     * <p>Cross-binding parity assertion:
     * <ol>
     *   <li>The JVM {@link Klv#isSt0601Family(byte[])} correctly identifies the
     *       payload as an ST 0601 family record (the UL check does not require a
     *       valid body — it only inspects the 16-byte UL prefix).</li>
     *   <li>The JVM {@link Klv#decodeUasDatalink(byte[])} throws a
     *       {@link org.tstrans.KlvDecodeException} with kind {@code TRUNCATED_SET},
     *       matching the {@code KlvDecodeError::Truncated} that Rust and Python
     *       produce for the same input (verified: Python also raises
     *       {@code KlvError(TRUNCATED_SET)} with message
     *       "buffer truncated at offset 17: needed 3 bytes, have 0").</li>
     * </ol>
     *
     * <p>Rich typed-field correctness is covered by {@code St0601Test} with real
     * fixtures (synthetic_full.klv + synthetic_minimal.klv). The golden scenario
     * fixture is intentionally minimal — its purpose is the mux/demux round-trip
     * proof, not KLV decode richness.
     *
     * <p>See {@code payload_sha256: 9b3800ff…} in {@code h264-st0601-mp/golden.json}
     * — that hash is the VIDEO NAL RBSP digest; the KLV payload sha is not committed
     * in the golden (it is the empty-body 17-byte LS).
     */
    @Test
    void typedKlvDecodeParityFromSharedGolden() throws Exception {
        Path dir = scenarioDir();
        Path inputPath = dir.resolve("input.ts");
        assertTrue(Files.isRegularFile(inputPath),
            "shared scenario input missing (expected committed fixture): " + inputPath);

        byte[] tsBytes = Files.readAllBytes(inputPath);

        // Collect the first ST 0601 Metadata payload from the shared scenario.
        List<byte[]> klvPayloads = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(tsBytes);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Metadata m) {
                    ByteBuffer view = m.payload().duplicate();
                    byte[] bytes = new byte[view.remaining()];
                    view.get(bytes);
                    if (Klv.isSt0601Family(bytes)) {
                        klvPayloads.add(bytes);
                    }
                }
            }
        }

        assertFalse(klvPayloads.isEmpty(),
            "no ST 0601 KLV Metadata event found in " + inputPath);

        byte[] payload = klvPayloads.get(0);

        // Parity assertion 1: isSt0601Family correctly identifies the UL.
        // This is the UL-prefix check (first 13 bytes + byte 15 == 0x00); it does
        // not require a valid body. All three bindings agree: this IS an ST 0601 UL.
        assertTrue(Klv.isSt0601Family(payload),
            "isSt0601Family should return true for the shared golden's KLV payload UL");

        // Parity assertion 2: decodeUasDatalink throws TRUNCATED_SET for this
        // empty-body payload, exactly as Rust's tst_core::klv::st0601::decode and
        // Python's decode_uas_datalink do.
        // The payload is [16-byte ST 0601 UL][0x00 BER] = 17 bytes; the decoder
        // needs the Tag-1 checksum in the body (≥3 bytes) but finds 0.
        org.tstrans.KlvDecodeException ex = assertThrows(
            org.tstrans.KlvDecodeException.class,
            () -> Klv.decodeUasDatalink(payload),
            "decodeUasDatalink should throw TRUNCATED_SET for the empty-body minimal LS");
        assertEquals(org.tstrans.KlvDecodeException.Kind.TRUNCATED_SET, ex.kind(),
            "exception kind must be TRUNCATED_SET (mirrors Rust KlvDecodeError::Truncated "
                + "and Python KlvError(TRUNCATED_SET) for the same empty-body input)");
    }

    /**
     * Typed-structure cross-binding parity for AUDIO: feed the {@code aac-audio-only}
     * shared scenario and assert the first {@code DemuxEvent.Audio}'s typed payload
     * agrees structurally with Rust/Python — codec {@code AAC}, first frame an
     * {@link AdtsFrame} with {@code sampleRateHz == 44100} and
     * {@code channelConfiguration == 2} (stereo). The companion to the H.264 video
     * proof; the audio split is covered here too.
     */
    @Test
    void reproducesAacAudioOnlyTypedPayload() throws Exception {
        Path dir = Path.of(
                System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios", "aac-audio-only")
            .normalize();
        Path inputPath = dir.resolve("input.ts");
        assertTrue(Files.isRegularFile(inputPath),
            "shared scenario input missing (expected committed fixture): " + inputPath);

        byte[] tsBytes = Files.readAllBytes(inputPath);

        DemuxEvent.Audio firstAudio = null;
        try (Demuxer d = new Demuxer()) {
            d.feed(tsBytes);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Audio a) {
                    firstAudio = a;
                    break;
                }
            }
        }

        assertNotNull(firstAudio, "no DemuxEvent.Audio found in " + inputPath);
        assertEquals(AudioCodec.AAC, firstAudio.codec(),
            "the aac-audio-only scenario's audio stream must be tagged AAC");
        // Opt-in parse() yields the typed frames; the clean ADTS payload parses
        // without throwing (lenient mode).
        List<AudioFrame> frames = firstAudio.parse();
        assertNotNull(frames, "AAC Audio.parse() must return a typed List<AudioFrame>, not null");
        assertFalse(frames.isEmpty(), "AAC Audio.parse() must be non-empty");
        // Downcast via instanceof (no switch-on-sealed; JDK 17).
        AudioFrame first = frames.get(0);
        assertTrue(first instanceof AdtsFrame,
            "AAC AudioFrame must be an AdtsFrame, was " + first.getClass().getSimpleName());
        AdtsFrame adts = (AdtsFrame) first;
        assertEquals(44100L, adts.sampleRateHz(),
            "first ADTS frame sample rate must be 44100 Hz");
        assertEquals(2, adts.channelConfiguration(),
            "first ADTS frame channel_configuration must be 2 (stereo)");
    }

    /**
     * Derive the MISB KLV set identity from the first 13 bytes of the raw payload.
     * Returns {@code "st0601"} when they match the ST 0601 UAS Datalink LS UL
     * prefix, else {@code "unknown"}. Reads via {@code duplicate()} so the source
     * buffer's position is undisturbed. Mirrors {@code _klv_set_from_ul} in the
     * Python adapter and {@code klv_set_from_ul()} in the Rust normaliser.
     */
    private static String klvSetFromUl(ByteBuffer payload) {
        ByteBuffer view = payload.duplicate();
        if (view.remaining() < ST0601_UL_PREFIX.length) {
            return "unknown";
        }
        byte[] head = new byte[ST0601_UL_PREFIX.length];
        view.get(head);
        return java.util.Arrays.equals(head, ST0601_UL_PREFIX) ? "st0601" : "unknown";
    }

    private record VideoSample(int pid, long pts, String payloadSha256) {}

    private record GoldenVideo(int pid, long pts, String payloadSha256) {}

    private record MetadataSample(int pid, String set) {}

    private record GoldenKlv(int pid, String set) {}

    /**
     * Minimal extraction of the single {@code "event":"video"} core object's
     * {@code pid}, {@code pts}, and {@code payload_sha256} fields from the golden
     * JSON text. Deliberately not a general JSON parser — it locates the video
     * object by its {@code "event": "video"} marker and reads the three scalar
     * fields that follow within that object. Fails loudly if any is absent.
     */
    private static GoldenVideo extractVideoEvent(String json) {
        int videoMarker = json.indexOf("\"video\"");
        assertTrue(videoMarker >= 0, "golden has no \"video\" core event: " + json);
        // The object containing "video" starts at the preceding '{'.
        int objStart = json.lastIndexOf('{', videoMarker);
        int objEnd = json.indexOf('}', videoMarker);
        assertTrue(objStart >= 0 && objEnd > objStart,
            "could not bound the video core object in golden");
        String obj = json.substring(objStart, objEnd + 1);

        int pid = (int) extractNumber(obj, "pid");
        long pts = extractNumber(obj, "pts");
        String sha = extractString(obj, "payload_sha256");
        return new GoldenVideo(pid, pts, sha);
    }

    /**
     * Minimal extraction of the single {@code "event":"klv"} core object's
     * {@code pid} and {@code set} fields from the golden JSON text. Mirrors
     * {@link #extractVideoEvent}: it locates the klv object by its {@code "klv"}
     * marker, bounds the enclosing {@code {...}}, and reads the two scalar fields.
     * Fails loudly if any is absent.
     */
    private static GoldenKlv extractKlvEvent(String json) {
        int klvMarker = json.indexOf("\"klv\"");
        assertTrue(klvMarker >= 0, "golden has no \"klv\" core event: " + json);
        int objStart = json.lastIndexOf('{', klvMarker);
        int objEnd = json.indexOf('}', klvMarker);
        assertTrue(objStart >= 0 && objEnd > objStart,
            "could not bound the klv core object in golden");
        String obj = json.substring(objStart, objEnd + 1);

        int pid = (int) extractNumber(obj, "pid");
        String set = extractString(obj, "set");
        return new GoldenKlv(pid, set);
    }

    /** Read an integer-valued JSON field {@code "key": <number>} from {@code obj}. */
    private static long extractNumber(String obj, String key) {
        String needle = "\"" + key + "\"";
        int k = obj.indexOf(needle);
        assertTrue(k >= 0, "golden object missing field \"" + key + "\": " + obj);
        int colon = obj.indexOf(':', k + needle.length());
        assertTrue(colon >= 0, "malformed golden field \"" + key + "\"");
        int i = colon + 1;
        while (i < obj.length() && (obj.charAt(i) == ' ' || obj.charAt(i) == '\t')) {
            i++;
        }
        int start = i;
        while (i < obj.length() && (Character.isDigit(obj.charAt(i)) || obj.charAt(i) == '-')) {
            i++;
        }
        assertTrue(i > start, "golden field \"" + key + "\" is not a number");
        return Long.parseLong(obj.substring(start, i));
    }

    /** Read a string-valued JSON field {@code "key": "<value>"} from {@code obj}. */
    private static String extractString(String obj, String key) {
        String needle = "\"" + key + "\"";
        int k = obj.indexOf(needle);
        assertTrue(k >= 0, "golden object missing field \"" + key + "\": " + obj);
        int firstQuote = obj.indexOf('"', obj.indexOf(':', k + needle.length()) + 1);
        assertTrue(firstQuote >= 0, "malformed golden field \"" + key + "\"");
        int lastQuote = obj.indexOf('"', firstQuote + 1);
        assertTrue(lastQuote > firstQuote, "unterminated golden string field \"" + key + "\"");
        return obj.substring(firstQuote + 1, lastQuote);
    }
}
