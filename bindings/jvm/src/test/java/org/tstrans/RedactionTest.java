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
    void nonAsciiTextIsKept() {
        assertEquals("srt://h/é?latency=1", Redact.url("srt://h/é?latency=1"));
        assertEquals("rtsp://u:<redacted>@h/é", Redact.url("rtsp://u:pä\u00df@h/é"));
        assertEquals("srt://h/é?token=<redacted>&x=é", Redact.url("srt://h/é?token=\u00e9\u00e9&x=é"));
    }

    @Test
    void noBreakSpaceSplitsTokens() {
        assertEquals("a\u00a0passphrase=<redacted>\u00a0b", Redact.url("a\u00a0passphrase=abc\u00a0b"));
        assertEquals("u:<redacted>@h\u00a0x", Redact.url("u:pw@h\u00a0x"));
    }

    @Test
    void streamIdKeysAreRedacted() {
        assertEquals("srt://h:1?streamid=<redacted>&latency=1",
            Redact.url("srt://h:1?streamid=#!::r=x&latency=1"));
        assertEquals("srt://h:1?srt_streamid=<redacted>", Redact.url("srt://h:1?srt_streamid=abc"));
    }

    @Test
    void rawQuestionMarkInPasswordIsRedacted() {
        assertEquals("rtsp://admin:<redacted>@cam/s", Redact.url("rtsp://admin:pa?ss@cam/s"));
        assertEquals("rtsp://admin:<redacted>@cam/s?token=<redacted>",
            Redact.url("rtsp://admin:pa?ss@cam/s?token=t"));
        assertEquals("srt://h:1?mail=a@b", Redact.url("srt://h:1?mail=a@b"));
    }

    @Test
    void lateAtOverRedactsAndLeadingQueryKeyIsScrubbed() {
        assertEquals("srt://h:<redacted>@b", Redact.url("srt://h:9000/a@b"));
        assertEquals("?token=<redacted>", Redact.url("?token=abc"));
    }

    @Test
    void severalUrlsInOneToken() {
        assertEquals("a,srt://u:<redacted>@h2", Redact.url("a,srt://u:p@h,srt://v:q@h2"));
    }

    @Test
    void percentEncodedKeysAreRedacted() {
        assertEquals("srt://h:1?pass%70hrase=<redacted>&x=1",
            Redact.url("srt://h:1?pass%70hrase=abc&x=1"));
        assertEquals("?%74oken=<redacted>", Redact.url("?%74oken=abc"));
        assertEquals("srt://h:1?pass%7=abc", Redact.url("srt://h:1?pass%7=abc"));
    }

    @Test
    void quotesAndSpacesInsideSecretsAreRedacted() {
        assertEquals("rtsp://cam:<redacted>@10.0.0.5:554/s",
            Redact.url("rtsp://cam:hun'ter2@10.0.0.5:554/s"));
        assertEquals("rtsp://cam:<redacted>@10.0.0.5:554/s",
            Redact.url("rtsp://cam:hun ter2@10.0.0.5:554/s"));
        assertEquals("srt://h:1?passphrase=<redacted>&x=1",
            Redact.url("srt://h:1?passphrase=prefix'hunter2&x=1"));
        assertEquals("srt://h:1?passphrase=<redacted>&x=1",
            Redact.url("srt://h:1?passphrase=pre fix&x=1"));
        assertEquals("bad \"srt://h?token=<redacted>\" here",
            Redact.url("bad \"srt://h?token=a b\" here"));
    }

    @Test
    void aQuoteInsideAQuotedUrlDoesNotCloseIt() {
        assertEquals("'rtsp://cam:<redacted>@h/'", Redact.url("'rtsp://cam:hun'ter2@h/'"));
        assertEquals("\"srt://h:1?passphrase=<redacted>\"",
            Redact.url("\"srt://h:1?passphrase=a\"b\""));
        assertEquals("(see 'srt://h?token=<redacted>', 'x')",
            Redact.url("(see 'srt://h?token=abc', 'x')"));
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
    void rtspServerConfigBuildErrorRedactsBindAddr() {
        IllegalArgumentException e = assertThrows(IllegalArgumentException.class, () ->
            RtspServerConfig.builder()
                .bindAddr("u:" + SECRET + "@0.0.0.0:8554")
                .tlsCert("c.pem")
                .tlsKey("k.pem")
                .build());
        assertFalse(e.getMessage().contains(SECRET), e.getMessage());
        assertTrue(e.getMessage().contains("0.0.0.0:8554"), e.getMessage());
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
