// ServeWhep: play an MPEG-TS file in a browser over WebRTC (WHEP).
//
// Usage (from bindings/jvm):
//   ./gradlew runServeWhep --args='input.ts'
//   ./gradlew runServeWhep --args='input.ts whep://0.0.0.0:8080?advertise=192.0.2.10'
//
// A WHEP player then POSTs its SDP offer to http://127.0.0.1:8080/live and
// plays the file's H.264 video (set the mount's video codec for H.265), with
// each KLV record frame-locked to its access unit (in an SEI message and on
// the "klv" data channel).
//
// The shape: one WhepServer, one MP2T mount, and a loop that feeds the file's
// bytes to the mount at the speed its timestamps say. The mount does the
// demultiplexing, so this program never parses more of the TS than the PTS it
// paces on.
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import org.tstrans.WebrtcException;
import org.tstrans.webrtc.MountConfig;
import org.tstrans.webrtc.MountHandle;
import org.tstrans.webrtc.MountKind;
import org.tstrans.webrtc.MountStats;
import org.tstrans.webrtc.ServerStats;
import org.tstrans.webrtc.WhepServer;

public final class ServeWhep {
    private static final int TS_PACKET = 188;

    // PTS is a 33-bit count of 90 kHz ticks: 90 ticks per millisecond.
    private static final long PTS_MODULUS = 1L << 33;
    private static final long TICKS_PER_MS = 90;

    // Two PTS steps are discontinuities (a spliced or looped file), not real
    // time, and make the pacer re-anchor: forward by more than 5 s, which it
    // must not sleep through, and backward by more than 1 s. A backward step
    // up to 1 s is B-frame reordering (a few frames on the video PID) and does
    // not move the pacer. The mount's RTP clock uses wider bounds (10 s back,
    // 1 h ahead) because it only has to keep RTP timestamps moving forward; a
    // pacer that let a 3 s file loop back 3 s would push the second pass at
    // disk speed.
    private static final long MAX_FORWARD_TICKS = 5 * 90_000;
    private static final long MAX_REORDER_TICKS = 90_000;

    // Set by the Ctrl-C hook; the main loop checks it between packets.
    private static volatile boolean running = true;
    // Set before System.exit on an error, so the hook does not wait for main.
    private static volatile boolean exiting = false;

    public static void main(String[] args) throws IOException, InterruptedException {
        if (args.length < 1 || args.length > 2) {
            System.err.println("usage: ServeWhep <input.ts> [whep://ip:port[?key=value...]]");
            System.exit(2);
        }
        Path input = Path.of(args[0]);
        // The URL carries every server setting. 127.0.0.1 keeps the example on
        // this machine. To serve other machines, bind 0.0.0.0 and add
        // advertise=<the address viewers reach>, or the answer offers them only
        // loopback. A page on another origin also needs cors_origin=<origin>.
        String url = args.length == 2 ? args[1] : "whep://127.0.0.1:8080";

        // Ctrl-C runs shutdown hooks, not finally blocks: the hook asks the main
        // loop to stop and waits for it, so the try-with-resources below still
        // closes the server (which ends every viewer's session cleanly).
        Thread mainThread = Thread.currentThread();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            running = false;
            if (exiting) {
                return; // main itself called System.exit and will not return
            }
            try {
                mainThread.join(5_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }));

