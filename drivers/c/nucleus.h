/*
 * libnucleus — the C driver for nucleus: the nucleus protocol (ncp,
 * DOCS.md section 10).  C99, no dependencies; POSIX sockets, and Winsock
 * on Windows.
 *
 *     nc_error err;
 *     nc_config cfg = nc_default_config();     // 127.0.0.1:20035, nucleus / nucleus
 *     nc_conn *db = nc_connect(&cfg, &err);
 *     if (!db) { fprintf(stderr, "%s (%s)\n", err.message, err.code); return 1; }
 *     nc_value params[] = { nc_int(1), nc_text("one") };
 *     nc_result *r = nc_query(db, "INSERT INTO t VALUES ($1, $2)", params, 2, &err);
 *     nc_result_free(r);
 *     nc_cursor *rows = nc_cursor_open(db, "SELECT id FROM t", NULL, 0, 0, &err);
 *     while (rows && nc_cursor_next(rows, &err) == 1)     // a large result,
 *         use(nc_value_at(nc_cursor_result(rows), 0, 0));  // as it arrives
 *     nc_cursor_close(rows);
 *     nc_close(db);
 *
 * A result belongs to the caller (nc_result_free); values read from it live
 * as long as the result.  One thread at a time per connection.
 */
#ifndef NUCLEUS_H
#define NUCLEUS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* This driver's version. */
#define NC_VERSION "0.1.0"
/* 2: registered types (uuid, modules' types) arrive in their binary form;
 * 3: smallint, integer, real, bytea, time, inet and cidr in theirs;
 * 4: the password is proved by SCRAM-SHA-256, never sent; 5: errors carry
 * a hint; 6: results stream, fetched in batches; 7: columnar results. */
#define NC_PROTOCOL_VERSION 7
/* Rows a cursor asks the server for at a time. */
#define NC_DEFAULT_FETCH 1000
/* Rows a columnar batch holds when none is given. */
#define NC_DEFAULT_BATCH 65536
#define NC_DEFAULT_PORT 20035

/* Value and column types (the protocol's tags). */
typedef enum nc_type {
    NC_NULL = 0, /* for a column: unknown */
    NC_BOOL = 1,
    NC_INT = 2,
    NC_FLOAT = 3,
    NC_TEXT = 4,
    NC_NUMERIC = 5,
    NC_DATE = 6,
    NC_TIMESTAMP = 7,
    NC_TIMESTAMPTZ = 8,
    NC_INTERVAL = 9,
    NC_ARRAY = 10,
    NC_EXT = 11,  /* another registered type: its name and binary form */
    NC_SMALLINT = 12, /* as.integer */
    NC_INTEGER = 13,  /* as.integer */
    NC_REAL = 14,     /* as.real4 */
    NC_BYTEA = 15,    /* as.bytes */
    NC_TIME = 16,     /* as.time: µs since midnight */
    NC_CHAR = 17,     /* a column only: char(n); its values are NC_TEXT */
    NC_INET = 18,     /* as.net */
    NC_CIDR = 19,     /* as.net */
    NC_ENUM = 20,     /* a column only: an enum (nc_column_type_name); its values are NC_TEXT */
    NC_UUID = 100 /* uuid (on the wire, NC_EXT named "uuid") */
} nc_type;

/* A uuid: its 16 bytes. */
typedef struct nc_uuid {
    uint8_t bytes[16];
} nc_uuid;

typedef struct nc_value nc_value;

/* A value. */
struct nc_value {
    nc_type type;
    union {
        int boolean;
        int64_t integer;        /* bigint, integer, smallint */
        double real;
        float real4;            /* real */
        struct { const uint8_t *ptr; size_t len; } bytes;      /* bytea */
        int64_t time;           /* µs since midnight (24:00:00 included) */
        struct { uint8_t family, bits; uint8_t addr[16]; } net; /* family 4 (addr[0..4]) or 6 */
        struct { const char *ptr; size_t len; } text;          /* NUL-terminated */
        struct { uint64_t lo; int64_t hi; uint8_t scale; } numeric; /* (hi·2^64 + lo) × 10^-scale */
        int32_t date;           /* days since 2000-01-01 */
        int64_t timestamp;      /* µs since 2000-01-01 00:00 (UTC for timestamptz) */
        struct { int32_t months, days; int64_t micros; } interval;
        struct { const nc_value *items; size_t len; } array;
        nc_uuid uuid;
        struct { const char *type_name; const uint8_t *data; size_t len; } ext;
    } as;
};

