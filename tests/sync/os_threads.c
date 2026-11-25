#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#include "os_threads.h"
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#endif

static void os_fail(const char *what, long code) {
    fprintf(stderr, "harness: %s failed (%ld) in tests/sync/os_threads.c\n", what, code);
    fflush(stderr);
    abort();
}

struct harness_thread {
    void (*fn)(void *);
    void *arg;
#ifdef _WIN32
    HANDLE handle;
#else
    pthread_t id;
#endif
};

#ifdef _WIN32
static DWORD WINAPI os_trampoline(LPVOID p) {
    harness_thread *t = (harness_thread *)p;
    t->fn(t->arg);
    return 0;
}
#else
static void *os_trampoline(void *p) {
    harness_thread *t = (harness_thread *)p;
    t->fn(t->arg);
    return NULL;
}
#endif

harness_thread *harness_spawn(void (*fn)(void *), void *arg) {
    harness_thread *t = (harness_thread *)calloc(1, sizeof *t);
    if (!t) os_fail("calloc", 0);
    t->fn = fn;
    t->arg = arg;
#ifdef _WIN32
    t->handle = CreateThread(NULL, 0, os_trampoline, t, 0, NULL);
    if (!t->handle) os_fail("CreateThread", (long)GetLastError());
#else
    int rc = pthread_create(&t->id, NULL, os_trampoline, t);
    if (rc != 0) os_fail("pthread_create", rc);
#endif
    return t;
}

void harness_join(harness_thread *t) {
#ifdef _WIN32
    if (WaitForSingleObject(t->handle, INFINITE) != WAIT_OBJECT_0) os_fail("WaitForSingleObject", (long)GetLastError());
    if (!CloseHandle(t->handle)) os_fail("CloseHandle", (long)GetLastError());
#else
    int rc = pthread_join(t->id, NULL);
    if (rc != 0) os_fail("pthread_join", rc);
#endif
    free(t);
}

void harness_sleep_ms(int ms) {
#ifdef _WIN32
    Sleep((DWORD)ms);
#else
    struct timespec d = { ms / 1000, (long)(ms % 1000) * 1000000L };
    while (nanosleep(&d, &d) != 0) {
    }
#endif
}

int64_t harness_now_ms(void) {
#ifdef _WIN32
    return (int64_t)GetTickCount64();
#else
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t) != 0) os_fail("clock_gettime", 0);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
#endif
}

static const char *watched_test;
static int watched_seconds;

static void os_watchdog(void *unused) {
    (void)unused;
    harness_sleep_ms(watched_seconds * 1000);
    fprintf(stderr, "WATCHDOG: %s did not finish within %d s: a deadlock or a lost wakeup "
                    "(look at runtime/jabs_sync.c)\n", watched_test, watched_seconds);
    fflush(stderr);
    _Exit(3);
}

void harness_start_watchdog(const char *test, int seconds) {
    watched_test = test;
    watched_seconds = seconds;
    harness_spawn(os_watchdog, NULL);
}
