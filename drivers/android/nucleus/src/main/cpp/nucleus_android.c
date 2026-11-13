/* The embedded edition's C API (nucleus_embedded.h) for the JVM: what
 * io.github.davidsarosi92.nucleus.android.Native declares.  A session's
 * bytes go through send and recv; the Java driver speaks the protocol
 * over them (PLAN.md §M14.23). */
#include <jni.h>
#include <stdint.h>
#include <stdlib.h>

#include "nucleus_embedded.h"

#define NATIVE(name) Java_io_github_davidsarosi92_nucleus_android_Native_##name

/* Throws an IOException with the library's message (and frees it). */
static void fail(JNIEnv *env, char *error, const char *otherwise) {
    jclass io = (*env)->FindClass(env, "java/io/IOException");
    if (io) (*env)->ThrowNew(env, io, error ? error : otherwise);
    nce_free_error(error);
}

JNIEXPORT jstring JNICALL NATIVE(version)(JNIEnv *env, jclass cls) {
    (void)cls;
    return (*env)->NewStringUTF(env, nce_version());
}

JNIEXPORT jlong JNICALL NATIVE(open)(JNIEnv *env, jclass cls, jstring path, jstring options) {
    const char *p = (*env)->GetStringUTFChars(env, path, NULL);
    const char *o = options ? (*env)->GetStringUTFChars(env, options, NULL) : NULL;
    char *error = NULL;
    nce_database *db = p ? nce_open(p, o, &error) : NULL;
    (void)cls;
    if (o) (*env)->ReleaseStringUTFChars(env, options, o);
    if (p) (*env)->ReleaseStringUTFChars(env, path, p);
    if (!db) fail(env, error, "cannot open the database");
    return (jlong)(intptr_t)db;
}

JNIEXPORT void JNICALL NATIVE(close)(JNIEnv *env, jclass cls, jlong db) {
    char *error = NULL;
    (void)cls;
    if (nce_close((nce_database *)(intptr_t)db, &error) != 0) fail(env, error, "cannot close the database");
}

JNIEXPORT jlong JNICALL NATIVE(connect)(JNIEnv *env, jclass cls, jlong db) {
    char *error = NULL;
    nce_connection *c = nce_connect((nce_database *)(intptr_t)db, &error);
    (void)cls;
    if (!c) fail(env, error, "cannot start a session");
    return (jlong)(intptr_t)c;
}

/* Bytes go through a buffer of ours: the session may wait (recv), and a
 * Java array is not to be held while it does. */
#define CHUNK (64 << 10)

JNIEXPORT jint JNICALL NATIVE(send)(JNIEnv *env, jclass cls, jlong c, jbyteArray b, jint off, jint len) {
    uint8_t stack[CHUNK];
    (void)cls;
    while (len > 0) {
        jint n = len < CHUNK ? len : CHUNK;
        (*env)->GetByteArrayRegion(env, b, off, n, (jbyte *)stack);
        if ((*env)->ExceptionCheck(env)) return -1;
        if (nce_send((nce_connection *)(intptr_t)c, stack, (size_t)n) != 0) return -1;
        off += n;
        len -= n;
    }
    return 0;
}

JNIEXPORT jint JNICALL NATIVE(recv)(JNIEnv *env, jclass cls, jlong c, jbyteArray b, jint off, jint len) {
    uint8_t stack[CHUNK];
    int64_t n;
    (void)cls;
    n = nce_recv((nce_connection *)(intptr_t)c, stack, (size_t)(len < CHUNK ? len : CHUNK), -1);
    if (n <= 0) return -1; /* the session has ended */
    (*env)->SetByteArrayRegion(env, b, off, (jint)n, (const jbyte *)stack);
    return (jint)n;
}

JNIEXPORT void JNICALL NATIVE(end)(JNIEnv *env, jclass cls, jlong c) {
    (void)env;
    (void)cls;
    nce_end((nce_connection *)(intptr_t)c);
}

JNIEXPORT void JNICALL NATIVE(disconnect)(JNIEnv *env, jclass cls, jlong c) {
    (void)env;
    (void)cls;
    nce_disconnect((nce_connection *)(intptr_t)c);
}
