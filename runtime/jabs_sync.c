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
 * thin-lock layer of its own.
 *
 * Waiting. Each OS thread has a record (jabs_sync_thread) with its own lock and condition
 * variable. wait() puts a node for the thread at the end of the monitor's wait set, releases the
 * monitor fully and sleeps on its own condition variable; notify() takes the first node out of
 * the set, marks it notified under that thread's lock and wakes it. A thread sleeps only on its
 * own condition variable, so a wakeup reaches exactly the thread chosen, spurious OS wakeups are
 * filtered by the `notified` flag (JABS's wait never returns spuriously), and the interrupt hook
 * can wake a thread without touching any monitor. Lock order: a monitor's mutex, then a thread's
 * lock; nothing holds a thread's lock while taking a monitor's mutex. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#include "jabs_unit.h"

#ifdef _WIN32
#include <windows.h>
#else
#include <errno.h>
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
static void sync_cond_signal(sync_cond *c) { WakeConditionVariable(c); }
/* Sleep on `c` (releasing `m`) until woken, spuriously woken or `nanos` passed (-1: no limit). */
static void sync_cond_wait(sync_cond *c, sync_mutex *m, int64_t nanos) {
    DWORD ms = INFINITE;
    if (nanos >= 0) {
        int64_t rounded = nanos / 1000000 + (nanos % 1000000 != 0);
        /* INFINITE is 0xFFFFFFFF; a longer sleep wakes early and the caller sleeps again. */
        ms = rounded > 0x7fffffff ? 0x7fffffff : (DWORD)rounded;
    }
    if (!SleepConditionVariableSRW(c, m, ms, 0) && GetLastError() != ERROR_TIMEOUT) {
        sync_fail("SleepConditionVariableSRW failed", (long)GetLastError());
    }
}
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
static void sync_cond_signal(sync_cond *c) {
    int rc = pthread_cond_signal(c);
    if (rc != 0) sync_fail("pthread_cond_signal failed", rc);
}
/* Sleep on `c` (releasing `m`) until woken, spuriously woken or `nanos` passed (-1: no limit).
 * The default condition clock is the wall clock, which can jump, so one sleep is at most a
 * second and the caller keeps the real deadline on the monotonic clock. (pthread_condattr_setclock
 * needs POSIX feature macros that the single-file build cannot set before the C headers.) */
static void sync_cond_wait(sync_cond *c, sync_mutex *m, int64_t nanos) {
    if (nanos < 0) {
        int rc = pthread_cond_wait(c, m);
        if (rc != 0) sync_fail("pthread_cond_wait failed", rc);
        return;
    }
    if (nanos > 1000000000) nanos = 1000000000;
    struct timespec until;
    if (timespec_get(&until, TIME_UTC) != TIME_UTC) sync_fail("timespec_get failed", 0);
    int64_t ns = (int64_t)until.tv_nsec + nanos;
    until.tv_sec += (time_t)(ns / 1000000000);
    until.tv_nsec = (long)(ns % 1000000000);
    int rc = pthread_cond_timedwait(c, m, &until);
    if (rc != 0 && rc != ETIMEDOUT) sync_fail("pthread_cond_timedwait failed", rc);
}
#endif

/* ---- Threads as the monitors see them ---- */

/* One per OS thread, created on its first monitor operation and never freed, so a pointer to it
 * stays valid and unique even after the thread ends: it is the monitor owner's identity. */
struct jabs_sync_thread {
    sync_mutex lock;  /* guards `interrupted` and the `notified` flag of this thread's wait node */
    sync_cond wake;   /* only this thread sleeps on it */
    bool interrupted; /* jabs_sync_interrupt was called and no wait has consumed it yet */
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

void jabs_sync_interrupt(jabs_sync_thread *t) {
    if (!t) sync_fail("jabs_sync_interrupt without a thread", 0);
    sync_mutex_lock(&t->lock);
    t->interrupted = true;
    sync_cond_signal(&t->wake);
    sync_mutex_unlock(&t->lock);
}

bool jabs_sync_clear_interrupt(jabs_sync_thread *t) {
    if (!t) sync_fail("jabs_sync_clear_interrupt without a thread", 0);
    sync_mutex_lock(&t->lock);
    bool was = t->interrupted;
    t->interrupted = false;
    sync_mutex_unlock(&t->lock);
    return was;
}

/* ---- The monitor table ---- */

/* A thread in a monitor's wait set; lives on that thread's stack while it is in wait(), which
 * cannot return before it owns the monitor again, so whoever holds the monitor may touch it. */
typedef struct sync_waiter {
    struct sync_waiter *next;
    jabs_sync_thread *thread;
    bool notified; /* written under the monitor's mutex and the waiting thread's lock */
} sync_waiter;

typedef struct {
    sync_mutex mutex;          /* held by the owner for as long as it owns the monitor */
    jabs_sync_thread *owner;   /* NULL when free; any thread may read it (atomically) */
    uint32_t count;            /* recursion depth; only the owner touches it */
    sync_waiter *first, *last; /* the wait set in arrival order; only the owner touches it */
    uint32_t waiting;          /* nodes in the wait set */
} sync_monitor;

/* A wait set holds at most one node per thread; far above any real program. */
#define SYNC_MAX_WAITERS 1000000u

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

/* ---- wait / notify ---- */

/* Take the first node out of the wait set and wake its thread; false when the set is empty.
 * The caller owns the monitor. */
static bool sync_notify_first(sync_monitor *m) {
    sync_waiter *w = m->first;
    if (!w) return false;
    if (m->waiting == 0) sync_fail("wait set count out of step with its nodes", 0);
    m->first = w->next;
    if (!m->first) m->last = NULL;
    m->waiting--;
    w->next = NULL;
    sync_mutex_lock(&w->thread->lock);
    w->notified = true;
    sync_cond_signal(&w->thread->wake);
    sync_mutex_unlock(&w->thread->lock);
    return true;
}

int jabs_monitor_notify(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_notify(NULL): the caller must raise NullPointerException", 0);
    sync_monitor *m = sync_monitor_if_any(o);
    if (!m || sync_owner(m) != jabs_sync_current()) return 0;
    sync_notify_first(m);
    return 1;
}

int jabs_monitor_notify_all(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_notify_all(NULL): the caller must raise NullPointerException", 0);
    sync_monitor *m = sync_monitor_if_any(o);
    if (!m || sync_owner(m) != jabs_sync_current()) return 0;
    for (uint32_t i = 0; i <= SYNC_MAX_WAITERS; i++) {
        if (!sync_notify_first(m)) return 1;
    }
    sync_fail("wait set longer than any thread count", (long)m->waiting);
}

/* Take a node that was not notified out of the wait set (its wait ended another way). The
 * caller owns the monitor. */
static void sync_unlink(sync_monitor *m, sync_waiter *w) {
    sync_waiter *before = NULL;
    sync_waiter *at = m->first;
    for (uint32_t i = 0; i <= SYNC_MAX_WAITERS && at; i++) {
        if (at == w) {
            if (before) before->next = w->next;
            else m->first = w->next;
            if (m->last == w) m->last = before;
            m->waiting--;
            w->next = NULL;
            return;
        }
        before = at;
        at = at->next;
    }
    sync_fail("a waiting thread is missing from its wait set", (long)m->waiting);
}

int jabs_monitor_wait(jabs_object *o, int64_t millis) {
    if (!o) sync_fail("jabs_monitor_wait(NULL): the caller must raise NullPointerException", 0);
    if (millis < 0) sync_fail("jabs_monitor_wait with a negative timeout: Object.wait must reject it first", (long)millis);
    jabs_sync_thread *self = jabs_sync_current();
    sync_monitor *m = sync_monitor_if_any(o);
    /* The JVM checks ownership before a pending interrupt. */
    if (!m || sync_owner(m) != self) return JABS_WAIT_NOT_OWNER;
    if (jabs_sync_clear_interrupt(self)) return JABS_WAIT_INTERRUPTED;
    if (m->waiting >= SYNC_MAX_WAITERS) sync_fail("wait set longer than any thread count", (long)m->waiting);

    sync_waiter node = { NULL, self, false };
    if (m->last) m->last->next = &node;
    else m->first = &node;
    m->last = &node;
    m->waiting++;

    /* Release every level; the count comes back when this thread owns the monitor again. */
    uint32_t levels = m->count;
    m->count = 0;
    sync_set_owner(m, NULL);
    sync_mutex_unlock(&m->mutex);

    int64_t deadline = INT64_MAX;
    if (millis > 0) {
        int64_t limit = millis > INT64_MAX / 1000000 ? INT64_MAX : millis * 1000000;
        int64_t now = jabs_nano_time();
        deadline = now > INT64_MAX - limit ? INT64_MAX : now + limit;
    }
    int outcome = JABS_WAIT_NOT_OWNER;
    sync_mutex_lock(&self->lock);
    /* Ends on a notification, an interrupt or the deadline; an OS wakeup with none of these only
     * sleeps again, so this loop has no count of its own. */
    for (;;) {
        if (node.notified) { outcome = JABS_WAIT_NOTIFIED; break; }
        if (self->interrupted) { self->interrupted = false; outcome = JABS_WAIT_INTERRUPTED; break; }
        int64_t left = -1;
        if (millis > 0) {
            left = deadline - jabs_nano_time();
            if (left <= 0) { outcome = JABS_WAIT_TIMED_OUT; break; }
        }
        sync_cond_wait(&self->wake, &self->lock, left);
    }
    sync_mutex_unlock(&self->lock);

    sync_mutex_lock(&m->mutex);
    if (sync_owner(m) != NULL || m->count != 0) sync_fail("monitor reacquired after wait while still owned", (long)m->count);
    sync_set_owner(m, self);
    m->count = levels;
    /* A notify may have picked this node after the wait ended another way (it saw the node
     * before this thread got the monitor back). Then the notification counts: otherwise it
     * would be lost to every other waiter. An interrupt consumed meanwhile stays pending, as
     * the JLS allows for a thread both notified and interrupted (17.2.4). */
    sync_mutex_lock(&self->lock);
    bool notified = node.notified;
    if (notified && outcome == JABS_WAIT_INTERRUPTED) self->interrupted = true;
    sync_mutex_unlock(&self->lock);
    if (notified) return JABS_WAIT_NOTIFIED;
    sync_unlink(m, &node);
    return outcome;
}
