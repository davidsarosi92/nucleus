# nucleus on Android

A database inside the app (nucleus PLAN.md §M14.23): a Kotlin library
(an AAR, Android 8+) with the engine in `libnucleus_android.so` and the
Java driver's API — `NucleusClient` — over a session in the process.

```kotlin
import io.github.davidsarosi92.nucleus.android.Database

val database = Database.open(context, "notes")   // files/nucleus/notes; or Database.open(path), open(":memory:")
database.connect().use { db ->                   // a NucleusClient (drivers/java)
    db.query("CREATE TABLE IF NOT EXISTS notes (id bigint PRIMARY KEY, body text)")
    db.query("INSERT INTO notes VALUES ($1, $2)", 1L, "hello")
    for (r in db.query("SELECT * FROM notes").records()) println(r)
}
database.close()
```

Run it off the main thread, as any I/O.  `Database.engineVersion` is
the engine's version.

## Building

```sh
rustup target add aarch64-linux-android x86_64-linux-android
drivers/android/build-native.sh                  # ABIS="arm64-v8a" for one; ANDROID_NDK_HOME or the SDK's newest NDK
cd drivers/android && ./gradlew :nucleus:assembleRelease   # nucleus/build/outputs/aar/
```

`build-native.sh` builds the embedded edition (LTO, one codegen unit,
without the model modules) per ABI and links it with the JNI glue
(`nucleus/src/main/cpp/nucleus_android.c`) by the NDK's clang (API 26),
exporting only the JNI functions, aligned for 16 KB pages; it checks the
size budget — at most 8 MiB on arm64-v8a (now 7.45 MiB).

## Tests

With an emulator or a device attached:

```sh
cd drivers/android && ./gradlew :nucleus:connectedDebugAndroidTest
```

runs the Java driver's conformance cases on a database in the app (in
memory and in a directory), that a directory keeps its data, and that a
closed database refuses sessions.  `examples/android/run.sh` runs the
Notes example twice.