typedef struct nc_config {
    const char *host;
    uint16_t port;
    const char *user;
    const char *password;
    const char *database;
    const char *client_name;
    /* TLS (a build with NC_WITH_OPENSSL; see nc_tls_supported): 1 to
     * encrypt, the server's certificate checked against the system's CA
     * certificates — or against tls_ca's (a PEM file; setting it implies
     * tls).  tls_no_verify = 1 skips the check: encrypted, but anyone can
     * pose as the server.  tls_cert and tls_key (PEM files) show the
     * server a client certificate: one naming the user logs it in without
     * a password, if the server accepts its CA. */
    int tls;
    const char *tls_ca;
    int tls_no_verify;
    const char *tls_cert;
    const char *tls_key;
    /* An OpenID Connect identity token to log in with instead of the
     * password: it names the user (OAUTHBEARER). */
    const char *token;
} nc_config;

/* An error: SQLSTATE (empty for a driver or network error), message, and
 * hint — what to do about it, the name meant, the cast that would work
 * (empty: none). */
typedef struct nc_error {
    char code[6];
    char message[512];
    char hint[512];
} nc_error;

typedef struct nc_conn nc_conn;
typedef struct nc_result nc_result;
typedef struct nc_cursor nc_cursor;

/* The defaults: 127.0.0.1:20035, user, password and database "nucleus";
 * no TLS. */
nc_config nc_default_config(void);

/* 1 if this build can do TLS (NC_WITH_OPENSSL), else 0. */
int nc_tls_supported(void);

/* Connects and logs in; NULL (and err filled) on failure. */
nc_conn *nc_connect(const nc_config *cfg, nc_error *err);

/* Runs sql (one statement with parameters, or several without) and
 * returns the last statement's result; NULL (and err filled) when a
 * statement fails or the connection is lost. */
nc_result *nc_query(nc_conn *c, const char *sql, const nc_value *params, size_t nparams, nc_error *err);

/* Runs one statement and reads its rows as they arrive, fetch at a time
 * (0: NC_DEFAULT_FETCH): the server sends a batch and waits until it is
 * read, so a result of any size takes a batch of memory.  NULL (and err
 * filled) when the statement fails before its first row.  Running another
 * statement on the connection ends the cursor (what is left is skipped). */
nc_cursor *nc_cursor_open(nc_conn *c, const char *sql, const nc_value *params, size_t nparams, unsigned fetch,
                          nc_error *err);
/* The next row: 1 a row (row 0 of nc_cursor_result, until the next call),
 * 0 the end, -1 an error (err filled: the statement's, after its rows). */
int nc_cursor_next(nc_cursor *cur, nc_error *err);
/* The columns, the current row, and once the statement has ended its
 * command and count (the rows sent); read with the result functions. */
const nc_result *nc_cursor_result(const nc_cursor *cur);
/* Stops the statement if it has not ended: the server is told to send no
 * more, and what it sent is skipped.  The command and count stay. */
void nc_cursor_stop(nc_cursor *cur);
/* Stops the statement if it has not ended, and frees the cursor. */
void nc_cursor_close(nc_cursor *cur);

/* Columnar results (protocol version 7): runs one statement and reads its
 * result a batch of `rows` rows at a time (0: NC_DEFAULT_BATCH), each column
 * one buffer.  nc_cursor_next reads the next batch (1, 0 the end, -1 an
 * error); the accessors below point into it (valid until the next call;
 * little-endian, 8-byte aligned).  The columns: nc_cursor_result. */
nc_cursor *nc_batches_open(nc_conn *c, const char *sql, const nc_value *params, size_t nparams, unsigned rows,
                           nc_error *err);
