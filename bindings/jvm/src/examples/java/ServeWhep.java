// ServeWhep: play an MPEG-TS file in a browser over WebRTC (WHEP).
//
// Usage (from bindings/jvm):
//   ./gradlew runServeWhep --args='input.ts'
//   ./gradlew runServeWhep --args='input.ts whep://0.0.0.0:8080?advertise=192.0.2.10'
//
// A WHEP player then POSTs its SDP offer to http://127.0.0.1:8080/live and
// plays the file's H.264 or H.265 video, with each KLV record frame-locked to
// its access unit (in an SEI message and on the "klv" data channel).
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

    // A forward PTS step longer than this is a discontinuity (a spliced or
    // looped file), not five seconds of real time: the pacer re-anchors on it
    // instead of sleeping through it.
    private static final long MAX_STEP_TICKS = 5 * 90_000;

    // Set by the Ctrl-C hook; the main loop checks it between packets.
    private static volatile boolean running = true;

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
            printStats(server, mount);
            System.out.println(running ? "end of file" : "interrupted");
        } catch (WebrtcException e) {
            // URL for a bad bind URL, IO for a port in use, MOUNT for a refused
            // mount, PUSH or CLOSED from a push.
            System.err.println("webrtc error (" + e.kind() + "): " + e.getMessage());
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
    // without PCRs still paces.
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
        long lastPts = -1;     // the 33-bit PTS last paced on
        long mediaTicks = 0;   // ticks elapsed since the first PTS, unwrapped
        long anchorTicks = 0;  // mediaTicks at the last re-anchor
        long anchorNanos = startNanos;

        int n;
        while (running && (n = in.readNBytes(chunk, 0, chunk.length)) > 0) {
            // A trailing partial packet is not TS; leave it out.
            n -= n % TS_PACKET;
            for (int off = 0; off < n && running; off += TS_PACKET) {
                long pts = pesPts(chunk, off);
                if (pts >= 0) {
                    if (lastPts < 0) {
                        lastPts = pts;
                    }
                    // Unwrap the 33-bit clock: a step below 2^32 is forward
                    // (it may cross the wrap); a larger one is backward, the
                    // reordered PTS of a B-frame or a stream that runs behind
                    // another, and does not move the pacer.
                    long step = Math.floorMod(pts - lastPts, PTS_MODULUS);
                    if (step > 0 && step < PTS_MODULUS / 2) {
                        lastPts = pts;
                        if (step > MAX_STEP_TICKS) {
                            // A discontinuity: send what is due now and
                            // restart the timeline here.
                            anchorTicks = mediaTicks;
                            anchorNanos = System.nanoTime();
                        } else {
                            mediaTicks += step;
                        }
                        // Everything before this packet is due by now: push
                        // it, then wait for this packet's time.
                        if (batchLen > 0) {
                            mount.pushTs(Arrays.copyOf(batch, batchLen));
                            batchLen = 0;
                        }
                        long dueNanos = anchorNanos
                            + (mediaTicks - anchorTicks) * 1_000_000 / TICKS_PER_MS;
                        long waitNanos = dueNanos - System.nanoTime();
                        if (waitNanos > 0) {
                            Thread.sleep(waitNanos / 1_000_000, (int) (waitNanos % 1_000_000));
                        }
                    }
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

    // The PTS of the PES packet that starts in the TS packet at buf[off], or -1
    // when none starts there. Only the fields pacing needs are read: the TS
    // header, the adaptation field's length, and the PES header's PTS.
    private static long pesPts(byte[] buf, int off) {
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
        if ((buf[p + 7] & 0x80) == 0) {
            return -1;
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

    // One line per second. stats() is a snapshot and safe from any thread;
    // the counters are unsigned 64-bit values carried in a long.
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
