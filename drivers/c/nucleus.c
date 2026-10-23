/* libnucleus: see nucleus.h. */
#if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)
#define _POSIX_C_SOURCE 200112L
#endif
#if defined(_WIN32) && !defined(_CRT_RAND_S)
#define _CRT_RAND_S /* rand_s, for SCRAM's nonce; before <stdlib.h> */
#endif

#include "nucleus.h"

#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef NC_WITH_OPENSSL
#include <openssl/err.h>
#include <openssl/ssl.h>
#include <openssl/x509v3.h>
#endif

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
typedef SOCKET nc_socket;
#define NC_BAD_SOCKET INVALID_SOCKET
#define nc_closesocket closesocket
#else
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>
typedef int nc_socket;
#define NC_BAD_SOCKET (-1)
#define nc_closesocket close
#endif

struct nc_conn {
    nc_socket sock;
#ifdef NC_WITH_OPENSSL
    SSL *ssl; /* NULL: plain TCP */
    SSL_CTX *ssl_ctx;
#endif
    int status;
    char server[128];
    uint32_t session;
};

typedef struct {
    char *name;
    nc_type type;
    nc_type element;
    char *type_name; /* NC_UUID, NC_EXT */
} nc_column;

struct nc_result {
    nc_column *columns;
    size_t ncolumns;
    nc_value *values; /* rows × columns */
    size_t nrows, cap_rows;
    char *command;
    uint64_t count;
};

/* ----------------------------------------------------------------- errors */

static void set_error(nc_error *err, const char *code, const char *fmt, ...) {
    va_list ap;
    if (!err) return;
    snprintf(err->code, sizeof err->code, "%s", code ? code : "");
    va_start(ap, fmt);
    vsnprintf(err->message, sizeof err->message, fmt, ap);
    va_end(ap);
}

/* ---------------------------------------------------------------- buffers */

typedef struct {
    unsigned char *b;
    size_t len, cap;
    int failed;
} buf;

static void put(buf *w, const void *p, size_t n) {
    if (w->failed) return;
    if (w->len + n > w->cap) {
        size_t cap = w->cap ? w->cap * 2 : 256;
        unsigned char *nb;
        while (cap < w->len + n) cap *= 2;
        nb = realloc(w->b, cap);
        if (!nb) {
            w->failed = 1;
            return;
        }
        w->b = nb;
        w->cap = cap;
    }
    memcpy(w->b + w->len, p, n);
    w->len += n;
}

static void put_u8(buf *w, unsigned v) {
    unsigned char c = (unsigned char)v;
    put(w, &c, 1);
}

static void put_le(buf *w, uint64_t v, int n) {
    unsigned char b[8];
    int i;
    for (i = 0; i < n; i++) b[i] = (unsigned char)(v >> (8 * i));
    put(w, b, (size_t)n);
}

static void put_str(buf *w, const char *s) {
    size_t n = s ? strlen(s) : 0;
    put_le(w, n, 4);
    put(w, s ? s : "", n);
}

static void put_ext(buf *w, const char *name, const void *data, size_t len) {
    put_u8(w, NC_EXT);
    put_str(w, name);
    put_le(w, len, 4);
    put(w, data, len);
}

static int put_value(buf *w, const nc_value *v) {
    size_t i;
    if (v->type == NC_UUID) {
        put_ext(w, "uuid", v->as.uuid.bytes, 16);
        return 0;
    }
    if (v->type == NC_EXT) {
        put_ext(w, v->as.ext.type_name, v->as.ext.data, v->as.ext.len);
        return 0;
    }
    put_u8(w, (unsigned)v->type);
    switch (v->type) {
    case NC_NULL: break;
    case NC_BOOL: put_u8(w, v->as.boolean ? 1 : 0); break;
    case NC_INT: put_le(w, (uint64_t)v->as.integer, 8); break;
    case NC_SMALLINT: put_le(w, (uint16_t)v->as.integer, 2); break;
    case NC_INTEGER: put_le(w, (uint32_t)v->as.integer, 4); break;
    case NC_REAL: {
        uint32_t bits;
        memcpy(&bits, &v->as.real4, 4);
        put_le(w, bits, 4);
        break;
    }
    case NC_BYTEA:
        put_le(w, v->as.bytes.len, 4);
        put(w, v->as.bytes.ptr, v->as.bytes.len);
        break;
    case NC_TIME: put_le(w, (uint64_t)v->as.time, 8); break;
    case NC_INET:
    case NC_CIDR:
        put_u8(w, v->as.net.family);
        put_u8(w, v->as.net.bits);
        put(w, v->as.net.addr, v->as.net.family == 4 ? 4 : 16);
        break;
    case NC_FLOAT: {
        uint64_t bits;
        memcpy(&bits, &v->as.real, 8);
        put_le(w, bits, 8);
        break;
    }
    case NC_TEXT:
        put_le(w, v->as.text.len, 4);
        put(w, v->as.text.ptr, v->as.text.len);
        break;
    case NC_NUMERIC:
        put_le(w, v->as.numeric.lo, 8);
        put_le(w, (uint64_t)v->as.numeric.hi, 8);
        put_u8(w, v->as.numeric.scale);
        break;
    case NC_DATE: put_le(w, (uint32_t)v->as.date, 4); break;
    case NC_TIMESTAMP:
    case NC_TIMESTAMPTZ: put_le(w, (uint64_t)v->as.timestamp, 8); break;
    case NC_INTERVAL:
        put_le(w, (uint32_t)v->as.interval.months, 4);
        put_le(w, (uint32_t)v->as.interval.days, 4);
        put_le(w, (uint64_t)v->as.interval.micros, 8);
        break;
    case NC_ARRAY:
        put_le(w, v->as.array.len, 4);
        for (i = 0; i < v->as.array.len; i++)
            if (put_value(w, &v->as.array.items[i]) != 0) return -1;
        break;
    default: return -1;
    }
    return 0;
}

typedef struct {
    const unsigned char *b;
    size_t len, at;
    int bad;
} reader;

static const unsigned char *take(reader *r, size_t n) {
    const unsigned char *p;
    if (r->bad || r->at + n > r->len) {
        r->bad = 1;
        return NULL;
    }
    p = r->b + r->at;
    r->at += n;
    return p;
}

static uint64_t get_le(reader *r, int n) {
    const unsigned char *p = take(r, (size_t)n);
    uint64_t v = 0;
    int i;
    if (!p) return 0;
    for (i = 0; i < n; i++) v |= (uint64_t)p[i] << (8 * i);
    return v;
}

static char *get_str(reader *r) {
    size_t n = (size_t)get_le(r, 4);
    const unsigned char *p = take(r, n);
    char *s;
    if (!p) return NULL;
    s = malloc(n + 1);
    if (!s) {
        r->bad = 1;
        return NULL;
    }
    memcpy(s, p, n);
    s[n] = 0;
    return s;
}

static void free_value(nc_value *v) {
    size_t i;
    if (v->type == NC_TEXT) free((void *)v->as.text.ptr);
    if (v->type == NC_BYTEA) free((void *)v->as.bytes.ptr);
    if (v->type == NC_EXT) {
        free((void *)v->as.ext.type_name);
        free((void *)v->as.ext.data);
    }
    if (v->type == NC_ARRAY) {
        for (i = 0; i < v->as.array.len; i++) free_value((nc_value *)&v->as.array.items[i]);
        free((void *)v->as.array.items);
    }
    v->type = NC_NULL;
}