        // try-with-resources closes the mount handle, then the server: close()
        // stops the server's two threads and frees its native memory. Closing
        // the handle alone would leave the mount serving.
        try (WhepServer server = WhepServer.bind(url);
             MountHandle mount = server.addMount("/live", mp2tMount());
             InputStream in = Files.newInputStream(input)) {
            System.out.println("serving " + input + " at http://" + server.localAddr()
                + "/live (media on udp " + server.mediaAddr() + ")");
            serve(in, server, mount);
            // The mount's demuxer emits an access unit when the next one
            // starts, so it runs one frame behind the bytes pushed and the
            // file's last access unit is never played: a 150-frame file
            // reports frames=149.
            printStats(server, mount);
            System.out.println(running ? "end of file" : "interrupted");
        } catch (WebrtcException e) {
            // URL for a bad bind URL, IO for a port in use, MOUNT for a refused
            // mount, PUSH or CLOSED from a push.
            System.err.println("webrtc error (" + e.kind() + "): " + e.getMessage());
            exiting = true;
            System.exit(1);
        }
    }

    // MountConfig.defaults() is an elementary mount (pushVideo / pushKlv);
    // this program has muxed TS, so it keeps the other defaults and swaps the
    // kind. The video codec must match the file's: H.264 here.
    private static MountConfig mp2tMount() {
        MountConfig d = MountConfig.defaults();
        return new MountConfig(MountKind.MP2T, d.video(), d.audio(), d.klv(),
            d.seiBudgetBytes(), d.viewerQueuePackets());
    }

    // Reads the file in whole TS packets and pushes them so that a packet
    // carrying PTS p goes out (p - firstPts) / 90 ms after the first one.
    //
    // Why pace at all: the mount sends each access unit to the viewers as soon
    // as it is complete, and a browser plays frames as they arrive. Pushing the
    // file at disk speed would send minutes of video in a second; the viewers'
    // queues would overflow (counted in viewerPacketsDroppedQueueFull) and the
    // browser would show a burst, then nothing.
    //
    // Why PTS rather than PCR: PTS is the clock the mount and the browser use
    // for the frames themselves, and it is in every PES header, so a file
    // without PCRs still paces. Only the first video PID's PTS is used: KLV
    // and audio PES run on their own PIDs, and a stream that runs behind the
    // video would otherwise look like a jump back.
    private static void serve(InputStream in, WhepServer server, MountHandle mount)
            throws IOException, InterruptedException, WebrtcException {
        // Push in batches: everything read since the last PTS step goes in one
        // pushTs call. The mount accepts any chunking, so a batch boundary in
        // the middle of a PES packet is fine.
        byte[] chunk = new byte[TS_PACKET * 64];
        byte[] batch = new byte[TS_PACKET * 4096];
        int batchLen = 0;

        long startNanos = System.nanoTime();
        long lastStats = startNanos;
        int pacingPid = -1;    // the video PID paced on, once seen
        long lastPts = -1;     // the 33-bit PTS last paced on
        long mediaTicks = 0;   // ticks elapsed since the first PTS, unwrapped
        long anchorTicks = 0;  // mediaTicks at the last re-anchor
        long anchorNanos = startNanos;

        int n;
        while (running && (n = in.readNBytes(chunk, 0, chunk.length)) > 0) {
            // A trailing partial packet is not TS; leave it out.
            n -= n % TS_PACKET;
            for (int off = 0; off < n && running; off += TS_PACKET) {
                long pts = videoPts(chunk, off);
                int pid = ((chunk[off + 1] & 0x1F) << 8) | (chunk[off + 2] & 0xFF);
                if (pts >= 0 && pacingPid < 0) {
                    pacingPid = pid;
                    lastPts = pts;
                }
                if (pts >= 0 && pid == pacingPid && pts != lastPts) {
                    // Unwrap the 33-bit clock: a step below 2^32 is forward
                    // (it may cross the wrap), a larger one is backward.
                    long step = Math.floorMod(pts - lastPts, PTS_MODULUS);
                    boolean forward = step < PTS_MODULUS / 2;
                    long back = PTS_MODULUS - step;
                    if (forward || back > MAX_REORDER_TICKS) {
                        lastPts = pts;
                        // Everything before this packet is due by now.
                        if (batchLen > 0) {
                            mount.pushTs(Arrays.copyOf(batch, batchLen));
                            batchLen = 0;
                        }
                        if (!forward || step > MAX_FORWARD_TICKS) {
                            // A discontinuity: restart the timeline here.
                            anchorTicks = mediaTicks;
                            anchorNanos = System.nanoTime();
                        } else {
                            // Wait for this packet's time.
                            mediaTicks += step;
                            long dueNanos = anchorNanos
                                + (mediaTicks - anchorTicks) * 1_000_000 / TICKS_PER_MS;
                            long waitNanos = dueNanos - System.nanoTime();
                            if (waitNanos > 0) {
                                Thread.sleep(waitNanos / 1_000_000,
                                    (int) (waitNanos % 1_000_000));
                            }
                        }
                    }
                    // else: a reordered B-frame PTS; the pacer stays put.
                }
                if (batchLen == batch.length) {
                    mount.pushTs(Arrays.copyOf(batch, batchLen));
                    batchLen = 0;
                }
                System.arraycopy(chunk, off, batch, batchLen, TS_PACKET);
                batchLen += TS_PACKET;

                long now = System.nanoTime();
                if (now - lastStats >= 1_000_000_000L) {
                    lastStats = now;
                    printStats(server, mount);
                }
            }
        }
        if (batchLen > 0) {
            mount.pushTs(Arrays.copyOf(batch, batchLen));
        }
    }

    // The PTS of the video PES packet (stream_id 0xE0 to 0xEF) that starts in
    // the TS packet at buf[off], or -1 when none starts there. Only the fields
    // pacing needs are read: the TS header, the adaptation field's length, and
    // the PES header's stream_id and PTS.
    private static long videoPts(byte[] buf, int off) {
        if (buf[off] != 0x47) {
            return -1; // lost sync; the mount's demuxer resynchronises itself
        }
        boolean payloadStart = (buf[off + 1] & 0x40) != 0;
        int adaptation = (buf[off + 3] >> 4) & 0x3;
        if (!payloadStart || (adaptation & 0x1) == 0) {
            return -1; // no PES header begins in this packet
        }
        int p = off + 4;
        if ((adaptation & 0x2) != 0) {
            p += 1 + (buf[p] & 0xFF); // skip the adaptation field
        }
        int end = off + TS_PACKET;
        // PES header: 00 00 01, stream_id, length (2), flags (2), header
        // length (1), then the 5-byte PTS when PTS_DTS_flags has its top bit.
        if (p + 14 > end || buf[p] != 0 || buf[p + 1] != 0 || buf[p + 2] != 1) {
            return -1; // a PSI section (PAT, PMT) or a header that is cut short
        }
        if ((buf[p + 3] & 0xF0) != 0xE0 || (buf[p + 7] & 0x80) == 0) {
            return -1; // not video, or no PTS
        }
        int t = p + 9;
        // PTS[32..30] in bits 3..1 of the first byte, then two 15-bit halves,
        // each followed by a marker bit.
        return ((long) (buf[t] & 0x0E) << 29)
            | ((long) (buf[t + 1] & 0xFF) << 22)
            | ((long) (buf[t + 2] & 0xFE) << 14)
            | ((long) (buf[t + 3] & 0xFF) << 7)
            | ((long) (buf[t + 4] & 0xFF) >> 1);
    }

    // One line per second. server.stats() asks the media thread and can wait
    // up to 2 s for its reply; calling it inline from the pushing loop is
    // acceptable here because it runs once a second and the wait is bounded.
    // mount.stats() never waits. The counters are unsigned 64-bit values
    // carried in a long.
    private static void printStats(WhepServer server, MountHandle mount) {
        ServerStats s = server.stats();
        MountStats m = mount.stats();
        System.out.printf(
            "viewers=%d sessions=%d frames=%d keyframes=%d klv=%d klv_sei=%d klv_dc=%d"
                + " rtp_packets=%d queue_drops=%d nonconformant=%d%n",
            m.viewers(), s.sessionsTotal(), m.framesPushed(), m.keyframesPushed(),
            m.klvRecordsPushed(), m.klvSeiInserted(), m.klvDcMessagesSent(),
            s.rtpPacketsSent(), m.viewerPacketsDroppedQueueFull(), m.demuxNonconformant());
    }
}
