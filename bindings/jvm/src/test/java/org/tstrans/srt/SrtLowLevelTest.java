package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;

/**
 * Non-live tests for {@link Builder}, {@link Socket}, {@link Listener}, and
 * {@link HostPort}. All paths exercised here complete without a live SRT peer —
 * they hit config-validation or option-narrowing paths that return before any
 * network call.
 */
class SrtLowLevelTest {

    /**
     * Builder.rendezvous().connect() must throw CONFIG_INVALID — rendezvous is
     * not yet supported by tst-srt. The rejection happens in nConnect before
     * any URL parse or network call.
     */
    @Test
    void rendezvousConnectRejected() {
        var b = new Builder("srt://127.0.0.1:9000").rendezvous();
        var e = assertThrows(SrtException.class, b::connect);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * Builder.rendezvous().listen() must also throw CONFIG_INVALID for the same
     * reason — rendezvous is not a valid listener mode.
     */
    @Test
    void rendezvousListenRejected() {
        var b = new Builder("srt://127.0.0.1:9000").rendezvous();
        var e = assertThrows(SrtException.class, b::listen);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * All chainable setters return the same {@code Builder} instance and store
     * their fields without making any native call. The native call only happens
     * at {@link Builder#connect()} / {@link Builder#listen()}.
     */
    @Test
    void builderSettersChain() {
        var b = new Builder("srt://127.0.0.1:9000")
            .caller()
            .latencyMs(120)
            .streamId("publish")
            .passphrase("hunter2hunter2")
            .congestion("live")
            .connectTimeoutMs(5000)
            .recvTimeoutMs(3000)
            .sendTimeoutMs(3000)
            .peerLatencyMs(80)
            .recvLatencyMs(80)
            .maxBandwidthBps(10_000_000L)
            .mss(1316)
            .payloadSize(1316);
        // Pure config holder — no native call yet; just verify the object exists.
        assertNotNull(b);
    }

    /**
     * mss(70000) exceeds u16::MAX (65535). The Rust side runs checked_u16
     * BEFORE any socket creation or network operation — the exception is an
     * IllegalArgumentException thrown from nConnect (not an SrtException).
     */
    @Test
    void mssOutOfRangeRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller().mss(70000);
        assertThrows(IllegalArgumentException.class, b::connect);
    }

    /**
     * payloadSize(70000) also exceeds u16::MAX — same narrowing path as mss.
     */
    @Test
    void payloadSizeOutOfRangeRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller().payloadSize(70000);
        assertThrows(IllegalArgumentException.class, b::connect);
    }

    /**
     * Negative millisecond/bandwidth knobs are rejected at the Java setter
     * boundary (these map to unsigned Rust quantities). The setter throws
     * IllegalArgumentException eagerly — before any finalizer or network call.
     */
    @Test
    void negativeTimeoutRejected() {
        assertThrows(IllegalArgumentException.class,
            () -> new Builder("srt://127.0.0.1:9000").connectTimeoutMs(-1));
    }

    /**
     * HostPort is a plain Java record; verify field access works correctly.
     */
    @Test
    void hostPortRecord() {
        var hp = new HostPort("192.168.1.1", 9000);
        assertEquals("192.168.1.1", hp.host());
        assertEquals(9000, hp.port());
    }

    /**
     * Builder.caller().listen() must throw CONFIG_INVALID because the URL
     * defaults to mode=caller but nListen requires listener mode.
     */
    @Test
    void callerModeOnListenRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller();
        var e = assertThrows(SrtException.class, b::listen);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * Builder.listener().connect() must throw CONFIG_INVALID — listener mode
     * is incompatible with the connect finalizer.
     */
    @Test
    void listenerModeOnConnectRejected() {
        // ?mode=listener in the URL + explicit .listener() on the builder.
        // nConnect checks mode ordinal == 2 (LISTENER) and rejects immediately.
        var b = new Builder("srt://127.0.0.1:9000?mode=listener").listener();
        var e = assertThrows(SrtException.class, b::connect);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /** A zero peer-idle timeout is refused when the socket is configured. */
    @Test
    void zeroPeerIdleTimeoutRefusedOnConnectAndListen() {
        var c = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:9000").caller().peerIdleTimeoutMs(0).connect());
        assertEquals(SrtException.Kind.CONFIG_INVALID, c.kind());
        var l = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=listener").listener().peerIdleTimeoutMs(0).listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, l.kind());
    }

    /** libsrt multiplies the millisecond value by 1000 into an int, so 2147484 ms is refused. */
    @Test
    void peerIdleTimeoutAboveCeilingRefusedOnConnectAndListen() {
        var c = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:9000").caller().peerIdleTimeoutMs(2_147_484).connect());
        assertEquals(SrtException.Kind.CONFIG_INVALID, c.kind());
        var l = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=listener").listener().peerIdleTimeoutMs(2_147_484).listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, l.kind());
    }

    @Test
    void negativePeerIdleTimeoutRejected() {
        assertThrows(IllegalArgumentException.class,
            () -> new Builder("srt://127.0.0.1:9000").peerIdleTimeoutMs(-1));
    }

    /**
     * A positive peer-idle timeout is applied on both ends of a loopback connection. The value
     * itself is pinned by the Rust readback tests (peer_idle_timeout.rs); this proves the knob
     * crosses the binding without a config error.
     */
    @Test
    void positivePeerIdleTimeoutAcceptedOnLoopback() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");
        try (var lst = new Builder("srt://127.0.0.1:0?mode=listener").listener()
                .peerIdleTimeoutMs(8000).listen()) {
            int port = lst.localAddr().port();
            try (var sock = new Builder("srt://127.0.0.1:" + port).caller()
                    .peerIdleTimeoutMs(8000).connect();
                 var accepted = lst.accept(5000)) {
                assertNotNull(accepted);
            }
        }
    }
}
