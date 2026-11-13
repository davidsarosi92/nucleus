package io.github.davidsarosi92.nucleus.android

/** The engine's C API (libnucleus_android.so: the embedded edition and its JNI glue). */
internal object Native {
    init {
        System.loadLibrary("nucleus_android")
    }

    @JvmStatic external fun version(): String

    @JvmStatic external fun open(path: String, options: String?): Long

    @JvmStatic external fun close(db: Long)

    @JvmStatic external fun connect(db: Long): Long

    /** 0, or -1 once the session has ended. */
    @JvmStatic external fun send(conn: Long, buf: ByteArray, off: Int, len: Int): Int

    /** Bytes received (waiting for one), or -1 once the session has ended. */
    @JvmStatic external fun recv(conn: Long, buf: ByteArray, off: Int, len: Int): Int

    @JvmStatic external fun end(conn: Long)

    @JvmStatic external fun disconnect(conn: Long)
}
