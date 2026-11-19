pluginManagement {
    repositories {
        // Plugin Portal first (canonical plugin source), Maven Central as a
        // fallback: a transient Portal outage once killed a CI leg at plugin
        // RESOLUTION ("Plugin ... was not found ... Searched
        // in: Gradle Central Plugin Repository") while the same commit
        // resolved fine on the other legs. The vanniktech plugin marker +
        // implementation are mirrored on Central (verified end-to-end:
        // Central-only resolution succeeds in a fresh GRADLE_USER_HOME),
        // so either repository alone can satisfy the build's plugins.
        gradlePluginPortal()
        mavenCentral()
        // Google's mirror of Maven Central, last: Maven Central has answered
        // "not found" for a file it holds, and on a "not found" Gradle tries
        // the next repository. (A 429, a 5xx or a reset instead disables
        // Maven Central for the rest of the build and fails it; the CI
        // resolve step, scripts/ci/gradle-resolve.sh, retries those.) Every
        // file is pinned by SHA-256 in gradle/verification-metadata.xml, so
        // the mirror is trusted for availability only.
        maven {
            name = "GoogleMavenCentralMirror"
            url = uri("https://maven-central.storage-download.googleapis.com/maven2/")
        }
    }
}

rootProject.name = "tstrans-jvm"