static int get_value(reader *r, nc_value *v) {
    unsigned tag = (unsigned)get_le(r, 1);
    memset(v, 0, sizeof *v);
    v->type = (nc_type)tag;
    switch (tag) {
    case NC_NULL: break;
    case NC_BOOL: v->as.boolean = get_le(r, 1) != 0; break;
    case NC_INT: v->as.integer = (int64_t)get_le(r, 8); break;
    case NC_SMALLINT: v->as.integer = (int16_t)(uint16_t)get_le(r, 2); break;
    case NC_INTEGER: v->as.integer = (int32_t)(uint32_t)get_le(r, 4); break;
    case NC_REAL: {
        uint32_t bits = (uint32_t)get_le(r, 4);
        memcpy(&v->as.real4, &bits, 4);
        break;
    }
    case NC_BYTEA: {
        size_t n = (size_t)get_le(r, 4);
        const unsigned char *p = take(r, n);
        uint8_t *data;
        if (!p) return -1;
        data = malloc(n ? n : 1);
        if (!data) return -1;
        memcpy(data, p, n);
        v->as.bytes.ptr = data;
        v->as.bytes.len = n;
        break;
    }
    case NC_TIME: v->as.time = (int64_t)get_le(r, 8); break;
    case NC_INET:
    case NC_CIDR: {
        const unsigned char *p;
        v->as.net.family = (uint8_t)get_le(r, 1);
        v->as.net.bits = (uint8_t)get_le(r, 1);
        if (v->as.net.family != 4 && v->as.net.family != 6) return -1;
        p = take(r, v->as.net.family == 4 ? 4 : 16);
        if (!p) return -1;
        memcpy(v->as.net.addr, p, v->as.net.family == 4 ? 4 : 16);
        break;
    }
    case NC_FLOAT: {
        uint64_t bits = get_le(r, 8);
        memcpy(&v->as.real, &bits, 8);
        break;
    }
    case NC_TEXT: {
        size_t n = (size_t)get_le(r, 4);
        const unsigned char *p = take(r, n);
        char *s;
        if (!p) return -1;
        s = malloc(n + 1);
        if (!s) return -1;
        memcpy(s, p, n);
        s[n] = 0;
        v->as.text.ptr = s;
        v->as.text.len = n;
        break;
    }
    case NC_NUMERIC:
        v->as.numeric.lo = get_le(r, 8);
        v->as.numeric.hi = (int64_t)get_le(r, 8);
        v->as.numeric.scale = (uint8_t)get_le(r, 1);
        break;
    case NC_DATE: v->as.date = (int32_t)(uint32_t)get_le(r, 4); break;
    case NC_TIMESTAMP:
    case NC_TIMESTAMPTZ: v->as.timestamp = (int64_t)get_le(r, 8); break;
    case NC_INTERVAL:
        v->as.interval.months = (int32_t)(uint32_t)get_le(r, 4);
        v->as.interval.days = (int32_t)(uint32_t)get_le(r, 4);
        v->as.interval.micros = (int64_t)get_le(r, 8);
        break;
    case NC_EXT: {
        char *name = get_str(r);
        size_t n = (size_t)get_le(r, 4);
        const unsigned char *p = take(r, n);
        uint8_t *data;
        if (!name || !p) {
            free(name);
            return -1;
        }
        if (strcmp(name, "uuid") == 0 && n == 16) {
            free(name);
            v->type = NC_UUID;
            memcpy(v->as.uuid.bytes, p, 16);
            break;
        }
        data = malloc(n ? n : 1);
        if (!data) {
            free(name);
            return -1;
        }
        memcpy(data, p, n);
        v->as.ext.type_name = name;
        v->as.ext.data = data;
        v->as.ext.len = n;
        break;
    }
    case NC_ARRAY: {
        size_t n = (size_t)get_le(r, 4), i;
        nc_value *items;
        if (r->bad || n > r->len) return -1;
        items = calloc(n ? n : 1, sizeof *items);
        if (!items) return -1;
        v->as.array.items = items;
        v->as.array.len = 0;
        for (i = 0; i < n; i++) {
            if (get_value(r, &items[i]) != 0) return -1;
            v->as.array.len = i + 1;
        }
        break;
    }
    default: return -1;
    }
    return r->bad ? -1 : 0;
}

/* ------------------------------------------------------- SCRAM-SHA-256 */

/* SHA-256 (FIPS 180-4), HMAC (RFC 2104), PBKDF2 (RFC 8018), base64: what
 * a SCRAM-SHA-256 login (RFC 5802) needs, without a library. */

typedef struct {
    uint32_t h[8];
    uint64_t len;
    unsigned char block[64];
    size_t fill;
} sha256_ctx;

static uint32_t ror(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }

static void sha256_block(sha256_ctx *c, const unsigned char *b) {
    static const uint32_t k[64] = {
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2};
    uint32_t w[64], v[8], t1, t2;
    int i;
    for (i = 0; i < 16; i++)
        w[i] = (uint32_t)b[4 * i] << 24 | (uint32_t)b[4 * i + 1] << 16 | (uint32_t)b[4 * i + 2] << 8 | b[4 * i + 3];
    for (i = 16; i < 64; i++)
        w[i] = (ror(w[i - 2], 17) ^ ror(w[i - 2], 19) ^ (w[i - 2] >> 10)) + w[i - 7] +
               (ror(w[i - 15], 7) ^ ror(w[i - 15], 18) ^ (w[i - 15] >> 3)) + w[i - 16];
    memcpy(v, c->h, sizeof v);
    for (i = 0; i < 64; i++) {
        t1 = v[7] + (ror(v[4], 6) ^ ror(v[4], 11) ^ ror(v[4], 25)) + ((v[4] & v[5]) ^ (~v[4] & v[6])) + k[i] + w[i];
        t2 = (ror(v[0], 2) ^ ror(v[0], 13) ^ ror(v[0], 22)) + ((v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]));
        memmove(v + 1, v, 7 * sizeof *v);
        v[4] += t1;
        v[0] = t1 + t2;
    }
    for (i = 0; i < 8; i++) c->h[i] += v[i];
}

