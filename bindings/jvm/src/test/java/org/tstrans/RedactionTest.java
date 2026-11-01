package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import org.junit.jupiter.api.Test;
import org.tstrans.rtp.RtspClientConfig;
import org.tstrans.rtp.RtspServerConfig;
import org.tstrans.shells.Receiver;
import org.tstrans.webrtc.WhepServerConfig;

/**
 * Secrets in URL text never reach {@code toString()} or an exception message. The
 * {@code Redact.url} vectors are copied from {@code tst_core::url::redact_url}'s tests
 * so the two implementations cannot drift apart silently.
 */
class RedactionTest {
    private static final String SECRET = "hunter2";

    @Test
    void userinfoPasswordIsRedactedUserKept() {
        assertEquals("rtsp://cam:<redacted>@10.0.0.5:554/s",
            Redact.url("rtsp://cam:hunter2@10.0.0.5:554/s"));
    }

    @Test
    void userinfoWithoutPasswordIsUntouched() {
        assertEquals("rtsp://cam@h:554/s", Redact.url("rtsp://cam@h:554/s"));
        assertEquals("srt://h:9000?latency=120", Redact.url("srt://h:9000?latency=120"));
    }

    @Test
    void passwordContainingSlashAndAtIsRedacted() {
        assertEquals("rtsp://u:<redacted>@h/x", Redact.url("rtsp://u:pa/ss@word@h/x"));
    }

    @Test
    void eachSecretKeyIsRedacted() {
        for (String k : new String[] {"passphrase", "secret", "token", "auth_pass", "password"}) {
            assertEquals("srt://h:1?a=b&" + k + "=<redacted>&c=d",
                Redact.url("srt://h:1?a=b&" + k + "=s3cr3t&c=d"));
        }
    }

    @Test
    void firstQueryKeyAndOtherValuesVerbatim() {
        assertEquals("rist://h:1?secret=<redacted>&latency=50",
            Redact.url("rist://h:1?secret=abc&latency=50"));
    }

    @Test
    void keyMatchIsExactLowercase() {
        assertEquals("srt://h:1?Token=abc", Redact.url("srt://h:1?Token=abc"));
        assertEquals("srt://h:1?client_secret=abc", Redact.url("srt://h:1?client_secret=abc"));
    }

    @Test
    void nonUrlTextIsScrubbed() {
        assertEquals("passphrase=<redacted>", Redact.url("passphrase=abc"));
        assertEquals("u:<redacted>@h", Redact.url("u:pw@h"));
        assertEquals("bad url 'srt://h?passphrase=<redacted>' here",
            Redact.url("bad url 'srt://h?passphrase=abc' here"));
    }

    @Test
    void plainTextIsUnchanged() {
        assertEquals("hello world", Redact.url("hello world"));
        assertEquals("", Redact.url(""));
        assertEquals("user@host", Redact.url("user@host"));
    }

    @Test
    void rtspClientConfigToString() {
        String s = RtspClientConfig.of("rtsp://cam:" + SECRET + "@10.0.0.5:554/s?token=" + SECRET)
            .toString();
        assertFalse(s.contains(SECRET), s);
        assertTrue(s.contains("10.0.0.5:554"), s);
    }

    @Test
    void rtspServerConfigToString() {
        String s = RtspServerConfig.builder()
            .bindAddr("u:" + SECRET + "@0.0.0.0:8554")
            .build()
            .toString();
        assertFalse(s.contains(SECRET), s);
        assertTrue(s.contains("0.0.0.0:8554"), s);
    }

    @Test
    void whepServerConfigToString() {
        String s = new WhepServerConfig("whep://127.0.0.1:8889?token=" + SECRET).toString();
        assertFalse(s.contains(SECRET), s);
        assertTrue(s.contains("127.0.0.1:8889"), s);
    }

    @Test
    void shellAdapterExceptionMessagesRedact() throws Exception {
        String url = "udp://u:" + SECRET + "@h:1?passphrase=" + SECRET;
        for (String pkg : new String[] {"srt", "rtp"}) {
            Class<?> c = Class.forName("org.tstrans." + pkg + ".ShellAdapters");
            Method m = c.getDeclaredMethod(pkg, BindingException.class, String.class);
            m.setAccessible(true);
            Throwable t = assertThrows(InvocationTargetException.class, () ->
                m.invoke(null, new UdpException(UdpException.Kind.IO, "x"), url)).getCause();
            assertTrue(t instanceof IllegalStateException, String.valueOf(t));
            assertFalse(t.getMessage().contains(SECRET), t.getMessage());
            assertTrue(t.getMessage().contains("h:1"), t.getMessage());
        }
    }

    @Test
    void unsupportedSchemeMessageRedacts() {
        IllegalArgumentException e = assertThrows(IllegalArgumentException.class,
            () -> Receiver.open("ftp://u:" + SECRET + "@h:1/x?token=" + SECRET));
        assertFalse(e.getMessage().contains(SECRET), e.getMessage());
        assertTrue(e.getMessage().contains("h:1"), e.getMessage());
    }
}
