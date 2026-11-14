package io.github.davidsarosi92.nucleus.android

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.davidsarosi92.nucleus.NucleusException
import io.github.davidsarosi92.nucleus.NucleusTest
import java.io.File
import java.util.UUID
import java.util.function.Supplier
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.junit.runner.RunWith

/** On a device (or an emulator): the database inside the app's process. */
@RunWith(AndroidJUnit4::class)
class DatabaseTest {
    /** The Java driver's conformance cases, each on a session of the database in memory. */
    @Test
    fun theDriversCasesInMemory() {
        Database.open().use { db ->
            NucleusTest.connector = Supplier { db.connect() }
            assertEquals(12, NucleusTest.runCases())
        }
    }

    /** The same on a directory in the app's files. */
    @Test
    fun theDriversCasesOnADirectory() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val name = "cases-${UUID.randomUUID()}"
        Database.open(context, name).use { db ->
            NucleusTest.connector = Supplier { db.connect() }
            assertEquals(12, NucleusTest.runCases())
        }
        File(File(context.filesDir, "nucleus"), name).deleteRecursively()
    }

    /** A directory keeps what was committed: closed, opened again. */
    @Test
    fun aDirectoryKeepsItsData() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val name = "kept-${UUID.randomUUID()}"
        Database.open(context, name, "cache=16MB").use { db ->
            db.connect().use { c ->
                c.query("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")
                c.executeMany("INSERT INTO notes VALUES ($1, $2)", (1..1000).map { arrayOf<Any>(it, "note $it") })
            }
        }
        Database.open(context, name).use { db ->
            val a = db.connect()
            val b = db.connect()
            assertEquals(listOf<Any>(1000L, "note 999"), a.query("SELECT count(*), max(body) FROM notes").rows()[0])
            // Two sessions, one database: the second sees the first's commit.
            a.query("INSERT INTO notes VALUES (1001, 'from a')")
            assertEquals("from a", b.query("SELECT body FROM notes WHERE id = 1001").rows()[0][0])
            a.close()
            b.close()
        }
        assertTrue(File(File(context.filesDir, "nucleus"), name).deleteRecursively())
    }

    @Test
    fun aClosedDatabaseRefusesSessions() {
        val db = Database.open()
        db.close()
        try {
            db.connect()
            fail("a session on a closed database")
        } catch (e: NucleusException) {
            assertTrue(e.message!!.contains("closed"))
        }
        assertTrue(Database.engineVersion.isNotEmpty())
    }
}
