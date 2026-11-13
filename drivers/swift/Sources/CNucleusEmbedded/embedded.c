/* nc_connect_io over the embedded edition's session (nucleus_embedded.h). */
#include <stdio.h>

#include "nucleus_swift_embedded.h"
#include "NucleusEmbeddedC/nucleus_embedded.h"

static int io_send(void *context, const void *p, size_t n) {
    return nce_send((nce_connection *)context, (const uint8_t *)p, n);
}

static long io_recv(void *context, void *p, size_t n) {
    return (long)nce_recv((nce_connection *)context, (uint8_t *)p, n, -1);
}

static void io_close(void *context) { nce_disconnect((nce_connection *)context); }

nc_conn *nc_connect_embedded(struct NcDatabase *db, const nc_config *cfg, nc_error *err) {
    char *error = NULL;
    nce_connection *c = nce_connect(db, &error);
    nc_io io;
    if (!c) {
        if (err) {
            err->code[0] = 0;
            err->hint[0] = 0;
            snprintf(err->message, sizeof err->message, "%s", error ? error : "cannot start a session");
        }
        nce_free_error(error);
        return NULL;
    }
    io.context = c;
    io.send = io_send;
    io.recv = io_recv;
    io.close = io_close;
    return nc_connect_io(cfg, &io, err);
}
