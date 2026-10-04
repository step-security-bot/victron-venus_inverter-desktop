import java.util.Properties
import groovy.json.JsonSlurper

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Tauri rewrites tauri.properties during each build. Read the immutable release
// plan separately so candidate names and reserved native counters survive that step.
val releasePlanFile = file("../../../../.release-plan.json")
val releasePlan = if (releasePlanFile.exists()) {
    JsonSlurper().parse(releasePlanFile) as Map<*, *>
} else null

android {
    compileSdk = 36
    namespace = "com.alvit.inverter_dashboard"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.alvit.inverter_dashboard"
        minSdk = 24
        targetSdk = 36
        versionCode = releasePlan?.get("build_number")?.let { (it as Number).toInt() }
            ?: tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = releasePlan?.get("version") as String?
            ?: tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

val webkitVersion = "1.14.0"
val appcompatVersion = "1.7.1"
val activityKtxVersion = "1.10.1"
val materialVersion = "1.12.0"
val lifecycleProcessVersion = "2.10.0"
val junitVersion = "4.13.2"
val testExtJunitVersion = "1.1.4"
val testEspressoVersion = "3.5.0"

dependencies {
    implementation("androidx.webkit:webkit:$webkitVersion")
    implementation("androidx.appcompat:appcompat:$appcompatVersion")
    implementation("androidx.activity:activity-ktx:$activityKtxVersion")
    implementation("com.google.android.material:material:$materialVersion")
    implementation("androidx.lifecycle:lifecycle-process:$lifecycleProcessVersion")
    testImplementation("junit:junit:$junitVersion")
    androidTestImplementation("androidx.test.ext:junit:$testExtJunitVersion")
    androidTestImplementation("androidx.test.espresso:espresso-core:$testEspressoVersion")
}

apply(from = "tauri.build.gradle.kts")
apply(from = "release-packages.gradle.kts")
