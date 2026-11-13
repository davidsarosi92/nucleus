package io.github.davidsarosi92.nucleus.notes

import android.app.Activity
import android.os.Bundle
import android.util.Log
import android.widget.TextView
import io.github.davidsarosi92.nucleus.android.Database
import java.util.concurrent.Executors

/**
 * Notes kept in a database inside the app — `files/nucleus/notes` — and a
 * count of the app's launches, which goes on from the last.  The database
 * is opened off the main thread, once for the app.
 */
class NotesActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        val text = TextView(this).apply { setPadding(48, 96, 48, 48) }
        setContentView(text)
        worker.execute {
            val shown = notes()
            runOnUiThread { text.text = shown }
        }
    }

    private fun notes(): String =
        db.connect().use { c ->
            c.query(
                """CREATE TABLE IF NOT EXISTS notes (
                     id BIGINT PRIMARY KEY, body TEXT NOT NULL, created TIMESTAMPTZ NOT NULL DEFAULT now())""",
            )
            c.query("CREATE TABLE IF NOT EXISTS launches (n INT PRIMARY KEY, at TIMESTAMPTZ DEFAULT now())")
            val launch = c.query("INSERT INTO launches (n) SELECT coalesce(max(n), 0) + 1 FROM launches RETURNING n").rows()[0][0]
            c.query("INSERT INTO notes (id, body) SELECT coalesce(max(id), 0) + 1, $1 FROM notes", "launch $launch")
            val rows = c.query("SELECT body FROM notes ORDER BY id DESC LIMIT 5").rows().map { it[0] }
            val total = c.query("SELECT count(*) FROM notes").rows()[0][0]
            val line = "nucleus notes: launch $launch, $total notes, engine ${Database.engineVersion}"
            // What examples/android/run.sh reads in the log.
            Log.i("nucleus", line)
            "$line\n\n" + rows.joinToString("\n")
        }

    private val db: Database
        get() = opened ?: Database.open(this, "notes").also { opened = it }

    companion object {
        /** The app's one handle on its database. */
        private var opened: Database? = null
    }
}
