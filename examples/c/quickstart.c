/* nucleus from C: connect, create, insert with parameters, query, a
 * transaction.  Run a server first (`nucleus server ./mydb`), then:
 *
 *   cc -std=c99 -Idrivers/c -o quickstart examples/c/quickstart.c drivers/c/nucleus.c
 *   ./quickstart
 */
#include <stdio.h>
#include <stdlib.h>

#include "nucleus.h"

static nc_result *run(nc_conn *db, const char *sql, const nc_value *params, size_t n) {
    nc_error err;
    nc_result *r = nc_query(db, sql, params, n, &err);
    if (!r) fprintf(stderr, "error: %s (%s)\n", err.message, err.code);
    return r;
}

int main(void) {
    nc_error err;
    nc_config cfg = nc_default_config(); /* 127.0.0.1:20035, user/password nucleus */
    nc_conn *db;
    nc_result *r;
    nc_value total, params[4];
    size_t i, j;
    const char *port = getenv("NUCLEUS_PORT");
    if (port) cfg.port = (uint16_t)atoi(port);

    db = nc_connect(&cfg, &err);
    if (!db) {
        fprintf(stderr, "cannot connect: %s (%s)\n", err.message, err.code);
        return 1;
    }
    printf("connected to %s\n", nc_server_version(db));

    nc_result_free(run(db, "DROP TABLE IF EXISTS c_orders", NULL, 0));
    nc_result_free(run(db, "CREATE TABLE c_orders (id INT PRIMARY KEY, customer TEXT NOT NULL, "
                           "total NUMERIC(10, 2), placed DATE)", NULL, 0));

    nc_numeric("19.99", &total);
    params[0] = nc_int(1);
    params[1] = nc_text("Anna");
    params[2] = total;
    params[3] = nc_date(nc_days_from_ymd(2024, 5, 1));
    nc_result_free(run(db, "INSERT INTO c_orders VALUES ($1, $2, $3, $4)", params, 4));

    /* A transaction: both statements or neither. */
    nc_result_free(run(db, "BEGIN", NULL, 0));
    nc_numeric("120.00", &total);
    params[0] = nc_int(2);
    params[1] = nc_text("B\xc3\xa9la");
    params[2] = total;
    params[3] = nc_date(nc_days_from_ymd(2024, 5, 3));
    nc_result_free(run(db, "INSERT INTO c_orders VALUES ($1, $2, $3, $4)", params, 4));
    nc_result_free(run(db, "UPDATE c_orders SET total = total * 0.9 WHERE id = 2", NULL, 0));
    nc_result_free(run(db, nc_status(db) == 2 ? "ROLLBACK" : "COMMIT", NULL, 0));

    r = run(db, "SELECT id, customer, total, placed FROM c_orders ORDER BY id", NULL, 0);
    if (r) {
        for (i = 0; i < nc_rows(r); i++) {
            for (j = 0; j < nc_columns(r); j++) {
                char buf[128];
                nc_format(nc_value_at(r, i, j), buf, sizeof buf);
                printf("%s%s=%s", j ? "  " : "", nc_column_name(r, j), buf);
            }
            printf("\n");
        }
        nc_result_free(r);
    }

    /* An error carries its SQLSTATE. */
    if (!nc_query(db, "INSERT INTO c_orders (id, customer) VALUES (1, 'dup')", NULL, 0, &err))
        printf("as expected: %s %s\n", err.code, err.message);

    nc_result_free(run(db, "DROP TABLE c_orders", NULL, 0));
    nc_close(db);
    return 0;
}
