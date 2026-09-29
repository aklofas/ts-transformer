package org.tstrans.srt;

import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Child-JVM entry point for {@link ExitWithParkedCallTest}: parks a daemon
 * thread inside a listener-mode open, proves the park, prints {@code PARKED}
 * and leaves — without closing anything, because the open has not returned
 * and there is nothing to close.
 *
 * <p>Arguments: {@code <shape> <how>} where {@code shape} is {@code demux},
 * {@code receiver} or {@code managed} and {@code how} is {@code exit}
 * ({@code System.exit(0)}) or {@code return} (fall off the end of
 * {@code main}).
 */
public final class ExitWithParkedCallChild {
    private ExitWithParkedCallChild() {}

    /** Port 0: the kernel picks one; no peer ever connects. */
    private static final String LISTEN_URL = "srt://:0?mode=listener";

    public static void main(String[] args) throws Exception {
        String shape = args[0];
        String how = args[1];

        // Set immediately before the native call and after it: "entered and
        // not returned" is what proves the thread is inside the call. Without
        // it an open that failed at once would pass with the guard removed.
        AtomicBoolean entered = new AtomicBoolean();
        AtomicBoolean returned = new AtomicBoolean();

        Thread parked = new Thread(() -> {
            entered.set(true);
            try {
                switch (shape) {
                    case "demux" -> DemuxReceiver.fromUrl(LISTEN_URL);
                    case "receiver" -> Receiver.fromUrl(LISTEN_URL);
                    case "managed" -> ManagedDemuxReceiver.fromUrl(LISTEN_URL);
                    default -> throw new IllegalArgumentException("unknown shape: " + shape);
                }
            } catch (Throwable t) {
                System.out.println("OPEN-ENDED: " + t);
            } finally {
                returned.set(true);
            }
        }, "parked-in-open");
        parked.setDaemon(true);
        parked.start();

        long deadline = System.nanoTime() + 10_000_000_000L;
        while (!entered.get()) {
            if (System.nanoTime() > deadline) {
                System.out.println("the worker never reached the native call");
                System.exit(3);
            }
            Thread.sleep(5);
        }
        // Bounded look, no wall-clock assertion: the call must still be
        // outstanding.
        for (int i = 0; i < 20 && !returned.get(); i++) {
            Thread.sleep(10);
        }
        if (returned.get()) {
            System.out.println("the native call returned; nothing is parked");
            System.exit(4);
        }
        System.out.println("PARKED");
        System.out.flush();

        if (how.equals("exit")) {
            System.exit(0);
        }
        // "return": fall off the end of main with the daemon thread parked.
    }
}
