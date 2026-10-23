/* libnucleus against a running nucleus server (NUCLEUS_HOST, NUCLEUS_PORT;
 * scripts/driver-tests.sh starts one). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "nucleus.h"

static int failures;

#define CHECK(cond)                                                       \
    do {                                                                  \
        if (!(cond)) {                                                    \
            fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                   \
        }                                                                 \
    } while (0)

static nc_config config(void) {
    nc_config cfg = nc_default_config();
    const char *host = getenv("NUCLEUS_HOST"), *port = getenv("NUCLEUS_PORT");
    if (host) cfg.host = host;
    if (port) cfg.port = (uint16_t)atoi(port);
    return cfg;
}

static void table(char *name, size_t size) {
    static unsigned counter;
    snprintf(name, size, "c_%lx_%u", (unsigned long)time(NULL), ++counter);
}

static const char *text_of(const nc_result *r, size_t row, size_t col) {
    static char buf[256];
    nc_format(nc_value_at(r, row, col), buf, sizeof buf);
    return buf;
}

static nc_result *must(nc_conn *db, const char *sql, const nc_value *p, size_t n) {
    nc_error err;
    nc_result *r = nc_query(db, sql, p, n, &err);
    if (!r) {
        fprintf(stderr, "query failed: %s (%s)\n  %s\n", err.message, err.code, sql);
        failures++;
    }
    return r;
}

static void types_both_ways(void) {
    nc_error err;
    nc_config cfg = config();
    nc_conn *db = nc_connect(&cfg, &err);
    char t[64], sql[512];
    nc_value tags[3], params[10], num;
    nc_result *r;
    CHECK(db != NULL);
    if (!db) return;
    table(t, sizeof t);
    snprintf(sql, sizeof sql, "CREATE TABLE %s (id INT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2), "
             "d DATE, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[])", t);
    nc_result_free(must(db, sql, NULL, 0));
    tags[0] = nc_text("a");
    tags[1] = nc_null();
    tags[2] = nc_text("c");
    CHECK(nc_numeric("-123.45", &num) == 0);
    params[0] = nc_int(1);
    params[1] = nc_bool(1);
    params[2] = nc_float(0.5);
    params[3] = nc_text("h\xc3\xa9llo");
    params[4] = num;
    params[5] = nc_date(nc_days_from_ymd(2024, 2, 29));
    params[6] = nc_timestamp(763394706500000LL); /* 2024-03-10 14:05:06.5 */
    params[7] = nc_timestamptz(763387200000000LL);
    params[8] = nc_interval(1, 2, 3000000);
    params[9] = nc_array(tags, 3);
    snprintf(sql, sizeof sql, "INSERT INTO %s VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)", t);
    r = must(db, sql, params, 10);
    if (r) {
        CHECK(strcmp(nc_command(r), "INSERT") == 0 && nc_count(r) == 1);
        nc_result_free(r);
    }
    snprintf(sql, sizeof sql, "SELECT * FROM %s", t);
    r = must(db, sql, NULL, 0);
    if (r) {
        CHECK(nc_rows(r) == 1 && nc_columns(r) == 10);
        CHECK(nc_column_type(r, 4) == NC_NUMERIC);
        CHECK(nc_column_type(r, 9) == NC_ARRAY && nc_column_element(r, 9) == NC_TEXT);
        CHECK(nc_value_at(r, 0, 0)->as.integer == 1);
        CHECK(nc_value_at(r, 0, 1)->as.boolean == 1);
        CHECK(strcmp(text_of(r, 0, 3), "h\xc3\xa9llo") == 0);
        CHECK(strcmp(text_of(r, 0, 4), "-123.45") == 0);
        CHECK(strcmp(text_of(r, 0, 5), "2024-02-29") == 0);
        CHECK(strcmp(text_of(r, 0, 6), "2024-03-10 14:05:06.5") == 0);
        CHECK(strcmp(text_of(r, 0, 7), "2024-03-10 12:00:00+00") == 0);
        CHECK(nc_value_at(r, 0, 8)->as.interval.days == 2);
        CHECK(strcmp(text_of(r, 0, 9), "{a,NULL,c}") == 0);
        nc_result_free(r);
    }
    {
        nc_value p = nc_text("41");
        r = must(db, "SELECT 1.50 + 1, $1 + 1, NULL", &p, 1);
        if (r) {
            CHECK(strcmp(text_of(r, 0, 0), "2.50") == 0);
            CHECK(nc_value_at(r, 0, 1)->as.integer == 42);
            CHECK(nc_value_at(r, 0, 2)->type == NC_NULL);
            nc_result_free(r);
        }
    }
    nc_close(db);
}

