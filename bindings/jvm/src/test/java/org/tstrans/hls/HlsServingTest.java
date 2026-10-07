package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;

import java.io.InputStream;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.time.Duration;
import java.util.Base64;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManagerFactory;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import org.tstrans.HlsException;

class HlsServingTest {
    static byte[] onePacket() { byte[] b = new byte[188]; b[0] = 0x47; return b; }

    static HlsPublisher vod(Path dir) throws HlsException {
        return HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString()).mode(HlsMode.VOD).build();
    }

    static String fixture(String name) throws Exception {
        Path p = Files.createTempFile("hls-" + name, ".pem");
        try (InputStream in = HlsServingTest.class.getResourceAsStream("/hls/" + name)) {
            assertNotNull(in, "missing test resource /hls/" + name);
            Files.copy(in, p, java.nio.file.StandardCopyOption.REPLACE_EXISTING);
        }
        p.toFile().deleteOnExit();
        return p.toString();
    }

    static HttpClient trusting(String certPath) throws Exception {
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        X509Certificate cert;
        try (InputStream in = Files.newInputStream(Path.of(certPath))) {
            cert = (X509Certificate) cf.generateCertificate(in);
        }
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        ks.setCertificateEntry("hls-test", cert);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, tmf.getTrustManagers(), null);
        return HttpClient.newBuilder().sslContext(ctx).connectTimeout(Duration.ofSeconds(10)).build();
    }

    static HttpResponse<String> get(HttpClient c, String url, String authHeader) throws Exception {
        HttpRequest.Builder r = HttpRequest.newBuilder(URI.create(url)).timeout(Duration.ofSeconds(10)).GET();
        if (authHeader != null) r.header("Authorization", authHeader);
        return c.send(r.build(), HttpResponse.BodyHandlers.ofString());
    }

    @Test
    void finishServingServesVodPlaylistAndSegment(@TempDir Path dir) throws Exception {
        HlsPublisher pub = vod(dir);
        pub.pushTs(onePacket());
        pub.cutSegment();
        try (HlsServerHandle h = pub.finishServing()) {
            assertTrue(h.localPort() > 0);
            assertTrue(h.localAddr().startsWith("127.0.0.1:"));
            assertThrows(IllegalStateException.class, pub::stats, "publisher consumed");
            HttpClient c = HttpClient.newHttpClient();
            HttpResponse<String> pl = get(c, "http://127.0.0.1:" + h.localPort() + "/playlist.m3u8", null);
            assertEquals(200, pl.statusCode());
            assertTrue(pl.body().contains("#EXT-X-ENDLIST"), pl.body());
            String seg = pl.body().lines().filter(l -> l.endsWith(".ts")).findFirst().orElseThrow();
            HttpResponse<String> sr = get(c, "http://127.0.0.1:" + h.localPort() + "/" + seg, null);
            assertEquals(200, sr.statusCode());
        }
    }

    @Test
    void finishThenFinishServingThrowsIllegalState(@TempDir Path dir) throws Exception {
        HlsPublisher pub = vod(dir);
        pub.finish();
        assertThrows(IllegalStateException.class, pub::finishServing);
        HlsPublisher pub2 = vod(dir.resolve("b"));
        try (HlsServerHandle h = pub2.finishServing()) {
            assertThrows(IllegalStateException.class, pub2::finish);
        }
    }

    @Test
    void httpsServingWithTls(@TempDir Path dir) throws Exception {
        String cert = fixture("cert.pem");
        String key = fixture("key.pem");
        HlsPublisher pub = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString())
            .mode(HlsMode.VOD).enableTls(cert, key).build();
        pub.pushTs(onePacket());
        pub.cutSegment();
        try (HlsServerHandle h = pub.finishServing()) {
            HttpResponse<String> pl = get(trusting(cert), "https://127.0.0.1:" + h.localPort() + "/playlist.m3u8", null);
            assertEquals(200, pl.statusCode());
            assertTrue(pl.body().contains("#EXT-X-ENDLIST"), pl.body());
        }
    }

    @Test
    void basicAuthRejectsThenAccepts(@TempDir Path dir) throws Exception {
        HlsPublisher pub = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString())
            .mode(HlsMode.VOD).basicAuth("viewer", "s3cret").build();
        pub.pushTs(onePacket());
        pub.cutSegment();
        try (HlsServerHandle h = pub.finishServing()) {
            HttpClient c = HttpClient.newHttpClient();
            String url = "http://127.0.0.1:" + h.localPort() + "/playlist.m3u8";
            assertEquals(401, get(c, url, null).statusCode());
            String auth = "Basic " + Base64.getEncoder().encodeToString("viewer:s3cret".getBytes());
            assertEquals(200, get(c, url, auth).statusCode());
        }
    }

    @Test
    void serverHandleShutdownIsIdempotent(@TempDir Path dir) throws Exception {
        HlsPublisher pub = vod(dir);
        pub.pushTs(onePacket());
        pub.cutSegment();
        HlsServerHandle h = pub.finishServing();
        int port = h.localPort();
        h.shutdown();
        h.close();
        h.shutdown();
        assertThrows(IllegalStateException.class, h::localPort);
        assertTrue(port > 0);
    }
}
