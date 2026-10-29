package org.tstrans.io;

import java.io.BufferedInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.EnumSet;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.Optional;
import java.util.Set;
import java.util.Spliterator;
import java.util.Spliterators;
import java.util.TreeSet;
import java.util.stream.Stream;
import java.util.stream.StreamSupport;
import org.tstrans.DemuxException;
import org.tstrans.KlvDecodeException;
import org.tstrans.mpegts.AudioCodec;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.SubtitleCodec;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.klv.Klv;
import org.tstrans.klv.KlvSet;

/**
 * Convenience helpers for reading {@code .ts} files. Mirrors tst-py's
 * {@code tstrans.io} module (read side). The write side lives on
 * {@link org.tstrans.mpegts.Muxer#writeFile} (mirrors tst-py's
 * {@code Muxer.write_file}).
 *
 * <p>All helpers are pure-Java orchestration over {@link Demuxer}; there is no
 * native code in this package.
 */
public final class Io {
    private Io() {}

    /** Chunk size for feeding the demuxer (tst-py {@code _FEED_CHUNK}). */
    private static final int FEED_CHUNK = 64 * 1024;

    /** Probe scan budget (tst-py {@code _PROBE_BYTES} = 5 MiB). */
    private static final int PROBE_BYTES = 5 * 1024 * 1024;

    /**
     * Open {@code path}, feed it to a {@link Demuxer} in 64 KiB chunks, and yield
     * each {@link DemuxEvent} lazily. Mirrors {@code tstrans.io.parse_file}.
     *
     * <p>The returned stream is {@link AutoCloseable} — use try-with-resources so
     * the backing demuxer + file handle are released. A demux error mid-stream
     * surfaces as a {@link RuntimeException} wrapping a {@link DemuxException}
     * (Java streams cannot throw checked exceptions during iteration — same idiom
     * as {@link Demuxer#iterator()}); an I/O read error surfaces as
     * {@link UncheckedIOException}. Truncation is clean EOF (no error).
     */
    public static Stream<DemuxEvent> parseFile(Path path) throws IOException {
        return parseFile(path, null);
    }

    /** Like {@link #parseFile(Path)} with a non-default {@link DemuxerConfig}. */
    public static Stream<DemuxEvent> parseFile(Path path, DemuxerConfig config) throws IOException {
        ParseFileIterator it = new ParseFileIterator(path, config);
        return StreamSupport.stream(
                Spliterators.spliteratorUnknownSize(
                    it, Spliterator.ORDERED | Spliterator.NONNULL),
                false)
            .onClose(it::close);
    }

    /**
     * Scan the first 5 MiB and summarize. Mirrors {@code tstrans.io.probe}.
     *
     * <p>Like {@link #parseFile(Path)}, a demux error from a malformed file
     * surfaces as a {@link RuntimeException} wrapping a {@link DemuxException} — a
     * caller catching only {@link IOException} will not see it.
     */
    public static ProbeResult probe(Path path) throws IOException {
        return probe(path, null);
    }

    /** Like {@link #probe(Path)} with a non-default {@link DemuxerConfig}. */
    public static ProbeResult probe(Path path, DemuxerConfig config) throws IOException {
        long size = Files.size(path);
        long readTotal = 0;

        List<DemuxEvent.ProgramMap> programs = new ArrayList<>();
        TreeSet<Integer> pids = new TreeSet<>();
        Set<VideoCodec> video = EnumSet.noneOf(VideoCodec.class);
        Set<AudioCodec> audio = EnumSet.noneOf(AudioCodec.class);
        Set<SubtitleCodec> subtitle = EnumSet.noneOf(SubtitleCodec.class);
        boolean hasKlv = false;

        try (Demuxer d = (config == null) ? new Demuxer() : new Demuxer(config);
             InputStream in = new BufferedInputStream(Files.newInputStream(path), FEED_CHUNK)) {
            byte[] buf = new byte[FEED_CHUNK];
            while (readTotal < PROBE_BYTES) {
                int n = in.read(buf);
                if (n < 0) break;
                if (n == 0) continue;
                try {
                    d.feed(Arrays.copyOf(buf, n));
                } catch (DemuxException e) {
                    throw new RuntimeException(e);
                }
                readTotal += n;
            }
            d.flush();
            try {
                DemuxEvent ev;
                while ((ev = d.nextEvent()) != null) {
                    if (ev instanceof DemuxEvent.ProgramMap pm) {
                        programs.add(pm);
                        pids.addAll(pm.elementaryPids());
                    } else if (ev instanceof DemuxEvent.Video v) {
                        video.add(v.codec());
                    } else if (ev instanceof DemuxEvent.Audio a) {
                        audio.add(a.codec());
                    } else if (ev instanceof DemuxEvent.Subtitle s) {
                        subtitle.add(s.codec());
                    } else if (ev instanceof DemuxEvent.Metadata) {
                        hasKlv = true;
                    }
                }
            } catch (DemuxException e) {
                throw new RuntimeException(e);
            }
        }

        return new ProbeResult(
            size,
            List.copyOf(programs),
            new ArrayList<>(pids),
            video.stream().sorted(Comparator.comparing(Enum::name)).toList(),
            audio.stream().sorted(Comparator.comparing(Enum::name)).toList(),
            subtitle.stream().sorted(Comparator.comparing(Enum::name)).toList(),
            hasKlv,
            readTotal / 188);
    }

