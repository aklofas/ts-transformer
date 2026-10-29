package org.tstrans.mpegts;

import java.io.BufferedOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import org.tstrans.MuxException;

/**
 * A context-managed sink that drains a {@link Muxer}'s output to a file. Mirrors
 * tst-py's {@code MuxerFileSink}: after each {@code push*} call (and on
 * {@link #close()}) it pulls pending TS packets and writes them to the file's
 * buffered output stream. As with tst-py's buffered file handle, bytes reach the
 * underlying file when the buffer fills or on {@link #close()} — not necessarily
 * synchronously per push. Construct via {@link Muxer#writeFile}.
 *
 * <p><b>Not thread-safe:</b> use a sink from one thread; its closed state is a
 * plain field.
 *
 * <p>The {@link Muxer} is <b>borrowed, not owned</b> — it remains usable after the
 * try-with-resources block (including for further {@code writeFile} calls).
 *
 * <p>Unlike the bare {@link Muxer} push methods, the sink's {@code push*} methods
 * also declare {@link java.io.IOException} because each drains pending packets to
 * the file's output stream.
 *
 * <p><b>Atomic mode.</b> tst-py infers success from Python's
 * {@code __exit__(exc_type)}; Java's {@link AutoCloseable#close()} has no exception
 * hook, so atomic mode uses an explicit {@link #commit()} marker: only a committed
 * sink promotes the {@code *.partial} temp to the destination on close; otherwise
 * the temp is discarded (nothing appears at the destination). Non-atomic mode
 * always preserves whatever was written (partial output on a body exception),
 * matching tst-py's non-atomic contract.
 */
public final class MuxerFileSink implements AutoCloseable {
    private static final int DRAIN_CHUNK_PACKETS = 7;
    private static final int DRAIN_CHUNK_BYTES = DRAIN_CHUNK_PACKETS * 188; // 1316

    private final Muxer muxer; // borrowed
    private final Path dest;
    private final boolean atomic;
    private final Path tmpPath; // non-null iff atomic
    private final OutputStream out;
    private final byte[] buf = new byte[DRAIN_CHUNK_BYTES];
    private boolean committed = false;
    private boolean closed = false;

    MuxerFileSink(Muxer muxer, Path dest, boolean atomic) throws IOException {
        this.muxer = muxer;
        this.dest = dest;
        this.atomic = atomic;
        if (atomic) {
            Path dir = dest.toAbsolutePath().getParent();
            this.tmpPath = Files.createTempFile(dir, dest.getFileName().toString(), ".partial");
            OutputStream raw;
            try {
                raw = Files.newOutputStream(tmpPath);
            } catch (IOException e) {
                Files.deleteIfExists(tmpPath);
                throw e;
            }
            this.out = new BufferedOutputStream(raw);
        } else {
            this.tmpPath = null;
            this.out = new BufferedOutputStream(Files.newOutputStream(dest));
        }
    }

    /**
     * Push one H.264/H.265/H.266 access unit or AV1 OBU stream. Drains pending
     * TS packets to the file after pushing.
     */
    public void pushVideo(byte[] nal, long pts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideo(nal, pts, keyFrame);
        drain();
    }

    /**
     * Push one already-carried on-wire video access unit (pass-through; see
     * {@link Muxer#pushVideoWire}). Drains pending TS packets to the file after
     * pushing.
     */
    public void pushVideoWire(byte[] wire, long pts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoWire(wire, pts, keyFrame);
        drain();
    }

    /**
     * Push one KLV local-set payload. Drains pending TS packets to the file after
     * pushing.
     */
    public void pushKlv(byte[] klv, long pts, int metadataServiceId) throws MuxException, IOException {
        ensureOpen();
        muxer.pushKlv(klv, pts, metadataServiceId);
        drain();
    }

    /**
     * Push one audio frame. Drains pending TS packets to the file after pushing.
     */
    public void pushAudio(byte[] frames, long pts) throws MuxException, IOException {
        ensureOpen();
        muxer.pushAudio(frames, pts);
        drain();
    }

    /**
     * Push one subtitle PES payload. Note the {@code (pts, payload)} argument order
     * (matches {@code Muxer#pushSubtitle}). Drains pending TS packets to the file
     * after pushing.
     */
    public void pushSubtitle(long pts, byte[] payload) throws MuxException, IOException {
        ensureOpen();
        muxer.pushSubtitle(pts, payload);
        drain();
    }

    /**
     * Push one private-data payload onto the lone configured data stream
     * (pass-through; see {@link Muxer#pushData}). Drains pending TS packets to
     * the file after pushing.
     */
    public void pushData(byte[] data, long pts) throws MuxException, IOException {
        ensureOpen();
        muxer.pushData(data, pts);
        drain();
    }

    /**
     * Push one private-data payload onto a specific data stream (pass-through;
     * see {@link Muxer#pushDataTo}). Drains pending TS packets to the file after
     * pushing.
     */
    public void pushDataTo(DataStreamHandle h, byte[] data, long pts)
            throws MuxException, IOException {
        ensureOpen();
        muxer.pushDataTo(h, data, pts);
        drain();
    }

