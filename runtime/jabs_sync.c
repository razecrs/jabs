/* Synchronization the JVM provides: object monitors (synchronized, wait, notify) and the atomic
 * accesses behind volatile fields, Unsafe and VarHandles. Handwritten, OS-specific; generated code
 * reaches it only through the declarations in jabs_rt.h.
 *
 * Monitors. An object's header field `monitor` is 0 until the object is first locked, then the
 * 1-based index of its monitor in a grow-only table (objects are never freed, so neither are
 * monitors). Installing the index takes a global lock once per object; every later enter and
 * exit finds the monitor without it. A monitor is an OS mutex held for as long as a thread owns
 * the monitor, plus the owner and the recursion count (touched only by the owner). The OS mutex
 * already has an uncontended fast path (one atomic instruction), so JABS does not add a
 * thin-lock layer of its own. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#include "jabs_unit.h"

#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#endif

/* Something the OS or the runtime's own bookkeeping got wrong: there is no sane way on. */
static _Noreturn void sync_fail(const char *what, long code) {
    fprintf(stderr, "jabs: %s (%ld) in runtime/jabs_sync.c\n", what, code);
    fflush(stderr);
    abort();
}

#ifdef _WIN32
typedef SRWLOCK sync_mutex;
typedef CONDITION_VARIABLE sync_cond;
#define SYNC_MUTEX_INIT SRWLOCK_INIT
static void sync_mutex_init(sync_mutex *m) { InitializeSRWLock(m); }
static void sync_mutex_lock(sync_mutex *m) { AcquireSRWLockExclusive(m); }
static void sync_mutex_unlock(sync_mutex *m) { ReleaseSRWLockExclusive(m); }
static void sync_cond_init(sync_cond *c) { InitializeConditionVariable(c); }
#else
typedef pthread_mutex_t sync_mutex;
typedef pthread_cond_t sync_cond;
#define SYNC_MUTEX_INIT PTHREAD_MUTEX_INITIALIZER
static void sync_mutex_init(sync_mutex *m) {
    int rc = pthread_mutex_init(m, NULL);
    if (rc != 0) sync_fail("pthread_mutex_init failed", rc);
}
static void sync_mutex_lock(sync_mutex *m) {
    int rc = pthread_mutex_lock(m);
    if (rc != 0) sync_fail("pthread_mutex_lock failed", rc);
}
static void sync_mutex_unlock(sync_mutex *m) {
    int rc = pthread_mutex_unlock(m);
    if (rc != 0) sync_fail("pthread_mutex_unlock failed", rc);
}
static void sync_cond_init(sync_cond *c) {
    int rc = pthread_cond_init(c, NULL);
    if (rc != 0) sync_fail("pthread_cond_init failed", rc);
}
#endif

/* ---- Threads as the monitors see them ---- */

/* One per OS thread, created on its first monitor operation and never freed, so a pointer to it
 * stays valid and unique even after the thread ends: it is the monitor owner's identity. */
struct jabs_sync_thread {
    sync_mutex lock; /* for waiting (Object.wait) and waking (notify, interrupt) this thread */
    sync_cond wake;  /* only this thread sleeps on it */
};

static _Thread_local jabs_sync_thread *sync_self;

jabs_sync_thread *jabs_sync_current(void) {
    if (sync_self) return sync_self;
    jabs_sync_thread *t = (jabs_sync_thread *)calloc(1, sizeof *t);
    if (!t) sync_fail("out of memory for a thread's monitor record", 0);
    sync_mutex_init(&t->lock);
    sync_cond_init(&t->wake);
    sync_self = t;
    return t;
}

/* ---- The monitor table ---- */

typedef struct {
    sync_mutex mutex;          /* held by the owner for as long as it owns the monitor */
    jabs_sync_thread *owner;   /* NULL when free; any thread may read it (atomically) */
    uint32_t count;            /* recursion depth; only the owner touches it */
} sync_monitor;

#define SYNC_CHUNK_BITS 10
#define SYNC_CHUNK_SIZE (1u << SYNC_CHUNK_BITS)
/* 16384 chunks of 1024 monitors: up to 16M objects ever locked. */
#define SYNC_MAX_CHUNKS 16384u
static sync_monitor *sync_chunks[SYNC_MAX_CHUNKS];
static uint32_t sync_monitors_used; /* guarded by sync_table_lock */
static sync_mutex sync_table_lock = SYNC_MUTEX_INIT;