    /**
     * Iterate KLV payloads in {@code path}. Mirrors {@code tstrans.io.extract_klv}.
     * Each yielded {@link KlvEntry} carries raw bytes or a typed {@link KlvSet} per
     * {@code opts}; see {@link ExtractKlvOptions} for the knobs.
     *
     * <p>With {@code parsed=true}, each payload runs through
     * {@link Klv#parseUniversal}. {@code skipUnknown} (default true) drops payloads
     * whose UL is unrecognized; {@code skipMalformed} (default false) controls a
     * {@link KlvDecodeException} from a recognized UL — propagated by default so
     * corruption is not silently lost, skipped when true. A propagated decode error
     * surfaces as a {@link RuntimeException} wrapping the {@link KlvDecodeException}
     * (Java streams cannot throw checked exceptions during iteration — same idiom as
     * {@link #parseFile}); catching the wrapped {@code KlvDecodeException}
     * specifically still lets binding-shape regressions surface naturally.
     */
    public static Stream<KlvEntry> extractKlv(Path path) throws IOException {
        return extractKlv(path, ExtractKlvOptions.defaults());
    }

    /** Like {@link #extractKlv(Path)} with explicit {@link ExtractKlvOptions}. */
    public static Stream<KlvEntry> extractKlv(Path path, ExtractKlvOptions opts) throws IOException {
        Stream<DemuxEvent> events = parseFile(path, opts.config());
        return events
            .filter(e -> e instanceof DemuxEvent.Metadata)
            .map(e -> (DemuxEvent.Metadata) e)
            .flatMap(m -> toEntry(m, opts).stream())
            .onClose(events::close);
    }

    /** Project one Metadata event to zero-or-one KlvEntry per the options. */
    private static Optional<KlvEntry> toEntry(DemuxEvent.Metadata m, ExtractKlvOptions opts) {
        Long pts = opts.withPts() ? Long.valueOf(m.pts()) : null;
        // Always copy the payload: needed as the raw entry (parsed=false) AND as parseUniversal's input (parsed=true).
        java.nio.ByteBuffer view = m.payload().duplicate();
        byte[] raw = new byte[view.remaining()];
        view.get(raw);

        if (!opts.parsed()) {
            return Optional.of(new KlvEntry(pts, raw, null));
        }
        KlvSet typed;
        try {
            Optional<KlvSet> parsed = Klv.parseUniversal(raw);
            if (parsed.isEmpty()) {
                if (opts.skipUnknown()) {
                    return Optional.empty();
                }
                return Optional.of(new KlvEntry(pts, null, null)); // unknown UL, surfaced
            }
            typed = parsed.get();
        } catch (KlvDecodeException ex) {
            if (opts.skipMalformed()) {
                return Optional.empty();
            }
            throw new RuntimeException(ex);
        }
        return Optional.of(new KlvEntry(pts, null, typed));
    }

    /**
     * Iterator that feeds a file to a {@link Demuxer} 64 KiB at a time, draining
     * events on demand. Mirrors the streaming, bounded-memory shape of
     * {@code tst_core::io_file::TryDemuxFromFile}.
     */
    private static final class ParseFileIterator implements Iterator<DemuxEvent>, AutoCloseable {
        private final InputStream in;
        private final Demuxer demuxer;
        private final ArrayDeque<DemuxEvent> pending = new ArrayDeque<>();
        private final byte[] buf = new byte[FEED_CHUNK];
        private boolean eof = false;
        private boolean flushed = false;

        ParseFileIterator(Path path, DemuxerConfig config) throws IOException {
            InputStream s = new BufferedInputStream(Files.newInputStream(path), FEED_CHUNK);
            try {
                this.demuxer = (config == null) ? new Demuxer() : new Demuxer(config);
            } catch (RuntimeException | Error e) {
                // An Error too: the first Demuxer loads the native library, and an
                // UnsatisfiedLinkError must not leave the input open.
                try { s.close(); } catch (IOException ignored) {
                    // best-effort close on failed construction
                }
                throw e;
            }
            this.in = s;
        }

        @Override
        public boolean hasNext() {
            while (pending.isEmpty()) {
                if (flushed) {
                    return false;
                }
                if (eof) {
                    demuxer.flush();
                    flushed = true; // flush was called — don't repeat it even if drain throws
                    drain();
                    continue;
                }
                int n;
                try {
                    n = in.read(buf);
                } catch (IOException e) {
                    throw new UncheckedIOException(e);
                }
                if (n < 0) {
                    eof = true;
                    continue;
                }
                if (n == 0) {
                    continue;
                }
                try {
                    demuxer.feed(Arrays.copyOf(buf, n));
                } catch (DemuxException e) {
                    throw new RuntimeException(e);
                }
                drain();
            }
            return true;
        }

        @Override
        public DemuxEvent next() {
            if (!hasNext()) {
                throw new NoSuchElementException();
            }
            return pending.poll();
        }

        private void drain() {
            try {
                DemuxEvent ev;
                while ((ev = demuxer.nextEvent()) != null) {
                    pending.add(ev);
                }
            } catch (DemuxException e) {
                throw new RuntimeException(e);
            }
        }

        @Override
        public void close() {
            demuxer.close();
            try {
                in.close();
            } catch (IOException ignored) {
                // best-effort close of the input stream
            }
        }
    }
}