static void errors_and_transactions(void) {
    nc_error err;
    nc_config cfg = config();
    nc_conn *db = nc_connect(&cfg, &err);
    char t[64], sql[256];
    nc_result *r;
    CHECK(db != NULL);
    if (!db) return;
    CHECK(nc_query(db, "SELECT * FROM nosuch", NULL, 0, &err) == NULL && strcmp(err.code, "42P01") == 0);
    CHECK(nc_query(db, "SELECT 1 / 0", NULL, 0, &err) == NULL && strcmp(err.code, "22012") == 0);
    table(t, sizeof t);
    snprintf(sql, sizeof sql, "CREATE TABLE %s (id INT PRIMARY KEY)", t);
    nc_result_free(must(db, sql, NULL, 0));
    nc_result_free(must(db, "BEGIN", NULL, 0));
    CHECK(nc_status(db) == 1);
    snprintf(sql, sizeof sql, "INSERT INTO %s VALUES (1)", t);
    nc_result_free(must(db, sql, NULL, 0));
    CHECK(nc_query(db, sql, NULL, 0, &err) == NULL && strcmp(err.code, "23505") == 0);
    CHECK(nc_status(db) == 2);
    nc_result_free(must(db, "ROLLBACK", NULL, 0));
    CHECK(nc_status(db) == 0);
    snprintf(sql, sizeof sql, "SELECT count(*) FROM %s", t);
    r = must(db, sql, NULL, 0);
    if (r) {
        CHECK(nc_value_at(r, 0, 0)->as.integer == 0);
        nc_result_free(r);
    }
    nc_close(db);
}