    /** Push a video AU to a specific configured video stream (see {@link Muxer#pushVideoTo}). Drains after pushing. */
    public void pushVideoTo(VideoStreamHandle h, byte[] nal, long pts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoTo(h, nal, pts, keyFrame);
        drain();
    }

    /** Push an on-wire video AU to a specific video stream (see {@link Muxer#pushVideoWireTo}). Drains after pushing. */
    public void pushVideoWireTo(VideoStreamHandle h, byte[] wire, long pts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoWireTo(h, wire, pts, keyFrame);
        drain();
    }

    /** Push a KLV payload to a specific KLV stream (see {@link Muxer#pushKlvTo}). Drains after pushing. */
    public void pushKlvTo(KlvStreamHandle h, byte[] klv, long pts, int metadataServiceId) throws MuxException, IOException {
        ensureOpen();
        muxer.pushKlvTo(h, klv, pts, metadataServiceId);
        drain();
    }

    /** Push an audio frame to a specific audio stream (see {@link Muxer#pushAudioTo}). Drains after pushing. */
    public void pushAudioTo(AudioStreamHandle h, byte[] frames, long pts) throws MuxException, IOException {
        ensureOpen();
        muxer.pushAudioTo(h, frames, pts);
        drain();
    }

    /** Push a subtitle PES to a specific subtitle stream; note the {@code (pts, payload)} order (see {@link Muxer#pushSubtitleTo}). Drains after pushing. */
    public void pushSubtitleTo(SubtitleStreamHandle h, long pts, byte[] payload) throws MuxException, IOException {
        ensureOpen();
        muxer.pushSubtitleTo(h, pts, payload);
        drain();
    }

    /** Push a video AU with explicit decode timestamp to a specific video stream (see {@link Muxer#pushVideoToWithDts}). Drains after pushing. */
    public void pushVideoToWithDts(VideoStreamHandle h, byte[] nal, long pts, long dts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoToWithDts(h, nal, pts, dts, keyFrame);
        drain();
    }

    /** Push an on-wire video AU with explicit decode timestamp to a specific video stream (see {@link Muxer#pushVideoWireToWithDts}). Drains after pushing. */
    public void pushVideoWireToWithDts(VideoStreamHandle h, byte[] wire, long pts, long dts, boolean keyFrame) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoWireToWithDts(h, wire, pts, dts, keyFrame);
        drain();
    }

    /** Push a video AU with MISP SEI splice to a specific video stream (see {@link Muxer#pushVideoMispTo}). Drains after pushing. */
    public void pushVideoMispTo(VideoStreamHandle h, byte[] nal, long pts, boolean keyFrame,
            org.tstrans.codec.MispTimestamp misp) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoMispTo(h, nal, pts, keyFrame, misp);
        drain();
    }

    /** Push a video AU with MISP SEI splice and explicit DTS to a specific video stream (see {@link Muxer#pushVideoMispTo(VideoStreamHandle, byte[], long, long, boolean, org.tstrans.codec.MispTimestamp)}). Drains after pushing. */
    public void pushVideoMispTo(VideoStreamHandle h, byte[] nal, long pts, long dts,
            boolean keyFrame, org.tstrans.codec.MispTimestamp misp) throws MuxException, IOException {
        ensureOpen();
        muxer.pushVideoMispTo(h, nal, pts, dts, keyFrame, misp);
        drain();
    }

    /**
     * Mark the write successful. In atomic mode, only a committed sink promotes the
     * {@code *.partial} temp to the destination on {@link #close()}; without it,
     * close discards the temp. A no-op in non-atomic mode.
     *
     * @throws IllegalStateException after {@link #close()}
     */
    public void commit() {
        ensureOpen();
        this.committed = true;
    }

    private void ensureOpen() {
        if (closed) {
            throw new IllegalStateException("MuxerFileSink is closed");
        }
    }

    private void drain() throws IOException {
        while (muxer.pendingPackets() > 0) {
            int n = muxer.pull(buf);
            if (n == 0) {
                break;
            }
            out.write(buf, 0, n);
        }
    }

    @Override
    public void close() throws IOException {
        if (closed) {
            return;
        }
        closed = true;
        boolean drainAndCloseOk = false;
        try {
            try {
                drain();
            } finally {
                out.close();
            }
            drainAndCloseOk = true;
        } finally {
            if (atomic && (!drainAndCloseOk || !committed)) {
                Files.deleteIfExists(tmpPath);
            }
        }
        if (atomic && committed) {
            // Promote the temp to the destination. Prefer an atomic rename so a
            // reader never observes a torn file; fall back to a plain replacing
            // move on filesystems that don't support atomic rename (some
            // Windows/network/virtual FS throw AtomicMoveNotSupportedException) so
            // commit() still yields a complete output file. If even the fallback
            // fails, drop the temp so a failed commit leaves no stray *.partial.
            try {
                Files.move(tmpPath, dest,
                    StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            } catch (AtomicMoveNotSupportedException e) {
                try {
                    Files.move(tmpPath, dest, StandardCopyOption.REPLACE_EXISTING);
                } catch (IOException e2) {
                    Files.deleteIfExists(tmpPath);
                    throw e2;
                }
            } catch (IOException e) {
                Files.deleteIfExists(tmpPath);
                throw e;
            }
        }
    }
}
