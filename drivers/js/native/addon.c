/*
 * The embedded edition for Node, Bun and Deno (PLAN.md section M14.23): a
 * Node-API addon over the engine's C API (nucleus_embedded.h), linked
 * with the engine into one file, nucleus.node.
 *
 * A connection's bytes arrive on a thread of its own (it waits in
 * nc_recv) and reach JavaScript through a thread-safe function, as a
 * socket's 'data' would: the event loop never waits for the engine.
 *
 *   open(path, options)  -> database      (throws)
 *   connect(db, onData)  -> connection    onData(Buffer), then onData(null)
 *   send(conn, buffer)   -> boolean       false once the session ended
 *   end(conn)            ends the session (onData(null) follows)
 *   close(db)            closes the database (throws)
 *   version()            -> string
 */
#define NAPI_VERSION 8
#include <node_api.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#include "nucleus_embedded.h"

#ifdef _WIN32
#include <windows.h>
typedef HANDLE thread_t;
#else
#include <pthread.h>
typedef pthread_t thread_t;
#endif

#define CHUNK (64 * 1024)
#define CHECK(call)                                                   \
  do {                                                                \
    if ((call) != napi_ok) {                                          \
      napi_throw_error(env, NULL, "nucleus: a Node-API call failed"); \
      return NULL;                                                    \
    }                                                                 \
  } while (0)

typedef struct {
  nc_database *db;
} database;

/* Held by JavaScript (its external) and by the reading thread; the last
 * to let go disconnects and frees it. */
typedef struct {
  nc_connection *c;
  napi_threadsafe_function on_data;
  atomic_int refs;
} connection;

typedef struct {
  size_t len;
  unsigned char bytes[];
} chunk;

static void release(connection *c) {
  if (atomic_fetch_sub(&c->refs, 1) == 1) {
    nc_disconnect(c->c);
    free(c);
  }
}

static napi_value throw_owned(napi_env env, char *message) {
  napi_throw_error(env, NULL, message ? message : "nucleus: failed");
  nc_free_error(message);
  return NULL;
}

static char *string_arg(napi_env env, napi_value v) {
  size_t n = 0;
  if (napi_get_value_string_utf8(env, v, NULL, 0, &n) != napi_ok) return NULL;
  char *s = malloc(n + 1);
  if (s && napi_get_value_string_utf8(env, v, s, n + 1, &n) != napi_ok) {
    free(s);
    return NULL;
  }
  return s;
}

static void database_finalize(napi_env env, void *data, void *hint) {
  (void)env;
  (void)hint;
  database *d = data;
  if (d->db) nc_close(d->db, NULL);
  free(d);
}

static void connection_finalize(napi_env env, void *data, void *hint) {
  (void)env;
  (void)hint;
  connection *c = data;
  nc_end(c->c);
  release(c);
}

static napi_value js_version(napi_env env, napi_callback_info info) {
  (void)info;
  napi_value out;
  CHECK(napi_create_string_utf8(env, nc_version(), NAPI_AUTO_LENGTH, &out));
  return out;
}

static napi_value js_open(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  if (argc < 2) {
    napi_throw_type_error(env, NULL, "open(path, options)");
    return NULL;
  }
  char *path = string_arg(env, argv[0]);
  char *options = string_arg(env, argv[1]);
  if (!path || !options) {
    free(path);
    free(options);
    napi_throw_type_error(env, NULL, "open(path, options): strings");
    return NULL;
  }
  char *error = NULL;
  nc_database *db = nc_open(path, options, &error);
  free(path);
  free(options);
  if (!db) return throw_owned(env, error);
  database *d = malloc(sizeof *d);
  d->db = db;
  napi_value out;
  CHECK(napi_create_external(env, d, database_finalize, NULL, &out));
  return out;
}

static database *database_arg(napi_env env, napi_value v) {
  void *p = NULL;
  if (napi_get_value_external(env, v, &p) != napi_ok || !p || !((database *)p)->db) {
    napi_throw_error(env, NULL, "the database is closed");
    return NULL;
  }
  return p;
}

static napi_value js_close(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  void *p = NULL;
  if (argc < 1 || napi_get_value_external(env, argv[0], &p) != napi_ok || !p) return NULL;
  database *d = p;
  if (!d->db) return NULL;
  char *error = NULL;
  nc_database *db = d->db;
  d->db = NULL;
  if (nc_close(db, &error) != 0) return throw_owned(env, error);
  return NULL;
}

