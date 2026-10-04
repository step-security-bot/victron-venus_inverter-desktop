// Tauri normally starts separate Gradle builds for APK and AAB. Opt in only
// when invoking `tauri android build --apk`: both packages then share the same
// four Rust tasks in one graph, without caching or skipping native compilation.
if (providers.gradleProperty("inverterAndroidBothPackages").orNull == "true") {
    afterEvaluate {
        val bundle = tasks.named("bundleUniversalRelease")
        tasks.named("assembleUniversalRelease").configure {
            dependsOn(bundle)
        }
    }
}
