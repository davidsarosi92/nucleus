// The Android library (nucleus-android): the embedded edition and the Java
// driver as an AAR, and the example app (examples/android/notes) built
// against it.  drivers/android/build-native.sh first.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "nucleus-android"
include(":nucleus")
include(":notes")
project(":notes").projectDir = file("../../examples/android/notes")
