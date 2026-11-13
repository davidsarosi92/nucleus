package io.github.davidsarosi92.nucleus.android

import android.content.Context
import io.github.davidsarosi92.nucleus.Config
import io.github.davidsarosi92.nucleus.NucleusClient
import io.github.davidsarosi92.nucleus.NucleusException
import java.io.Closeable
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.atomic.AtomicBoolean

/**
 * A database inside the app (PLAN.md §M14.23): the embedded edition — the
 * engine in the app's process — with the Java driver's API.  Each
 * [connect] is a session with the API a server's connection has.
 *
 * ```
 * val db = Database.open(context, "notes")           // files/nucleus/notes: kept
 * db.connect().use { c ->
 *     c.query("CREATE TABLE IF NOT EXISTS notes (id INT PRIMARY KEY, body TEXT)")
 *     c.query("INSERT INTO notes VALUES ($1, $2)", 1, "hello")
 * }
 * ```
 */
class Database private constructor(private var handle: Long, val path: String) : AutoCloseable {
    companion object {
        /**
         * Opens the database in directory [path] (made if missing), or a new
         * one in memory for `":memory:"`.  [options]: `"cache=64MB;
         * sync=ordered|full; wal=minimal|full"`.  A directory this process
         * has open already is shared.
         */
        @JvmStatic
        @JvmOverloads
        fun open(path: String = ":memory:", options: String = ""): Database =
            try {
                Database(Native.open(path, options), path)
            } catch (e: IOException) {
                throw NucleusException(e.message ?: "cannot open $path", null as String?)
            }

        /** Opens (or makes) the database [name] in the app's files: `files/nucleus/<name>`. */
        @JvmStatic
        @JvmOverloads
        fun open(context: Context, name: String, options: String = ""): Database =
            open(File(File(context.filesDir, "nucleus"), name).also { it.parentFile?.mkdirs() }.path, options)

        /** The engine's version. */
        @JvmStatic
        val engineVersion: String
            get() = Native.version()
    }

    /** A new session, as [user] (who it runs as: there is no login). */
    @JvmOverloads
    fun connect(user: String = "nucleus", database: String = "nucleus"): NucleusClient {
        val h = handle
        if (h == 0L) throw NucleusException("the database is closed", null as String?)
        val conn =
            try {
                Native.connect(h)
            } catch (e: IOException) {
                throw NucleusException(e.message ?: "cannot start a session", null as String?)
            }
        val pipe = Session(conn)
        val config = Config().user(user).database(database).clientName("nucleus-android")
        return NucleusClient.connect(pipe.input, pipe.output, pipe, config)
    }

    /** Closes it: its sessions end, a directory is checkpointed.  Close its clients first. */
    override fun close() {
        val h = handle
        handle = 0
        if (h != 0L) {
            try {
                Native.close(h)
            } catch (e: IOException) {
                throw NucleusException(e.message ?: "cannot close $path", null as String?)
            }
        }
    }
}

/** A session's bytes as streams: what the Java driver reads and writes. */
private class Session(private val conn: Long) : Closeable {
    private val closed = AtomicBoolean()

    val input: InputStream =
        object : InputStream() {
            override fun read(): Int {
                val one = ByteArray(1)
                return if (read(one, 0, 1) < 0) -1 else one[0].toInt() and 0xff
            }

            override fun read(b: ByteArray, off: Int, len: Int): Int {
                if (len == 0) return 0
                if (closed.get()) return -1
                return Native.recv(conn, b, off, len)
            }
        }

    val output: OutputStream =
        object : OutputStream() {
            override fun write(b: Int) = write(byteArrayOf(b.toByte()), 0, 1)

            override fun write(b: ByteArray, off: Int, len: Int) {
                if (closed.get() || Native.send(conn, b, off, len) != 0) throw IOException("the session has ended")
            }
        }

    override fun close() {
        if (closed.compareAndSet(false, true)) Native.disconnect(conn)
    }
}