/* On the JavaScript thread: a chunk as a Buffer, or the end as null. */
static void call_on_data(napi_env env, napi_value fn, void *context, void *data) {
  (void)context;
  chunk *ch = data;
  if (env != NULL && fn != NULL) {
    napi_value arg, undefined;
    if (ch) {
      void *copy;
      napi_create_buffer_copy(env, ch->len, ch->bytes, &copy, &arg);
    } else {
      napi_get_null(env, &arg);
    }
    napi_get_undefined(env, &undefined);
    napi_call_function(env, undefined, fn, 1, &arg, NULL);
  }
  free(ch);
}

#ifdef _WIN32
static DWORD WINAPI reader(LPVOID arg)
#else
static void *reader(void *arg)
#endif
{
  connection *c = arg;
  unsigned char *buf = malloc(CHUNK);
  for (;;) {
    int64_t n = buf ? nc_recv(c->c, buf, CHUNK, -1) : 0;
    if (n <= 0) break;
    chunk *ch = malloc(sizeof *ch + (size_t)n);
    if (!ch) break;
    ch->len = (size_t)n;
    memcpy(ch->bytes, buf, (size_t)n);
    if (napi_call_threadsafe_function(c->on_data, ch, napi_tsfn_blocking) != napi_ok) {
      free(ch);
      break;
    }
  }
  free(buf);
  nc_end(c->c);
  napi_call_threadsafe_function(c->on_data, NULL, napi_tsfn_blocking);
  napi_release_threadsafe_function(c->on_data, napi_tsfn_release);
  release(c);
#ifdef _WIN32
  return 0;
#else
  return NULL;
#endif
}

static napi_value js_connect(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  if (argc < 2) {
    napi_throw_type_error(env, NULL, "connect(database, onData)");
    return NULL;
  }
  database *d = database_arg(env, argv[0]);
  if (!d) return NULL;
  char *error = NULL;
  nc_connection *nc = nc_connect(d->db, &error);
  if (!nc) return throw_owned(env, error);
  connection *c = malloc(sizeof *c);
  c->c = nc;
  atomic_init(&c->refs, 2);
  napi_value name;
  CHECK(napi_create_string_utf8(env, "nucleus session", NAPI_AUTO_LENGTH, &name));
  /* At most 16 chunks wait for JavaScript; then the reader waits, the
   * pipe fills and the session waits — a socket's backpressure. */
  if (napi_create_threadsafe_function(env, argv[1], NULL, name, 16, 1, NULL, NULL, NULL,
                                      call_on_data, &c->on_data) != napi_ok) {
    nc_disconnect(nc);
    free(c);
    napi_throw_error(env, NULL, "nucleus: cannot make the session's callback");
    return NULL;
  }
  thread_t t;
#ifdef _WIN32
  t = CreateThread(NULL, 0, reader, c, 0, NULL);
  int started = t != NULL;
  if (started) CloseHandle(t);
#else
  int started = pthread_create(&t, NULL, reader, c) == 0;
  if (started) pthread_detach(t);
#endif
  if (!started) {
    napi_release_threadsafe_function(c->on_data, napi_tsfn_abort);
    nc_disconnect(nc);
    free(c);
    napi_throw_error(env, NULL, "nucleus: cannot start the session's reader");
    return NULL;
  }
  napi_value out;
  CHECK(napi_create_external(env, c, connection_finalize, NULL, &out));
  return out;
}

static connection *connection_arg(napi_env env, napi_value v) {
  void *p = NULL;
  if (napi_get_value_external(env, v, &p) != napi_ok || !p) {
    napi_throw_type_error(env, NULL, "not a connection");
    return NULL;
  }
  return p;
}

static napi_value js_send(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  if (argc < 2) {
    napi_throw_type_error(env, NULL, "send(connection, buffer)");
    return NULL;
  }
  connection *c = connection_arg(env, argv[0]);
  if (!c) return NULL;
  void *data;
  size_t len;
  CHECK(napi_get_buffer_info(env, argv[1], &data, &len));
  napi_value out;
  CHECK(napi_get_boolean(env, nc_send(c->c, data, len) == 0, &out));
  return out;
}

static napi_value js_end(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  connection *c = argc < 1 ? NULL : connection_arg(env, argv[0]);
  if (c) nc_end(c->c);
  return NULL;
}

NAPI_MODULE_INIT(/* napi_env env, napi_value exports */) {
  static const struct {
    const char *name;
    napi_callback fn;
  } fns[] = {{"version", js_version}, {"open", js_open},   {"close", js_close},
             {"connect", js_connect}, {"send", js_send},   {"end", js_end}};
  for (size_t i = 0; i < sizeof fns / sizeof fns[0]; i++) {
    napi_value f;
    if (napi_create_function(env, fns[i].name, NAPI_AUTO_LENGTH, fns[i].fn, NULL, &f) != napi_ok ||
        napi_set_named_property(env, exports, fns[i].name, f) != napi_ok)
      return NULL;
  }
  return exports;
}