/* The monitor at a 1-based index read from an object header with acquire order: the chunk was
 * stored before the index was published, so it is visible here. */
static sync_monitor *sync_monitor_at(uint32_t index) {
    if (index == 0) sync_fail("monitor index 0 looked up", 0);
    uint32_t slot = index - 1;
    if ((slot >> SYNC_CHUNK_BITS) >= SYNC_MAX_CHUNKS) sync_fail("monitor index out of range", (long)index);
    sync_monitor *chunk = sync_chunks[slot >> SYNC_CHUNK_BITS];
    if (!chunk) sync_fail("monitor index in a chunk never allocated", (long)index);
    return &chunk[slot & (SYNC_CHUNK_SIZE - 1)];
}

/* The object's monitor, or NULL if it was never locked (then no thread owns it). */
static sync_monitor *sync_monitor_if_any(jabs_object *o) {
    uint32_t index = __atomic_load_n(&o->monitor, __ATOMIC_ACQUIRE);
    return index == 0 ? NULL : sync_monitor_at(index);
}

/* The object's monitor, creating it on first use. */
static sync_monitor *sync_monitor_of(jabs_object *o) {
    sync_monitor *m = sync_monitor_if_any(o);
    if (m) return m;
    sync_mutex_lock(&sync_table_lock);
    /* Another thread may have installed one between the check and the lock. */
    uint32_t index = __atomic_load_n(&o->monitor, __ATOMIC_ACQUIRE);
    if (index != 0) {
        sync_mutex_unlock(&sync_table_lock);
        return sync_monitor_at(index);
    }
    uint32_t slot = sync_monitors_used;
    uint32_t chunk = slot >> SYNC_CHUNK_BITS;
    if (chunk >= SYNC_MAX_CHUNKS) sync_fail("more objects locked than the monitor table holds", (long)slot);
    if (!sync_chunks[chunk]) {
        sync_chunks[chunk] = (sync_monitor *)calloc(SYNC_CHUNK_SIZE, sizeof(sync_monitor));
        if (!sync_chunks[chunk]) sync_fail("out of memory for monitors", (long)chunk);
    }
    m = &sync_chunks[chunk][slot & (SYNC_CHUNK_SIZE - 1)];
    sync_mutex_init(&m->mutex);
    sync_monitors_used = slot + 1;
    /* Release: whoever reads the index also sees the chunk and the initialized mutex. Plain
     * store, not CAS: every installer holds sync_table_lock and rechecked the header under it. */
    __atomic_store_n(&o->monitor, slot + 1, __ATOMIC_RELEASE);
    sync_mutex_unlock(&sync_table_lock);
    return m;
}

/* Relaxed is enough for the owner: a thread only ever compares it with itself, and it is the
 * only thread that stores itself there, so it always sees its own last store. */
static jabs_sync_thread *sync_owner(sync_monitor *m) { return __atomic_load_n(&m->owner, __ATOMIC_RELAXED); }
static void sync_set_owner(sync_monitor *m, jabs_sync_thread *t) { __atomic_store_n(&m->owner, t, __ATOMIC_RELAXED); }

/* ---- monitorenter / monitorexit ---- */

void jabs_monitor_enter(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_enter(NULL): the caller must raise NullPointerException", 0);
    jabs_sync_thread *self = jabs_sync_current();
    sync_monitor *m = sync_monitor_of(o);
    if (sync_owner(m) == self) {
        if (m->count == UINT32_MAX) sync_fail("monitor recursion count overflow", (long)m->count);
        m->count++;
        return;
    }
    sync_mutex_lock(&m->mutex);
    if (sync_owner(m) != NULL || m->count != 0) sync_fail("monitor acquired while still owned", (long)m->count);
    sync_set_owner(m, self);
    m->count = 1;
}

int jabs_monitor_exit(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_exit(NULL): the caller must raise NullPointerException", 0);
    sync_monitor *m = sync_monitor_if_any(o);
    if (!m || sync_owner(m) != jabs_sync_current()) return 0;
    if (m->count == 0) sync_fail("owned monitor with recursion count 0", 0);
    m->count--;
    if (m->count > 0) return 1;
    sync_set_owner(m, NULL);
    sync_mutex_unlock(&m->mutex);
    return 1;
}

int jabs_monitor_holds(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_holds(NULL): the caller must raise NullPointerException", 0);
    sync_monitor *m = sync_monitor_if_any(o);
    return m && sync_owner(m) == jabs_sync_current();
}