static void uuids(void) {
    nc_error err;
    nc_config cfg = config();
    nc_conn *db = nc_connect(&cfg, &err);
    char t[64], sql[256];
    nc_uuid u, bad;
    nc_value p;
    nc_result *r;
    CHECK(db != NULL);
    if (!db) return;
    CHECK(nc_uuid_parse("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11", &u) == 0);
    CHECK(nc_uuid_parse("{a0eebc999c0b4ef8bb6d6bb9bd380a11}", &bad) == 0);
    CHECK(memcmp(u.bytes, bad.bytes, 16) == 0);
    CHECK(nc_uuid_parse("a0eeb-c999c0b4ef8bb6d6bb9bd380a11", &bad) != 0);
    table(t, sizeof t);
    snprintf(sql, sizeof sql, "CREATE TABLE %s (id UUID PRIMARY KEY, n INT)", t);
    nc_result_free(must(db, sql, NULL, 0));
    p = nc_uuid_value(u);
    snprintf(sql, sizeof sql, "INSERT INTO %s VALUES ($1, 1)", t);
    nc_result_free(must(db, sql, &p, 1));
    snprintf(sql, sizeof sql, "SELECT id, gen_random_uuid() FROM %s", t);
    r = must(db, sql, NULL, 0);
    if (r) {
        CHECK(nc_column_type(r, 0) == NC_UUID);
        CHECK(strcmp(nc_column_type_name(r, 0), "uuid") == 0);
        CHECK(nc_value_at(r, 0, 0)->type == NC_UUID);
        CHECK(memcmp(nc_value_at(r, 0, 0)->as.uuid.bytes, u.bytes, 16) == 0);
        CHECK(strcmp(text_of(r, 0, 0), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11") == 0);
        CHECK(nc_value_at(r, 0, 1)->as.uuid.bytes[6] >> 4 == 4);
        nc_result_free(r);
    }
    nc_close(db);
}

static void core_types(void) {
    nc_error err;
    nc_config cfg = config();
    nc_conn *db = nc_connect(&cfg, &err);
    char t[64], sql[512];
    static const uint8_t bytes[] = {0, 0xff, 7};
    nc_value params[9], ip, net, q[2];
    nc_result *r;
    CHECK(db != NULL);
    if (!db) return;
    CHECK(nc_inet_parse("10.1.2.3/8", &ip) == 0);
    CHECK(nc_inet_parse("2001:db8::/32", &net) == 0);
    net.type = NC_CIDR;
    CHECK(nc_inet_parse("1.2.3", &q[0]) != 0 && nc_inet_parse("1::2::3", &q[0]) != 0);
    table(t, sizeof t);
    snprintf(sql, sizeof sql, "CREATE TYPE %s_mood AS ENUM ('sad', 'happy')", t);
    nc_result_free(must(db, sql, NULL, 0));
    snprintf(sql, sizeof sql, "CREATE TABLE %s (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, ch CHAR(3), "
             "ip INET, net CIDR, m %s_mood)", t, t);
    nc_result_free(must(db, sql, NULL, 0));
    params[0] = nc_smallint(-2);
    params[1] = nc_integer(70000);
    params[2] = nc_real(0.1f);
    params[3] = nc_bytea(bytes, sizeof bytes);
    params[4] = nc_time(45296500000LL); /* 12:34:56.5 */
    params[5] = nc_text("ab");
    params[6] = ip;
    params[7] = net;
    params[8] = nc_text("happy");
    snprintf(sql, sizeof sql, "INSERT INTO %s VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", t);
    nc_result_free(must(db, sql, params, 9));
    snprintf(sql, sizeof sql, "SELECT * FROM %s", t);
    r = must(db, sql, NULL, 0);
    if (r) {
        static const nc_type want[] = {NC_SMALLINT, NC_INTEGER, NC_REAL, NC_BYTEA, NC_TIME,
                                       NC_CHAR, NC_INET, NC_CIDR, NC_ENUM};
        static const char *shown[] = {"-2", "70000", "0.1", "\\x00ff07", "12:34:56.5",
                                      "ab ", "10.1.2.3/8", "2001:db8::/32", "happy"};
        size_t i;
        for (i = 0; i < 9; i++) {
            CHECK(nc_column_type(r, i) == want[i]);
            CHECK(strcmp(text_of(r, 0, i), shown[i]) == 0);
        }
        CHECK(nc_value_at(r, 0, 0)->as.integer == -2);
        CHECK(nc_value_at(r, 0, 2)->as.real4 == 0.1f);
        CHECK(nc_value_at(r, 0, 3)->as.bytes.len == 3);
        CHECK(nc_value_at(r, 0, 6)->as.net.bits == 8 && nc_value_at(r, 0, 6)->as.net.addr[3] == 3);
        CHECK(strncmp(nc_column_type_name(r, 8), t, strlen(t)) == 0);
        nc_result_free(r);
    }
    CHECK(nc_inet_parse("::ffff:1.2.3.4", &q[0]) == 0);
    q[1] = nc_bytea(bytes, sizeof bytes);
    snprintf(sql, sizeof sql, "SELECT $1::inet, count(*) FROM %s WHERE b = $2", t);
    r = must(db, sql, q, 2);
    if (r) {
        CHECK(strcmp(text_of(r, 0, 0), "::ffff:1.2.3.4") == 0);
        CHECK(nc_value_at(r, 0, 1)->as.integer == 1);
        nc_result_free(r);
    }
    nc_close(db);
}

static void authentication(void) {
    nc_error err;
    nc_config cfg = config();
    cfg.password = "wrong";
    CHECK(nc_connect(&cfg, &err) == NULL && strcmp(err.code, "28P01") == 0);
}

static void tls(void) {
    nc_error err;
    nc_config cfg = config();
    nc_conn *db;
    nc_result *r;
    const char *ca = getenv("NUCLEUS_TLS_CA");
    if (!nc_tls_supported()) {
        cfg.tls = 1;
        CHECK(nc_connect(&cfg, &err) == NULL);
        return;
    }
    if (!ca) return;
    cfg.tls_ca = ca;
    db = nc_connect(&cfg, &err);
    CHECK(db != NULL);
    if (!db) {
        fprintf(stderr, "tls: %s\n", err.message);
        return;
    }
    r = must(db, "SELECT 'over tls'", NULL, 0);
    if (r) {
        CHECK(strcmp(text_of(r, 0, 0), "over tls") == 0);
        nc_result_free(r);
    }
    nc_close(db);
    cfg.tls_ca = NULL;
    cfg.tls = 1;
    cfg.tls_no_verify = 1;
    db = nc_connect(&cfg, &err);
    CHECK(db != NULL);
    if (db) nc_close(db);
    /* The test CA is not among the system's. */
    cfg.tls_no_verify = 0;
    CHECK(nc_connect(&cfg, &err) == NULL);
}

int main(void) {
    types_both_ways();
    errors_and_transactions();
    uuids();
    core_types();
    authentication();
    tls();
    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("libnucleus: all checks passed\n");
    return 0;
}
