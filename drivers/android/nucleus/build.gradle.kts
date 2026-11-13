// nucleus-android: a database inside an Android app (PLAN.md §M14.23) —
// the engine (libnucleus_android.so, from build-native.sh) and the Java
// driver (drivers/java, its sources built in) with a Kotlin entry.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.github.davidsarosi92.nucleus.android"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    sourceSets {
        getByName("main") {
            // The Java driver, as it is.
            java.srcDir("../../java/src/main/java")
            jniLibs.srcDir("../../../target/android/jniLibs")
        }
        getByName("androidTest") {
            // The Java driver's conformance cases, run on the database inside.
            java.srcDir("../../java/src/test/java")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs.useLegacyPackaging = false
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("junit:junit:4.13.2")
}
