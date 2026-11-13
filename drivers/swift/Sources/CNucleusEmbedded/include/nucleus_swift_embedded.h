/* A C driver connection to a database in the process (PLAN.md §M14.23):
 * nc_connect_io over the embedded edition's nce_send and nce_recv. */
#ifndef NUCLEUS_SWIFT_EMBEDDED_H
#define NUCLEUS_SWIFT_EMBEDDED_H

#include "nucleus.h"

/* The embedded edition's database (nucleus_embedded.h). */
struct NcDatabase;

/* A session on db, logged in as cfg's user (no password: there is no
 * login in the process).  NULL with err filled on failure. */
nc_conn *nc_connect_embedded(struct NcDatabase *db, const nc_config *cfg, nc_error *err);

#endif
