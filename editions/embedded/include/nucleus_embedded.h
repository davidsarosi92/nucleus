/*
 * nucleus embedded: a database inside the application (PLAN.md §M14.23).
 *
 * nc_open opens a database in a directory, or in memory (":memory:");
 * nc_connect starts a session on it.  A session speaks the nucleus
 * protocol (DOCS.md §10) exactly as over a socket — nc_send and nc_recv
 * carry its bytes — so a driver written for the network runs over it
 * unchanged.  There is no login: the session is the user its Hello names.
 *
 * Threads: a connection is one thread's at a time (but for nc_end);
 * different connections, and the database, may be used from any threads.  nc_recv blocks the
 * calling thread only (no lock is held while it waits).
 *
 * Errors: functions that fail return NULL or -1 and, when given a
 * non-NULL `char **error`, set it to a message to free with
 * nc_free_error.
 */
#ifndef NUCLEUS_EMBEDDED_H
#define NUCLEUS_EMBEDDED_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NcDatabase nc_database;
typedef struct NcConnection nc_connection;

/* This library's version ("0.1.0"); static, not to be freed. */
const char *nc_version(void);

/*
 * Opens the database in directory `path` (made if missing), or a new one
 * in memory for ":memory:".  `options` (NULL for none):
 * "cache=64MB; sync=ordered|full; wal=minimal|full".  A directory this
 * process has open already is shared; another process's is refused (its
 * lock).
 */
nc_database *nc_open(const char *path, const char *options, char **error);

/* A new session on `db`. */
nc_connection *nc_connect(nc_database *db, char **error);

/* Sends `len` bytes to the session: 0, or -1 once it has ended. */
int nc_send(nc_connection *c, const uint8_t *buf, size_t len);

/*
 * Receives up to `cap` bytes the session sent, waiting for at least one
 * up to `timeout_ms` (negative: no limit).  Returns the bytes received,
 * 0 once the session has ended, or -1 when the time ran out.
 */
int64_t nc_recv(nc_connection *c, uint8_t *buf, size_t cap, int64_t timeout_ms);

/*
 * Ends the session without freeing `c`: a thread waiting in nc_recv on it
 * returns (what is left, then 0).  The one call another thread may make
 * on a connection while one reads it; nc_disconnect after.
 */
void nc_end(nc_connection *c);

/* Ends the session and frees `c`. */
void nc_disconnect(nc_connection *c);

/*
 * Closes and frees `db`: its sessions end (each after its statement; their
 * connections then receive the end), it checkpoints, and the directory is
 * free again — unless another handle in this process has it open.
 * 0, or -1 with `*error` set.
 */
int nc_close(nc_database *db, char **error);

/* Frees an error message this library set. */
void nc_free_error(char *error);

#ifdef __cplusplus
}
#endif

#endif /* NUCLEUS_EMBEDDED_H */
