package org.tstrans.smoke;

import java.lang.module.ModuleDescriptor;
import java.util.Set;
import java.util.TreeSet;

/**
 * Consumer of the assembled jar from the module path. Each class literal below
 * is resolved through the module graph, so a package missing from
 * {@code module-info} fails to compile or link with the JVM's own access error.
 * The descriptor's export list is then compared with the packages touched here,
 * so a newly exported package must be added to this list.
 */
public final class Main {
    private static final Class<?>[] TOUCHED = {
        org.tstrans.Version.class,
        org.tstrans.codec.Rational.class,
        org.tstrans.hls.HlsMode.class,
        org.tstrans.io.Io.class,
        org.tstrans.klv.Klv.class,
        org.tstrans.mpegts.StrictMode.class,
        org.tstrans.pipeline.PairerMode.class,
        org.tstrans.rtp.TransportPref.class,
        org.tstrans.shells.EndReason.class,
        org.tstrans.srt.ReconnectMode.class,
        org.tstrans.webrtc.MountKind.class,
    };

    public static void main(String[] args) {
        // Loads the native library out of the jar, as any consumer's first call would.
        System.out.println("version=" + org.tstrans.Version.versionString());

        Set<String> touched = new TreeSet<>();
        for (Class<?> c : TOUCHED) {
            touched.add(c.getPackageName());
        }

        ModuleDescriptor d = Main.class.getModule().getLayer()
            .findModule("org.tstrans").orElseThrow().getDescriptor();
        Set<String> exported = new TreeSet<>();
        for (ModuleDescriptor.Exports e : d.exports()) {
            if (!e.isQualified()) {
                exported.add(e.source());
            }
        }
        if (!exported.equals(touched)) {
            System.err.println("module-path: exported " + exported + " but touched " + touched);
            System.exit(1);
        }
        System.out.println("module-path: ok (" + touched.size() + " packages)");
    }
}
