package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTimeoutPreemptively;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.rtp.RtspClientConfig;
import org.tstrans.rtp.RtspServerConfig;
import org.tstrans.shells.Receiver;
import org.tstrans.webrtc.WhepServerConfig;

/**
 * Secrets in URL text never reach {@code toString()} or an exception message. {@code Redact.url}
 * and {@code tst_core::url::redact_url} are tested against one shared vectors file, so the two
 * implementations cannot drift apart silently.
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
        // Accepted over-redaction: a query value holding an @ is read as userinfo.
        assertEquals("srt://h:<redacted>@b", Redact.url("srt://h:1?mail=a@b"));
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
        // Accepted over-redaction: a later quoted word joins the span.
        assertEquals("(see 'srt://h?token=<redacted>')",
            Redact.url("(see 'srt://h?token=abc', 'x')"));
    }

    @Test
    void quoteInsideASecretDoesNotOpenAUrl() {
        String out = Redact.url("'srt://127.0.0.1:9000?passphrase=abc'def'srt://sentinel'");
        for (String part : new String[] {"abc", "def", "sentinel"}) {
            assertFalse(out.contains(part), out);
        }
    }

    @Test
    void adjacentQuotedUrlsAreBothSeen() {
        assertEquals("'srt://a:1?passphrase=<redacted>' 'srt://b:2?passphrase=<redacted>'",
            Redact.url("'srt://a:1?passphrase=xxxxxxxxxx' 'srt://b:2?passphrase=yyyyyyyyyy'"));
    }

    /** The vectors {@code tst_core::url::redact_url}'s tests read too. */
    @Test
    void sharedVectorsMatchTheRustHelper() throws IOException {
        Path file = Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-core/tests/fixtures/redact/vectors.tsv").normalize();
        List<String> lines = Files.readAllLines(file, StandardCharsets.UTF_8);
        int rows = 0;
        for (int n = 0; n < lines.size(); n++) {
            String line = lines.get(n);
            if (line.endsWith("\r")) {
                line = line.substring(0, line.length() - 1);
            }
            if (line.isEmpty() || line.startsWith("#")) {
                continue;
            }
            int tab = line.indexOf('\t');
            assertTrue(tab >= 0, "vectors.tsv line " + (n + 1) + ": input<TAB>expected");
            assertEquals(line.substring(tab + 1), Redact.url(line.substring(0, tab)),
                "vectors.tsv line " + (n + 1));
            rows++;
        }
        assertTrue(rows > 0, "vectors.tsv holds no rows");
    }

    /** A long run of {@code %}, or of {@code a%} pairs, made the key scan reread the rest of the
     *  run at every position: quadratic in the run's length. */
    @Test
    void longPercentRunsRedactInLinearTime() {
        String percents = "srt://127.0.0.1:9000/" + "%".repeat(200_000);
        String pairs = "srt://127.0.0.1:9000?" + "a%".repeat(100_000);
        // Linear: milliseconds. Quadratic: tens of seconds.
        assertTimeoutPreemptively(Duration.ofSeconds(5), () -> {
            assertEquals(percents, Redact.url(percents));
            assertEquals(pairs, Redact.url(pairs));
        });
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