static void sha256_init(sha256_ctx *c) {
    static const uint32_t h0[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                                   0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    memcpy(c->h, h0, sizeof h0);
    c->len = 0;
    c->fill = 0;
}

static void sha256_update(sha256_ctx *c, const void *data, size_t n) {
    const unsigned char *p = data;
    c->len += n;
    while (n--) {
        c->block[c->fill++] = *p++;
        if (c->fill == 64) {
            sha256_block(c, c->block);
            c->fill = 0;
        }
    }
}

static void sha256_final(sha256_ctx *c, unsigned char out[32]) {
    uint64_t bits = c->len * 8;
    unsigned char pad = 0x80, zero = 0, len[8];
    int i;
    sha256_update(c, &pad, 1);
    while (c->fill != 56) sha256_update(c, &zero, 1);
    for (i = 0; i < 8; i++) len[i] = (unsigned char)(bits >> (56 - 8 * i));
    sha256_update(c, len, 8);
    for (i = 0; i < 8; i++) {
        out[4 * i] = (unsigned char)(c->h[i] >> 24);
        out[4 * i + 1] = (unsigned char)(c->h[i] >> 16);
        out[4 * i + 2] = (unsigned char)(c->h[i] >> 8);
        out[4 * i + 3] = (unsigned char)c->h[i];
    }
}

static void sha256(const void *data, size_t n, unsigned char out[32]) {
    sha256_ctx c;
    sha256_init(&c);
    sha256_update(&c, data, n);
    sha256_final(&c, out);
}

static void hmac_sha256(const unsigned char *key, size_t klen, const void *data, size_t n, unsigned char out[32]) {
    unsigned char k[64] = {0}, pad[64], inner[32];
    sha256_ctx c;
    int i;
    if (klen > 64) sha256(key, klen, k);
    else memcpy(k, key, klen);
    for (i = 0; i < 64; i++) pad[i] = k[i] ^ 0x36;
    sha256_init(&c);
    sha256_update(&c, pad, 64);
    sha256_update(&c, data, n);
    sha256_final(&c, inner);
    for (i = 0; i < 64; i++) pad[i] = k[i] ^ 0x5c;
    sha256_init(&c);
    sha256_update(&c, pad, 64);
    sha256_update(&c, inner, 32);
    sha256_final(&c, out);
}

static void pbkdf2_sha256(const char *password, const unsigned char *salt, size_t slen, unsigned iterations,
                          unsigned char out[32]) {
    unsigned char *first = malloc(slen + 4), u[32];
    unsigned i, j;
    if (!first) {
        memset(out, 0, 32);
        return;
    }
    memcpy(first, salt, slen);
    memcpy(first + slen, "\0\0\0\1", 4);
    hmac_sha256((const unsigned char *)password, strlen(password), first, slen + 4, u);
    free(first);
    memcpy(out, u, 32);
    for (i = 1; i < iterations; i++) {
        hmac_sha256((const unsigned char *)password, strlen(password), u, 32, u);
        for (j = 0; j < 32; j++) out[j] ^= u[j];
    }
}

static const char B64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/* Base64 of n bytes into out (4·⌈n/3⌉ + 1 bytes). */
static void b64_encode(const unsigned char *b, size_t n, char *out) {
    size_t i, o = 0;
    for (i = 0; i < n; i += 3) {
        uint32_t v = (uint32_t)b[i] << 16 | (i + 1 < n ? (uint32_t)b[i + 1] << 8 : 0) | (i + 2 < n ? b[i + 2] : 0);
        out[o++] = B64[v >> 18 & 63];
        out[o++] = B64[v >> 12 & 63];
        out[o++] = i + 1 < n ? B64[v >> 6 & 63] : '=';
        out[o++] = i + 2 < n ? B64[v & 63] : '=';
    }
    out[o] = 0;
}

/* Decodes base64 text of length n into out (at most size bytes); the
 * length, or -1. */
static long b64_decode(const char *s, size_t n, unsigned char *out, size_t size) {
    size_t i, o = 0;
    if (n % 4) return -1;
    for (i = 0; i < n; i += 4) {
        uint32_t v = 0;
        int k, pad = 0;
        for (k = 0; k < 4; k++) {
            const char *at = strchr(B64, s[i + k]);
            if (s[i + k] == '=') pad++, v <<= 6;
            else if (!at || !s[i + k] || pad) return -1;
            else v = v << 6 | (uint32_t)(at - B64);
        }
        for (k = 0; k < 3 - pad; k++) {
            if (o >= size) return -1;
            out[o++] = (unsigned char)(v >> (16 - 8 * k));
        }
    }
    return (long)o;
}

static int random_bytes(unsigned char *b, size_t n) {
#ifdef _WIN32
    size_t i;
    for (i = 0; i < n; i++) {
        unsigned int r;
        if (rand_s(&r) != 0) return -1;
        b[i] = (unsigned char)r;
    }
    return 0;
#else
    FILE *f = fopen("/dev/urandom", "rb");
    size_t got;
    if (!f) return -1;
    got = fread(b, 1, n, f);
    fclose(f);
    return got == n ? 0 : -1;
#endif
}

/* The value of "k=" in a SCRAM message, copied into out; 0 if found. */
static int scram_attr(const char *msg, char key, char *out, size_t size) {
    const char *p = msg;
    while (p && *p) {
        if (p[0] == key && p[1] == '=') {
            const char *end = strchr(p, ',');
            size_t n = end ? (size_t)(end - p - 2) : strlen(p + 2);
            if (n >= size) return -1;
            memcpy(out, p + 2, n);
            out[n] = 0;
            return 0;
        }
        p = strchr(p, ',');
        if (p) p++;
    }
    return -1;
}

typedef struct {
    char bare[64];
    char auth[1024];
    unsigned char server_key[32];
    /* The GS2 header, and the channel binding it binds to (with "p="). */
    const char *gs2;
    unsigned char binding[64];
    unsigned binding_len;
} scram_state;

/* The client's first message. */
static int scram_first(scram_state *st, char *out, size_t size) {
    unsigned char nonce[18];
    char text[32];
    if (random_bytes(nonce, sizeof nonce) != 0) return -1;
    b64_encode(nonce, sizeof nonce, text);
    snprintf(st->bare, sizeof st->bare, "n=,r=%s", text);
    return snprintf(out, size, "%s%s", st->gs2, st->bare) < (int)size ? 0 : -1;
}

/* The server's first message → the client's final one. */
static int scram_respond(scram_state *st, const char *password, const char *server_first, char *out, size_t size) {
    char nonce[256], salt64[256], iter[16], without[400], c64[160];
    unsigned char c[96];
    size_t clen = strlen(st->gs2);
    unsigned char salt[192], salted[32], client_key[32], stored[32], sig[32], proof[32];
    char proof64[48];
    long slen;
    int i;
    if (scram_attr(server_first, 'r', nonce, sizeof nonce) || scram_attr(server_first, 's', salt64, sizeof salt64) ||
        scram_attr(server_first, 'i', iter, sizeof iter))
        return -1;
    if (strncmp(nonce, st->bare + 5, strlen(st->bare + 5)) != 0 || strlen(nonce) == strlen(st->bare + 5)) return -1;
    slen = b64_decode(salt64, strlen(salt64), salt, sizeof salt);
    if (slen < 0 || atoi(iter) <= 0) return -1;
    pbkdf2_sha256(password, salt, (size_t)slen, (unsigned)atoi(iter), salted);
    hmac_sha256(salted, 32, "Client Key", 10, client_key);
    sha256(client_key, 32, stored);
    hmac_sha256(salted, 32, "Server Key", 10, st->server_key);
    /* c= repeats the GS2 header, and the channel binding when bound. */
    memcpy(c, st->gs2, clen);
    memcpy(c + clen, st->binding, st->binding_len);
    b64_encode(c, clen + st->binding_len, c64);
    snprintf(without, sizeof without, "c=%s,r=%s", c64, nonce);
    if (snprintf(st->auth, sizeof st->auth, "%s,%s,%s", st->bare, server_first, without) >= (int)sizeof st->auth)
        return -1;
    hmac_sha256(stored, 32, st->auth, strlen(st->auth), sig);
    for (i = 0; i < 32; i++) proof[i] = client_key[i] ^ sig[i];
    b64_encode(proof, 32, proof64);
    return snprintf(out, size, "%s,p=%s", without, proof64) < (int)size ? 0 : -1;
}

/* Checks the server's final message ("v=…"): 0 if it signed right. */
static int scram_verify(const scram_state *st, const char *server_final) {
    unsigned char want[32], got[48];
    long n;
    unsigned diff = 0;
    int i;
    if (strncmp(server_final, "v=", 2) != 0) return -1;
    n = b64_decode(server_final + 2, strlen(server_final + 2), got, sizeof got);
    if (n != 32) return -1;
    hmac_sha256(st->server_key, 32, st->auth, strlen(st->auth), want);
    for (i = 0; i < 32; i++) diff |= (unsigned)(want[i] ^ got[i]);
    return diff ? -1 : 0;
}

/* Whether the space-separated list has the word. */
static int has_word(const char *list, const char *word) {
    size_t n = strlen(word);
    const char *p = list;
    while ((p = strstr(p, word)) != NULL) {
        if ((p == list || p[-1] == ' ') && (p[n] == ' ' || p[n] == 0)) return 1;
        p += n;
    }
    return 0;
}

/* ---------------------------------------------------------------- network */

static int send_all(nc_conn *c, const unsigned char *p, size_t n) {
    while (n > 0) {
#ifdef NC_WITH_OPENSSL
        if (c->ssl) {
            int k = SSL_write(c->ssl, p, n > INT_MAX ? INT_MAX : (int)n);
            if (k <= 0) return -1;
            p += k;
            n -= (size_t)k;
            continue;
        }
#endif
#ifdef _WIN32
        int k = send(c->sock, (const char *)p, (int)n, 0);
#else
        ssize_t k = send(c->sock, p, n, 0);
#endif
        if (k <= 0) return -1;
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

static int recv_all(nc_conn *c, unsigned char *p, size_t n) {
    while (n > 0) {
#ifdef NC_WITH_OPENSSL
        if (c->ssl) {
            int k = SSL_read(c->ssl, p, n > INT_MAX ? INT_MAX : (int)n);
            if (k <= 0) return -1;
            p += k;
            n -= (size_t)k;
            continue;
        }
#endif
#ifdef _WIN32
        int k = recv(c->sock, (char *)p, (int)n, 0);
#else
        ssize_t k = recv(c->sock, p, n, 0);
#endif
        if (k <= 0) return -1;
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

static int send_frame(nc_conn *c, char kind, const buf *body) {
    unsigned char h[5];
    size_t n = body ? body->len : 0;
    int i;
    h[0] = (unsigned char)kind;
    for (i = 0; i < 4; i++) h[1 + i] = (unsigned char)(n >> (8 * i));
    if (send_all(c, h, 5) != 0) return -1;
    return n ? send_all(c, body->b, n) : 0;
}

/* Reads one frame; the body is malloc'd (free it). */
static int read_frame(nc_conn *c, char *kind, unsigned char **body, size_t *len) {
    unsigned char h[5];
    size_t n;
    if (recv_all(c, h, 5) != 0) return -1;
    n = (size_t)h[1] | (size_t)h[2] << 8 | (size_t)h[3] << 16 | (size_t)h[4] << 24;
    if (n > (1u << 30)) return -1;
    *body = malloc(n ? n : 1);
    if (!*body) return -1;
    if (n && recv_all(c, *body, n) != 0) {
        free(*body);
        return -1;
    }
    *kind = (char)h[0];
    *len = n;
    return 0;
}

int nc_tls_supported(void) {
#ifdef NC_WITH_OPENSSL
    return 1;
#else
    return 0;
#endif
}

/* Frees a connection and its socket (and TLS session). */
static void drop_conn(nc_conn *c) {
#ifdef NC_WITH_OPENSSL
    if (c->ssl) {
        SSL_shutdown(c->ssl);
        SSL_free(c->ssl);
    }
    if (c->ssl_ctx) SSL_CTX_free(c->ssl_ctx);
#endif
    if (c->sock != NC_BAD_SOCKET) nc_closesocket(c->sock);
    free(c);
}

/* The TLS session's tls-server-end-point value (RFC 5929: the server's
 * certificate hashed by its signature's hash, SHA-256 for MD5 and SHA-1)
 * into out (64 bytes); -1 without TLS or a hash. */
static int end_point(nc_conn *c, unsigned char *out, unsigned *len) {
#ifdef NC_WITH_OPENSSL
    X509 *cert;
    int md = NID_undef, pk = NID_undef, rc = -1;
    const EVP_MD *digest;
    if (!c->ssl) return -1;
#if OPENSSL_VERSION_NUMBER >= 0x30000000L
    cert = SSL_get1_peer_certificate(c->ssl);
#else
    cert = SSL_get_peer_certificate(c->ssl);
#endif
    if (!cert) return -1;
    if (OBJ_find_sigid_algs(X509_get_signature_nid(cert), &md, &pk)) {
        if (md == NID_md5 || md == NID_sha1) md = NID_sha256;
        digest = md == NID_undef ? NULL : EVP_get_digestbynid(md);
        if (digest && EVP_MD_size(digest) <= 64 && X509_digest(cert, digest, out, len) == 1) rc = 0;
    }
    X509_free(cert);
    return rc;
#else
    (void)c;
    (void)out;
    (void)len;
    return -1;
#endif
}

/* Starts TLS on c's socket, as the client: the server sees the handshake
 * (DOCS §10).  0, or -1 with err set. */
static int start_tls(nc_conn *c, const nc_config *cfg, const char *host, nc_error *err) {
#ifdef NC_WITH_OPENSSL
    unsigned char ip[16];
    int is_ip = inet_pton(AF_INET, host, ip) == 1 || inet_pton(AF_INET6, host, ip) == 1;
    long verified;
    c->ssl_ctx = SSL_CTX_new(TLS_client_method());
    if (!c->ssl_ctx || SSL_CTX_set_min_proto_version(c->ssl_ctx, TLS1_2_VERSION) != 1) {
        set_error(err, "", "TLS: cannot set up OpenSSL");
        return -1;
    }
    if (!cfg->tls_no_verify) {
        SSL_CTX_set_verify(c->ssl_ctx, SSL_VERIFY_PEER, NULL);
        if ((cfg->tls_ca ? SSL_CTX_load_verify_locations(c->ssl_ctx, cfg->tls_ca, NULL)
                         : SSL_CTX_set_default_verify_paths(c->ssl_ctx)) != 1) {
            set_error(err, "", "TLS: cannot load CA certificates%s%s", cfg->tls_ca ? " from " : "",
                      cfg->tls_ca ? cfg->tls_ca : "");
            return -1;
        }
    }
    if (cfg->tls_cert &&
        (SSL_CTX_use_certificate_chain_file(c->ssl_ctx, cfg->tls_cert) != 1 ||
         SSL_CTX_use_PrivateKey_file(c->ssl_ctx, cfg->tls_key ? cfg->tls_key : cfg->tls_cert, SSL_FILETYPE_PEM) != 1 ||
         SSL_CTX_check_private_key(c->ssl_ctx) != 1)) {
        set_error(err, "", "TLS: cannot use the client certificate %s", cfg->tls_cert);
        return -1;
    }
    c->ssl = SSL_new(c->ssl_ctx);
    if (!c->ssl || SSL_set_fd(c->ssl, (int)c->sock) != 1) {
        set_error(err, "", "TLS: cannot set up OpenSSL");
        return -1;
    }
    if (!cfg->tls_no_verify) {
        /* The certificate must name the host connected to. */
        X509_VERIFY_PARAM *param = SSL_get0_param(c->ssl);
        if (is_ip ? X509_VERIFY_PARAM_set1_ip_asc(param, host) != 1
                  : X509_VERIFY_PARAM_set1_host(param, host, 0) != 1) {
            set_error(err, "", "TLS: bad host %s", host);
            return -1;
        }
    }
    if (!is_ip) SSL_set_tlsext_host_name(c->ssl, host);
    if (SSL_connect(c->ssl) != 1) {
        verified = SSL_get_verify_result(c->ssl);
        if (verified != X509_V_OK) {
            set_error(err, "", "TLS: %s", X509_verify_cert_error_string(verified));
        } else {
            char why[256];
            ERR_error_string_n(ERR_get_error(), why, sizeof why);
            set_error(err, "", "TLS handshake failed: %s", why);
        }
        return -1;
    }
    return 0;
#else
    (void)c;
    (void)cfg;
    (void)host;
    set_error(err, "", "TLS: this libnucleus was built without it (build with NC_WITH_OPENSSL)");
    return -1;
#endif
}

nc_config nc_default_config(void) {
    nc_config cfg;
    memset(&cfg, 0, sizeof cfg);
    cfg.host = "127.0.0.1";
    cfg.port = NC_DEFAULT_PORT;
    cfg.user = "nucleus";
    cfg.password = "nucleus";
    cfg.database = "nucleus";
    cfg.client_name = "libnucleus";
    return cfg;
}

static void server_error(nc_error *err, const unsigned char *body, size_t len) {
    reader r = {body, len, 0, 0};
    char *code = get_str(&r), *msg = get_str(&r);
    set_error(err, code ? code : "", "%s", msg ? msg : "unknown error");
    free(code);
    free(msg);
}

nc_conn *nc_connect(const nc_config *cfg, nc_error *err) {
    nc_config d = nc_default_config();
    struct addrinfo hints, *res = NULL, *ai;
    char port[16];
    nc_conn *c;
    buf hello = {0};
    int one = 1, rc, scram_started = 0;
    scram_state scram;
    if (!cfg) cfg = &d;
#ifdef _WIN32
    {
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
            set_error(err, "", "WSAStartup failed");
            return NULL;
        }
    }
#endif
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    snprintf(port, sizeof port, "%u", (unsigned)(cfg->port ? cfg->port : NC_DEFAULT_PORT));
    rc = getaddrinfo(cfg->host ? cfg->host : d.host, port, &hints, &res);
    if (rc != 0) {
        set_error(err, "", "cannot resolve %s", cfg->host ? cfg->host : d.host);
        return NULL;
    }
    c = calloc(1, sizeof *c);
    if (!c) {
        freeaddrinfo(res);
        set_error(err, "", "out of memory");
        return NULL;
    }
    c->sock = NC_BAD_SOCKET;
    for (ai = res; ai; ai = ai->ai_next) {
        c->sock = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (c->sock == NC_BAD_SOCKET) continue;
        if (connect(c->sock, ai->ai_addr, (int)ai->ai_addrlen) == 0) break;
        nc_closesocket(c->sock);
        c->sock = NC_BAD_SOCKET;
    }
    freeaddrinfo(res);
    if (c->sock == NC_BAD_SOCKET) {
        set_error(err, "", "cannot connect to %s:%s", cfg->host ? cfg->host : d.host, port);
        free(c);
        return NULL;
    }
    setsockopt(c->sock, IPPROTO_TCP, TCP_NODELAY, (const char *)&one, sizeof one);
    if ((cfg->tls || cfg->tls_ca) && start_tls(c, cfg, cfg->host ? cfg->host : d.host, err) != 0) {
        drop_conn(c);
        return NULL;
    }
    put_le(&hello, NC_PROTOCOL_VERSION, 2);
    put_str(&hello, cfg->user ? cfg->user : d.user);
    put_str(&hello, cfg->database ? cfg->database : d.database);
    put_str(&hello, ""); /* version 4: the password is proved, not sent */
    put_str(&hello, cfg->client_name ? cfg->client_name : d.client_name);
    rc = hello.failed ? -1 : send_frame(c, 'H', &hello);
    free(hello.b);
    while (rc == 0) {
        char kind;
        unsigned char *body;
        size_t len;
        if (read_frame(c, &kind, &body, &len) != 0) break;
        if (kind == 'A') {
            reader r = {body, len, 0, 0};
            char *msg = get_str(&r), reply[1024];
            int ok = msg != NULL, send = 1;
            if (ok && !scram_started) {
                /* Over TLS: bound to the session if the server offers
                 * SCRAM-SHA-256-PLUS, else "y" (it could have been). */
                scram.gs2 = "n,,";
                scram.binding_len = 0;
                if (end_point(c, scram.binding, &scram.binding_len) == 0) {
                    scram.gs2 = has_word(msg, "SCRAM-SHA-256-PLUS") ? "p=tls-server-end-point,," : "y,,";
                    if (scram.gs2[0] == 'y') scram.binding_len = 0;
                }
                ok = has_word(msg, "SCRAM-SHA-256") && scram_first(&scram, reply, sizeof reply) == 0;
                scram_started = 1;
            } else if (ok && strncmp(msg, "v=", 2) == 0) {
                ok = scram_verify(&scram, msg) == 0;
                send = 0;
            } else if (ok) {
                ok = scram_respond(&scram, cfg->password ? cfg->password : "", msg, reply, sizeof reply) == 0;
            }
            free(msg);
            free(body);
            if (!ok) {
                set_error(err, "08006", "the SCRAM-SHA-256 login failed (or the server could not prove it knows the password)");
                drop_conn(c);
                return NULL;
            }
            if (send) {
                buf a = {0};
                put_str(&a, reply);
                rc = a.failed ? -1 : send_frame(c, 'a', &a);
                free(a.b);
            }
            continue;
        }
        if (kind == 'W') {
            reader r = {body, len, 0, 0};
            char *server;
            get_le(&r, 2);
            server = get_str(&r);
            snprintf(c->server, sizeof c->server, "%s", server ? server : "");
            free(server);
            c->session = (uint32_t)get_le(&r, 4);
        } else if (kind == 'R') {
            c->status = len ? body[0] : 0;
            free(body);
            return c;
        } else if (kind == 'E') {
            server_error(err, body, len);
            free(body);
            drop_conn(c);
            return NULL;
        } else {
            free(body);
            break;
        }
        free(body);
    }
    set_error(err, "08006", "the connection failed during login");
    drop_conn(c);
    return NULL;
}

void nc_result_free(nc_result *r) {
    size_t i;
    if (!r) return;
    for (i = 0; i < r->nrows * r->ncolumns; i++) free_value(&r->values[i]);
    for (i = 0; i < r->ncolumns; i++) {
        free(r->columns[i].name);
        free(r->columns[i].type_name);
    }
    free(r->columns);
    free(r->values);
    free(r->command);
    free(r);
}

nc_result *nc_query(nc_conn *c, const char *sql, const nc_value *params, size_t nparams, nc_error *err) {
    buf q = {0};
    size_t i;
    nc_result *cur = NULL, *last = NULL;
    int failed = 0;
    put_str(&q, sql);
    put_u8(&q, 0);
    put_le(&q, nparams, 2);
    for (i = 0; i < nparams; i++) {
        if (put_value(&q, &params[i]) != 0) {
            free(q.b);
            set_error(err, "", "parameter %u has an unknown type", (unsigned)i + 1);
            return NULL;
        }
    }
    if (q.failed || send_frame(c, 'Q', &q) != 0) {
        free(q.b);
        set_error(err, "08006", "connection lost");
        return NULL;
    }
    free(q.b);
    for (;;) {
        char kind;
        unsigned char *body;
        size_t len;
        reader r;
        if (read_frame(c, &kind, &body, &len) != 0) {
            nc_result_free(cur);
            nc_result_free(last);
            set_error(err, "08006", "connection lost");
            return NULL;
        }
        r.b = body;
        r.len = len;
        r.at = 0;
        r.bad = 0;
        switch (kind) {
        case 'C': {
            size_t n = (size_t)get_le(&r, 2);
            nc_result_free(cur);
            cur = calloc(1, sizeof *cur);
            if (cur) cur->columns = calloc(n ? n : 1, sizeof *cur->columns);
            if (!cur || !cur->columns) break;
            for (i = 0; i < n && !r.bad; i++) {
                cur->columns[i].name = get_str(&r);
                cur->columns[i].type = (nc_type)get_le(&r, 1);
                if (cur->columns[i].type == NC_ARRAY) cur->columns[i].element = (nc_type)get_le(&r, 1);
                if (cur->columns[i].type == NC_ENUM) cur->columns[i].type_name = get_str(&r);
                if (cur->columns[i].type == NC_EXT) {
                    cur->columns[i].type_name = get_str(&r);
                    if (cur->columns[i].type_name && strcmp(cur->columns[i].type_name, "uuid") == 0)
                        cur->columns[i].type = NC_UUID;
                }
                cur->ncolumns = i + 1;
            }
            break;
        }
        case 'D':
            if (!cur) break;
            if (cur->nrows == cur->cap_rows) {
                size_t cap = cur->cap_rows ? cur->cap_rows * 2 : 16;
                nc_value *nv = realloc(cur->values, cap * (cur->ncolumns ? cur->ncolumns : 1) * sizeof *nv);
                if (!nv) break;
                cur->values = nv;
                cur->cap_rows = cap;
            }
            for (i = 0; i < cur->ncolumns; i++) {
                nc_value *v = &cur->values[cur->nrows * cur->ncolumns + i];
                if (get_value(&r, v) != 0) {
                    free_value(v);
                    r.bad = 1;
                    for (; i + 1 < cur->ncolumns; i++) cur->values[cur->nrows * cur->ncolumns + i + 1].type = NC_NULL;
                    break;
                }
            }
            cur->nrows++;
            break;
        case 'K':
            if (!cur) cur = calloc(1, sizeof *cur);
            if (cur) {
                cur->command = get_str(&r);
                cur->count = get_le(&r, 8);
            }
            nc_result_free(last);
            last = cur;
            cur = NULL;
            break;
        case 'E':
            server_error(err, body, len);
            failed = 1;
            break;
        case 'R':
            c->status = len ? body[0] : 0;
            free(body);
            nc_result_free(cur);
            if (failed) {
                nc_result_free(last);
                return NULL;
            }
            if (!last) set_error(err, "", "no result");
            return last;
        default:
            r.bad = 1;
        }
        free(body);
        if (r.bad) {
            nc_result_free(cur);
            nc_result_free(last);
            set_error(err, "08P01", "malformed message from the server");
            return NULL;
        }
    }
}

int nc_status(const nc_conn *c) { return c->status; }
const char *nc_server_version(const nc_conn *c) { return c->server; }
uint32_t nc_session_id(const nc_conn *c) { return c->session; }

void nc_close(nc_conn *c) {
    if (!c) return;
    send_frame(c, 'X', NULL);
    drop_conn(c);
}

size_t nc_columns(const nc_result *r) { return r->ncolumns; }
const char *nc_column_name(const nc_result *r, size_t col) { return col < r->ncolumns ? r->columns[col].name : NULL; }
nc_type nc_column_type(const nc_result *r, size_t col) { return col < r->ncolumns ? r->columns[col].type : NC_NULL; }
nc_type nc_column_element(const nc_result *r, size_t col) { return col < r->ncolumns ? r->columns[col].element : NC_NULL; }
const char *nc_column_type_name(const nc_result *r, size_t col) { return col < r->ncolumns ? r->columns[col].type_name : NULL; }
size_t nc_rows(const nc_result *r) { return r->nrows; }
const nc_value *nc_value_at(const nc_result *r, size_t row, size_t col) {
    return row < r->nrows && col < r->ncolumns ? &r->values[row * r->ncolumns + col] : NULL;
}
const char *nc_command(const nc_result *r) { return r->command ? r->command : ""; }
uint64_t nc_count(const nc_result *r) { return r->count; }

/* ----------------------------------------------------------------- values */

nc_value nc_null(void) { nc_value v; memset(&v, 0, sizeof v); return v; }
nc_value nc_bool(int b) { nc_value v = nc_null(); v.type = NC_BOOL; v.as.boolean = b != 0; return v; }
nc_value nc_int(int64_t i) { nc_value v = nc_null(); v.type = NC_INT; v.as.integer = i; return v; }
nc_value nc_float(double f) { nc_value v = nc_null(); v.type = NC_FLOAT; v.as.real = f; return v; }
nc_value nc_smallint(int16_t i) { nc_value v = nc_null(); v.type = NC_SMALLINT; v.as.integer = i; return v; }
nc_value nc_integer(int32_t i) { nc_value v = nc_null(); v.type = NC_INTEGER; v.as.integer = i; return v; }
nc_value nc_real(float f) { nc_value v = nc_null(); v.type = NC_REAL; v.as.real4 = f; return v; }
nc_value nc_bytea(const uint8_t *data, size_t len) {
    nc_value v = nc_null();
    v.type = NC_BYTEA;
    v.as.bytes.ptr = data;
    v.as.bytes.len = len;
    return v;
}
nc_value nc_time(int64_t us) { nc_value v = nc_null(); v.type = NC_TIME; v.as.time = us; return v; }
static nc_value net_value(nc_type t, int family, const uint8_t *addr, int bits) {
    nc_value v = nc_null();
    v.type = t;
    v.as.net.family = (uint8_t)(family == 6 ? 6 : 4);
    v.as.net.bits = (uint8_t)bits;
    memcpy(v.as.net.addr, addr, family == 6 ? 16 : 4);
    return v;
}
nc_value nc_inet(int family, const uint8_t *addr, int bits) { return net_value(NC_INET, family, addr, bits); }
nc_value nc_cidr(int family, const uint8_t *addr, int bits) { return net_value(NC_CIDR, family, addr, bits); }

static int hex_digit(char c);

static int parse_v4(const char *s, size_t n, uint8_t out[4]) {
    size_t i = 0;
    int k;
    for (k = 0; k < 4; k++) {
        unsigned x = 0, digits = 0;
        while (i < n && s[i] >= '0' && s[i] <= '9' && digits < 3) x = x * 10 + (unsigned)(s[i++] - '0'), digits++;
        if (!digits || x > 255) return -1;
        out[k] = (uint8_t)x;
        if (k < 3 && (i >= n || s[i++] != '.')) return -1;
    }
    return i == n ? 0 : -1;
}

int nc_inet_parse(const char *text, nc_value *out) {
    const char *slash = strchr(text, '/');
    size_t n = slash ? (size_t)(slash - text) : strlen(text);
    uint8_t addr[16] = {0};
    int family, bits;
    if (!memchr(text, ':', n)) {
        if (parse_v4(text, n, addr) != 0) return -1;
        family = 4;
    } else {
        /* Groups before and after "::", and an IPv4 tail. */
        uint16_t head[8], tail[8];
        size_t nh = 0, nt = 0, i = 0;
        int gap = 0;
        uint16_t *g = head;
        size_t *ng = &nh;
        if (n >= 2 && text[0] == ':' && text[1] == ':') {
            gap = 1;
            g = tail;
            ng = &nt;
            i = 2;
        }
        while (i < n) {
            size_t j = i;
            unsigned x = 0;
            while (j < n && text[j] != ':' && text[j] != '.') j++;
            if (j < n && text[j] == '.') {
                uint8_t v4[4];
                size_t end = i;
                while (end < n && text[end] != ':') end++;
                if (end != n || *ng > 6 || parse_v4(text + i, n - i, v4) != 0) return -1;
                g[(*ng)++] = (uint16_t)(v4[0] << 8 | v4[1]);
                g[(*ng)++] = (uint16_t)(v4[2] << 8 | v4[3]);
                i = n;
                break;
            }
            if (j == i || j - i > 4 || *ng >= 8) return -1;
            for (; i < j; i++) {
                int d = hex_digit(text[i]);
                if (d < 0) return -1;
                x = x * 16 + (unsigned)d;
            }
            g[(*ng)++] = (uint16_t)x;
            if (i < n) {
                i++; /* ':' */
                if (i < n && text[i] == ':') {
                    if (gap) return -1;
                    gap = 1;
                    g = tail;
                    ng = &nt;
                    i++;
                } else if (i == n) {
                    return -1;
                }
            }
        }
        if (gap ? nh + nt > 7 : nh != 8) return -1;
        for (i = 0; i < nh; i++) addr[2 * i] = (uint8_t)(head[i] >> 8), addr[2 * i + 1] = (uint8_t)head[i];
        for (i = 0; i < nt; i++) {
            size_t at = 8 - nt + i;
            addr[2 * at] = (uint8_t)(tail[i] >> 8);
            addr[2 * at + 1] = (uint8_t)tail[i];
        }
        family = 6;
    }
    bits = family == 4 ? 32 : 128;
    if (slash) {
        char *end;
        long b = strtol(slash + 1, &end, 10);
        if (*end || end == slash + 1 || b < 0 || b > bits) return -1;
        bits = (int)b;
    }
    *out = nc_inet(family, addr, bits);
    return 0;
}
nc_value nc_text(const char *s) {
    nc_value v = nc_null();
    if (!s) return v;
    v.type = NC_TEXT;
    v.as.text.ptr = s;
    v.as.text.len = strlen(s);
    return v;
}
nc_value nc_date(int32_t days) { nc_value v = nc_null(); v.type = NC_DATE; v.as.date = days; return v; }
nc_value nc_timestamp(int64_t us) { nc_value v = nc_null(); v.type = NC_TIMESTAMP; v.as.timestamp = us; return v; }
nc_value nc_timestamptz(int64_t us) { nc_value v = nc_null(); v.type = NC_TIMESTAMPTZ; v.as.timestamp = us; return v; }
nc_value nc_interval(int32_t months, int32_t days, int64_t micros) {
    nc_value v = nc_null();
    v.type = NC_INTERVAL;
    v.as.interval.months = months;
    v.as.interval.days = days;
    v.as.interval.micros = micros;
    return v;
}
nc_value nc_array(const nc_value *items, size_t len) {
    nc_value v = nc_null();
    v.type = NC_ARRAY;
    v.as.array.items = items;
    v.as.array.len = len;
    return v;
}

nc_value nc_uuid_value(nc_uuid u) {
    nc_value v = nc_null();
    v.type = NC_UUID;
    v.as.uuid = u;
    return v;
}

nc_value nc_ext(const char *type_name, const uint8_t *data, size_t len) {
    nc_value v = nc_null();
    v.type = NC_EXT;
    v.as.ext.type_name = type_name;
    v.as.ext.data = data;
    v.as.ext.len = len;
    return v;
}

static int hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

int nc_uuid_parse(const char *text, nc_uuid *out) {
    const char *p = text;
    size_t len = strlen(text);
    int n = 0, hyphen_ok = 0, braces = 0;
    memset(out, 0, sizeof *out);
    if (len && p[0] == '{') {
        if (p[len - 1] != '}') return -1;
        braces = 1;
        p++;
        len -= 2;
    }
    for (; len > 0; p++, len--) {
        int d;
        if (*p == '-') {
            if (!hyphen_ok) return -1;
            hyphen_ok = 0;
            continue;
        }
        d = hex_digit(*p);
        if (d < 0 || n == 32) return -1;
        out->bytes[n / 2] |= (uint8_t)(n % 2 == 0 ? d << 4 : d);
        n++;
        hyphen_ok = n % 4 == 0 && n < 32;
    }
    (void)braces;
    return n == 32 && p[-1] != '-' ? 0 : -1;
}

/* 128-bit magnitudes as four 32-bit limbs, most significant first. */
typedef struct { uint32_t l[4]; } u128;

static int mul10_add(u128 *x, unsigned d) {
    uint64_t carry = d;
    int i;
    for (i = 3; i >= 0; i--) {
        uint64_t t = (uint64_t)x->l[i] * 10 + carry;
        x->l[i] = (uint32_t)t;
        carry = t >> 32;
    }
    return carry != 0;
}

static unsigned divmod10(u128 *x) {
    uint64_t rem = 0;
    int i;
    for (i = 0; i < 4; i++) {
        uint64_t cur = (rem << 32) | x->l[i];
        x->l[i] = (uint32_t)(cur / 10);
        rem = cur % 10;
    }
    return (unsigned)rem;
}

static int is_zero(const u128 *x) { return !(x->l[0] | x->l[1] | x->l[2] | x->l[3]); }

static void negate(uint64_t *lo, uint64_t *hi) {
    *lo = ~*lo + 1;
    *hi = ~*hi + (*lo == 0);
}

int nc_numeric(const char *text, nc_value *out) {
    u128 m = {{0, 0, 0, 0}};
    int neg = 0, scale = 0, seen = 0, point = 0;
    uint64_t lo, hi;
    const char *p = text;
    if (!p) return -1;
    while (*p == ' ') p++;
    if (*p == '-' || *p == '+') neg = *p++ == '-';
    for (; *p; p++) {
        if (*p == '.' && !point) {
            point = 1;
        } else if (*p >= '0' && *p <= '9') {
            if (mul10_add(&m, (unsigned)(*p - '0'))) return -1;
            seen = 1;
            if (point) scale++;
        } else {
            break;
        }
    }
    while (*p == ' ') p++;
    if (!seen || *p || scale > 38 || (m.l[0] & 0x80000000u)) return -1;
    lo = (uint64_t)m.l[2] << 32 | m.l[3];
    hi = (uint64_t)m.l[0] << 32 | m.l[1];
    if (neg) negate(&lo, &hi);
    *out = nc_null();
    out->type = NC_NUMERIC;
    out->as.numeric.lo = lo;
    out->as.numeric.hi = (int64_t)hi;
    out->as.numeric.scale = (uint8_t)scale;
    return 0;
}

int32_t nc_days_from_ymd(int year, int month, int day) {
    int64_t y = month <= 2 ? year - 1 : year;
    int64_t era = (y >= 0 ? y : y - 399) / 400;
    int64_t yoe = y - era * 400;
    int64_t mp = (month + 9) % 12;
    int64_t doy = (153 * mp + 2) / 5 + day - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return (int32_t)(era * 146097 + doe - 719468 - 10957);
}

void nc_ymd_from_days(int32_t days, int *year, int *month, int *day) {
    int64_t z = (int64_t)days + 10957 + 719468;
    int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    int64_t d = doy - (153 * mp + 2) / 5 + 1;
    int64_t m = mp < 10 ? mp + 3 : mp - 9;
    *year = (int)(yoe + era * 400 + (m <= 2));
    *month = (int)m;
    *day = (int)d;
}

static size_t fmt_time(char *out, size_t size, int64_t us_of_day) {
    int64_t h = us_of_day / 3600000000LL, mi = us_of_day / 60000000LL % 60;
    int64_t s = us_of_day / 1000000 % 60, f = us_of_day % 1000000;
    if (f == 0) return (size_t)snprintf(out, size, "%02lld:%02lld:%02lld", (long long)h, (long long)mi, (long long)s);
    {
        char frac[8];
        int n;
        snprintf(frac, sizeof frac, "%06lld", (long long)f);
        for (n = 5; n > 0 && frac[n] == '0'; n--) frac[n] = 0;
        return (size_t)snprintf(out, size, "%02lld:%02lld:%02lld.%s", (long long)h, (long long)mi, (long long)s, frac);
    }
}

size_t nc_format(const nc_value *v, char *buf, size_t size) {
    char tmp[128];
    size_t n = 0, i;
    if (size) buf[0] = 0;
    switch (v->type) {
    case NC_NULL:
    case NC_CHAR: /* column types only: their values are NC_TEXT */
    case NC_ENUM: return 0;
    case NC_BOOL: return (size_t)snprintf(buf, size, "%s", v->as.boolean ? "t" : "f");
    case NC_INT: return (size_t)snprintf(buf, size, "%lld", (long long)v->as.integer);
    case NC_SMALLINT:
    case NC_INTEGER: return (size_t)snprintf(buf, size, "%lld", (long long)v->as.integer);
    case NC_FLOAT: return (size_t)snprintf(buf, size, "%.17g", v->as.real);
    case NC_REAL: {
        /* The shortest digits that read back to the same float. */
        int p;
        for (p = 1; p < 9; p++) {
            snprintf(tmp, sizeof tmp, "%.*g", p, (double)v->as.real4);
            if (strtof(tmp, NULL) == v->as.real4) break;
        }
        return (size_t)snprintf(buf, size, "%.*g", p, (double)v->as.real4);
    }
    case NC_BYTEA:
        n = (size_t)snprintf(buf, size, "\\x");
        for (i = 0; i < v->as.bytes.len; i++)
            n += (size_t)snprintf(n < size ? buf + n : NULL, n < size ? size - n : 0, "%02x", v->as.bytes.ptr[i]);
        return n;
    case NC_TIME: return fmt_time(buf, size, v->as.time);
    case NC_INET:
    case NC_CIDR: {
        const uint8_t *a = v->as.net.addr;
        int full = v->as.net.family == 4 ? 32 : 128;
        if (v->as.net.family == 4) {
            n = (size_t)snprintf(tmp, sizeof tmp, "%u.%u.%u.%u", a[0], a[1], a[2], a[3]);
        } else {
            /* The longest run of two or more zero groups as "::"; a mapped
             * or compatible IPv4 address dotted (as nucleus prints it). */
            unsigned w[8];
            int best = -1, len = 0, k;
            for (k = 0; k < 8; k++) w[k] = (unsigned)(a[2 * k] << 8 | a[2 * k + 1]);
            for (k = 0; k < 8;) {
                int start = k;
                if (w[k]) {
                    k++;
                    continue;
                }
                while (k < 8 && !w[k]) k++;
                if (k - start > len) best = start, len = k - start;
            }
            if (len < 2) best = -1;
            if (best == 0 && (len == 6 || (len == 5 && w[5] == 0xffff))) {
                n = (size_t)snprintf(tmp, sizeof tmp, "%s%u.%u.%u.%u", len == 5 ? "::ffff:" : "::", a[12], a[13], a[14], a[15]);
            } else {
                n = 0;
                tmp[0] = 0;
                for (k = 0; k < 8;) {
                    if (k == best) {
                        n += (size_t)snprintf(tmp + n, sizeof tmp - n, "::");
                        k += len;
                        continue;
                    }
                    if (n && tmp[n - 1] != ':') tmp[n++] = ':', tmp[n] = 0;
                    n += (size_t)snprintf(tmp + n, sizeof tmp - n, "%x", w[k]);
                    k++;
                }
            }
        }
        if (v->type == NC_INET && v->as.net.bits == full) return (size_t)snprintf(buf, size, "%s", tmp);
        return (size_t)snprintf(buf, size, "%s/%u", tmp, v->as.net.bits);
    }
    case NC_TEXT: return (size_t)snprintf(buf, size, "%s", v->as.text.ptr);
    case NC_NUMERIC: {
        uint64_t lo = v->as.numeric.lo, hi = (uint64_t)v->as.numeric.hi;
        int neg = v->as.numeric.hi < 0, len = 0, scale = v->as.numeric.scale, k;
        u128 m;
        char digits[48];
        if (neg) negate(&lo, &hi);
        m.l[0] = (uint32_t)(hi >> 32);
        m.l[1] = (uint32_t)hi;
        m.l[2] = (uint32_t)(lo >> 32);
        m.l[3] = (uint32_t)lo;
        do digits[len++] = (char)('0' + divmod10(&m)); while (!is_zero(&m));
        while (len <= scale) digits[len++] = '0';
        if (neg) tmp[n++] = '-';
        for (k = len - 1; k >= 0; k--) {
            tmp[n++] = digits[k];
            if (k == scale && scale > 0) tmp[n++] = '.';
        }
        tmp[n] = 0;
        return (size_t)snprintf(buf, size, "%s", tmp);
    }
    case NC_DATE: {
        int y, m, d;
        nc_ymd_from_days(v->as.date, &y, &m, &d);
        return (size_t)snprintf(buf, size, "%04d-%02d-%02d", y, m, d);
    }
    case NC_TIMESTAMP:
    case NC_TIMESTAMPTZ: {
        const int64_t day = 86400000000LL;
        int64_t t = v->as.timestamp, days = t / day, rem = t % day;
        int y, m, d;
        if (rem < 0) {
            rem += day;
            days--;
        }
        nc_ymd_from_days((int32_t)days, &y, &m, &d);
        fmt_time(tmp, sizeof tmp, rem);
        return (size_t)snprintf(buf, size, "%04d-%02d-%02d %s%s", y, m, d, tmp,
                                v->type == NC_TIMESTAMPTZ ? "+00" : "");
    }
    case NC_INTERVAL: {
        int64_t us = v->as.interval.micros;
        fmt_time(tmp, sizeof tmp, us < 0 ? -us : us);
        return (size_t)snprintf(buf, size, "%d mons %d days %s%s", (int)v->as.interval.months,
                                (int)v->as.interval.days, us < 0 ? "-" : "", tmp);
    }
    case NC_UUID: {
        const uint8_t *b = v->as.uuid.bytes;
        return (size_t)snprintf(buf, size,
                                "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x",
                                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10],
                                b[11], b[12], b[13], b[14], b[15]);
    }
    case NC_EXT:
        n = (size_t)snprintf(buf, size, "\\x");
        for (i = 0; i < v->as.ext.len; i++)
            n += (size_t)snprintf(n < size ? buf + n : NULL, n < size ? size - n : 0, "%02x", v->as.ext.data[i]);
        return n;
    case NC_ARRAY:
        n = (size_t)snprintf(buf, size, "{");
        for (i = 0; i < v->as.array.len; i++) {
            const nc_value *e = &v->as.array.items[i];
            if (i) n += (size_t)snprintf(n < size ? buf + n : NULL, n < size ? size - n : 0, ",");
            if (e->type == NC_NULL)
                n += (size_t)snprintf(n < size ? buf + n : NULL, n < size ? size - n : 0, "NULL");
            else
                n += nc_format(e, n < size ? buf + n : NULL, n < size ? size - n : 0);
        }
        n += (size_t)snprintf(n < size ? buf + n : NULL, n < size ? size - n : 0, "}");
        return n;
    }
    return 0;
}
