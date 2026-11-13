// An Android app with a database inside it (PLAN.md §M14.23): built in
// drivers/android's Gradle build (`./gradlew :notes:installDebug`);
// examples/android/run.sh runs it on an emulator.
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.github.davidsarosi92.nucleus.notes"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.davidsarosi92.nucleus.notes"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    implementation(project(":nucleus"))
}
