/*
 * nucleus embedded: a database inside the application (PLAN.md §M14.23).
 *
 * nce_open opens a database in a directory, or in memory (":memory:");
 * nce_connect starts a session on it.  A session speaks the nucleus
 * protocol (DOCS.md §10) exactly as over a socket — nce_send and nce_recv
 * carry its bytes — so a driver written for the network runs over it
 * unchanged.  There is no login: the session is the user its Hello names.
 *
 * Threads: a connection is one thread's at a time (but for nce_end);
 * different connections, and the database, may be used from any threads.  nce_recv blocks the
 * calling thread only (no lock is held while it waits).
 *
 * Errors: functions that fail return NULL or -1 and, when given a
 * non-NULL `char **error`, set it to a message to free with
 * nce_free_error.
 */
#ifndef NUCLEUS_EMBEDDED_H
#define NUCLEUS_EMBEDDED_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NcDatabase nce_database;
typedef struct NcConnection nce_connection;

/* This library's version ("0.1.0"); static, not to be freed. */
const char *nce_version(void);

/*
 * Opens the database in directory `path` (made if missing), or a new one
 * in memory for ":memory:".  `options` (NULL for none):
 * "cache=64MB; sync=ordered|full; wal=minimal|full".  A directory this
 * process has open already is shared; another process's is refused (its
 * lock).
 */
nce_database *nce_open(const char *path, const char *options, char **error);

/* A new session on `db`. */
nce_connection *nce_connect(nce_database *db, char **error);

/* Sends `len` bytes to the session: 0, or -1 once it has ended. */
int nce_send(nce_connection *c, const uint8_t *buf, size_t len);

/*
 * Receives up to `cap` bytes the session sent, waiting for at least one
 * up to `timeout_ms` (negative: no limit).  Returns the bytes received,
 * 0 once the session has ended, or -1 when the time ran out.
 */
int64_t nce_recv(nce_connection *c, uint8_t *buf, size_t cap, int64_t timeout_ms);

/*
 * Ends the session without freeing `c`: a thread waiting in nce_recv on it
 * returns (what is left, then 0).  The one call another thread may make
 * on a connection while one reads it; nce_disconnect after.
 */
void nce_end(nce_connection *c);

/* Ends the session and frees `c`. */
void nce_disconnect(nce_connection *c);

/*
 * Closes and frees `db`: its sessions end (each after its statement; their
 * connections then receive the end), it checkpoints, and the directory is
 * free again — unless another handle in this process has it open.
 * 0, or -1 with `*error` set.
 */
int nce_close(nce_database *db, char **error);

/* Frees an error message this library set. */
void nce_free_error(char *error);

/*
 * Sessions without threads (WebAssembly, where nothing may wait): a
 * session run on the caller's thread, stepped by the bytes given to it.
 */
typedef struct NcExchange nce_stepped;

/* A new stepped session on `db` (NULL on failure, `*error` set). */
nce_stepped *nce_exchange(nce_database *db, char **error);

/*
 * Gives the session `len` bytes and runs the whole messages among what it
 * has; returns its reply, `*reply_len` bytes, valid until the next call on
 * `x`.  A COPY's data goes with the COPY (one without fails, 57014); a
 * fetch's batches follow one another without being asked for; one
 * statement's reply holds at most 256 MB (54000 past it).
 */
const uint8_t *nce_feed(nce_stepped *x, const uint8_t *buf, size_t len, size_t *reply_len);

/* 1 once the session has ended (a Bye, a failed login), else 0. */
int nce_exchange_ended(nce_stepped *x);

/* Frees the session. */
void nce_exchange_free(nce_stepped *x);

/* Memory of this library's, for a host that writes into it (WebAssembly's). */
uint8_t *nce_alloc(size_t len);
void nce_dealloc(uint8_t *p, size_t len);

/*
 * WebAssembly: the free slots of the storage pool the host provides
 * (imports `nucleus_slots`: count, read, write, size, truncate, flush),
 * which it tops up between calls; -1 elsewhere or before a directory is
 * opened.
 */
int64_t nce_pool_free(void);

#ifdef __cplusplus
}
#endif

#endif /* NUCLEUS_EMBEDDED_H */