size_t nc_batch_rows(const nc_cursor *cur);
/* A column's data: a fixed width's values back to back — uint8_t
 * (boolean), int16_t, int32_t (integer; date as days since 2000-01-01),
 * float, double, int64_t (bigint; timestamps as µs since 2000-01-01, time
 * as µs since midnight), 16 bytes (interval: int32 months, int32 days,
 * int64 µs; uuid), 17 (numeric: int128 mantissa, uint8 scale) — or a
 * variable width's bytes (text, char(n), enums: UTF-8; bytea; inet/cidr:
 * family, bits, address; arrays: a value as in rows), with its offsets. */
const void *nc_batch_data(const nc_cursor *cur, size_t col, size_t *len);
/* rows + 1 offsets into nc_batch_data; NULL for a fixed width. */
const uint32_t *nc_batch_offsets(const nc_cursor *cur, size_t col);
/* A bit per row, least significant first, set for a value; NULL: no NULLs. */
const uint8_t *nc_batch_validity(const nc_cursor *cur, size_t col);
int nc_batch_is_null(const nc_cursor *cur, size_t col, size_t row);

/* 0 idle, 1 in a transaction block, 2 in a failed block. */
int nc_status(const nc_conn *c);
const char *nc_server_version(const nc_conn *c);
uint32_t nc_session_id(const nc_conn *c);

/* Says Bye and frees the connection. */
void nc_close(nc_conn *c);

/* Results. */
size_t nc_columns(const nc_result *r);
const char *nc_column_name(const nc_result *r, size_t col);
nc_type nc_column_type(const nc_result *r, size_t col);
nc_type nc_column_element(const nc_result *r, size_t col); /* for NC_ARRAY columns */
/* A registered type's or an enum's name ("uuid", …) for NC_UUID, NC_EXT and
 * NC_ENUM columns, else NULL. */
const char *nc_column_type_name(const nc_result *r, size_t col);
size_t nc_rows(const nc_result *r);
const nc_value *nc_value_at(const nc_result *r, size_t row, size_t col);
const char *nc_command(const nc_result *r); /* "SELECT", "INSERT", … */
uint64_t nc_count(const nc_result *r);      /* rows returned or affected */
void nc_result_free(nc_result *r);

/* Values to send. */
nc_value nc_null(void);
nc_value nc_bool(int b);
nc_value nc_int(int64_t i);
nc_value nc_float(double f);
nc_value nc_smallint(int16_t i);
nc_value nc_integer(int32_t i);
nc_value nc_real(float f);
/* bytea: the bytes must outlive the query. */
nc_value nc_bytea(const uint8_t *data, size_t len);
nc_value nc_time(int64_t micros_since_midnight);
/* inet / cidr: family 4 (4 bytes of addr) or 6 (16), and a mask length. */
nc_value nc_inet(int family, const uint8_t *addr, int bits);
nc_value nc_cidr(int family, const uint8_t *addr, int bits);
/* Parses "10.1.2.3/8" or "2001:db8::/32" (IPv4 dotted, IPv6 with "::"
 * and an IPv4 tail) into an inet; 0 on success. */
int nc_inet_parse(const char *text, nc_value *out);
nc_value nc_text(const char *s);
nc_value nc_date(int32_t days);
nc_value nc_timestamp(int64_t micros);
nc_value nc_timestamptz(int64_t micros);
nc_value nc_interval(int32_t months, int32_t days, int64_t micros);
nc_value nc_array(const nc_value *items, size_t len);
nc_value nc_uuid_value(nc_uuid u);
/* A registered type's value from its name and binary form (both must
 * outlive the query). */
nc_value nc_ext(const char *type_name, const uint8_t *data, size_t len);
/* Parses a uuid's text (32 hex digits, hyphens after groups of four,
 * optionally in braces); 0 on success. */
int nc_uuid_parse(const char *text, nc_uuid *out);
/* Parses "-12.50" into a numeric; 0 on success. */
int nc_numeric(const char *text, nc_value *out);

/* Dates: days since 2000-01-01 <-> year, month, day. */
int32_t nc_days_from_ymd(int year, int month, int day);
void nc_ymd_from_days(int32_t days, int *year, int *month, int *day);

/* Writes v as text (as ncsql prints it) into buf, like snprintf: returns
 * the length the full text needs. */
size_t nc_format(const nc_value *v, char *buf, size_t size);

#ifdef __cplusplus
}
#endif

#endif /* NUCLEUS_H */
