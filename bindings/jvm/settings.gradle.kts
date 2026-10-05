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
    }
}

rootProject.name = "tstrans-jvm"
