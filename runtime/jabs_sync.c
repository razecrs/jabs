/* Synchronization the JVM provides: object monitors (synchronized, wait, notify) and the atomic
 * accesses behind volatile fields, Unsafe and VarHandles. Handwritten, OS-specific; generated code
 * reaches it only through the declarations in jabs_rt.h.
 *
 * Monitors. An object's header field `monitor` is its lock word (encoding at "Lock words"). Most
 * objects are only ever locked by one thread at a time and never waited on (StringBuffer,
 * Hashtable, Vector...), so a lock starts thin: the owner's thread id and the recursion depth in
 * the word itself, taken and released with one compare-and-set and no memory of its own. It is
 * inflated into a monitor record (mutex, condition variables, owner, depth, wait set) only on
 * contention, on wait, or past 2047 levels; then it stays inflated. Monitors live in a
 * grow-only table: objects are never freed, so a monitor is never orphaned either.
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

/* Thread ids name lock owners in object headers (20 bits there). Ids start at 1; 0 is "nobody". */
#define SYNC_MAX_THREAD_ID ((1u << 20) - 1)

/* One per OS thread, created on its first monitor operation and never freed, so a pointer to it
 * stays valid after the thread ends (an interrupt sent to it then does nothing useful). */
struct jabs_sync_thread {
    sync_mutex lock;  /* guards `interrupted` and the `notified` flag of this thread's wait node */
    sync_cond wake;   /* only this thread sleeps on it */
    bool interrupted; /* jabs_sync_interrupt was called and no wait has consumed it yet */
    uint32_t id;      /* this thread's id in lock words and as an inflated monitor's owner */
    uint64_t held;    /* monitor levels this thread holds now; only this thread touches it */
    uint32_t spare;   /* a monitor index allocated by a lost inflation race, kept for the next */
    struct jabs_sync_thread *next_retired; /* in the retired list, guarded by sync_table_lock */
};

static _Thread_local jabs_sync_thread *sync_self;

static sync_mutex sync_table_lock = SYNC_MUTEX_INIT; /* thread ids and the monitor table */
static uint32_t sync_next_thread_id = 1;               /* guarded by sync_table_lock */
static jabs_sync_thread *sync_retired;                 /* ended threads whose ids are free again */

jabs_sync_thread *jabs_sync_current(void) {
    if (sync_self) return sync_self;
    jabs_sync_thread *t = (jabs_sync_thread *)calloc(1, sizeof *t);
    if (!t) sync_fail("out of memory for a thread's monitor record", 0);
    sync_mutex_init(&t->lock);
    sync_cond_init(&t->wake);
    sync_mutex_lock(&sync_table_lock);
    if (sync_retired) {
        /* The ended thread's record stays allocated (its address may still be held); only its
         * id moves to this thread. */
        t->id = sync_retired->id;
        sync_retired = sync_retired->next_retired;
    } else {
        if (sync_next_thread_id > SYNC_MAX_THREAD_ID) {
            sync_mutex_unlock(&sync_table_lock);
            sync_fail("more threads alive at once than lock words can name", (long)sync_next_thread_id);
        }
        t->id = sync_next_thread_id++;
    }
    sync_mutex_unlock(&sync_table_lock);
    sync_self = t;
    return t;
}

void jabs_sync_thread_end(void) {
    jabs_sync_thread *t = sync_self;
    if (!t) return;
    /* An id may name a lock owner only while its thread holds a level somewhere; with none held
     * no header or monitor names it, so another thread may take it. */
    if (t->held != 0) sync_fail("a thread ended still holding monitor levels", (long)t->held);
    sync_mutex_lock(&sync_table_lock);
    t->next_retired = sync_retired;
    sync_retired = t;
    sync_mutex_unlock(&sync_table_lock);
    sync_self = NULL;
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

/* ---- Lock words ---- */

/* 0: unlocked and never inflated. Bit 0 set: inflated, monitor index (1-based) in bits 31..1.
 * Bit 0 clear, nonzero: thin lock, owner thread id in bits 31..12, recursion depth in bits 11..1
 * (1..2047). A word goes 0 <-> thin any number of times, and thin or 0 -> inflated once. */
#define SYNC_INFLATED 1u
#define SYNC_THIN_ID_SHIFT 12
#define SYNC_THIN_COUNT_SHIFT 1
#define SYNC_THIN_MAX_COUNT 2047u
/* Compare-and-set retries on one lock word before something must be wrong: each failure means
 * another thread changed the word, which a lock word survives only so many times in a row. */
#define SYNC_CAS_RETRIES 100000000u

static bool sync_is_inflated(uint32_t w) { return (w & SYNC_INFLATED) != 0; }
static uint32_t sync_thin_owner(uint32_t w) { return w >> SYNC_THIN_ID_SHIFT; }
static uint32_t sync_thin_count(uint32_t w) { return (w >> SYNC_THIN_COUNT_SHIFT) & SYNC_THIN_MAX_COUNT; }
static uint32_t sync_thin(uint32_t id, uint32_t count) {
    return (id << SYNC_THIN_ID_SHIFT) | (count << SYNC_THIN_COUNT_SHIFT);
}
static uint32_t sync_word(jabs_object *o) { return __atomic_load_n(&o->monitor, __ATOMIC_ACQUIRE); }
static bool sync_swap_word(jabs_object *o, uint32_t *expected, uint32_t desired, int order) {
    return __atomic_compare_exchange_n(&o->monitor, expected, desired, false, order, __ATOMIC_ACQUIRE);
}

/* ---- The monitor table: inflated monitors only ---- */

/* A thread in a monitor's wait set; lives on that thread's stack while it is in wait(), which
 * cannot return before it owns the monitor again, so whoever holds the monitor may touch it. */
typedef struct sync_waiter {
    struct sync_waiter *next;
    jabs_sync_thread *thread;
    bool notified; /* written under the monitor's mutex and the waiting thread's lock */
} sync_waiter;

/* Ownership is data guarded by `mutex` (taken only briefly), not the mutex itself: a thread that
 * inflates a thin lock must be able to make another thread the owner. */
typedef struct {
    sync_mutex mutex;
    sync_cond entry;           /* threads waiting to own the monitor */
    uint32_t owner;            /* thread id, 0 when free; read without the mutex only to compare
                                * with oneself (atomically) */
    uint32_t count;            /* recursion depth */
    uint32_t contenders;       /* threads asleep on `entry` */
    sync_waiter *first, *last; /* the wait set in arrival order */
    uint32_t waiting;          /* nodes in the wait set */
} sync_monitor;

/* A wait set holds at most one node per thread; far above any real program. */
#define SYNC_MAX_WAITERS 1000000u

#define SYNC_CHUNK_BITS 10
#define SYNC_CHUNK_SIZE (1u << SYNC_CHUNK_BITS)
/* 16384 chunks of 1024: up to 16M inflated monitors, i.e. objects ever contended, waited on or
 * locked more than 2047 levels deep. */
#define SYNC_MAX_CHUNKS 16384u
static sync_monitor *sync_chunks[SYNC_MAX_CHUNKS];
static uint32_t sync_monitors_used; /* guarded by sync_table_lock; read atomically for tests */

uint32_t jabs_sync_inflated_monitors(void) { return __atomic_load_n(&sync_monitors_used, __ATOMIC_ACQUIRE); }

/* The monitor at a 1-based index read from a lock word with acquire order: the chunk and the
 * monitor's fields were written before the word was published, so they are visible here. */
static sync_monitor *sync_monitor_at(uint32_t index) {
    if (index == 0) sync_fail("monitor index 0 looked up", 0);
    uint32_t slot = index - 1;
    if ((slot >> SYNC_CHUNK_BITS) >= SYNC_MAX_CHUNKS) sync_fail("monitor index out of range", (long)index);
    sync_monitor *chunk = sync_chunks[slot >> SYNC_CHUNK_BITS];
    if (!chunk) sync_fail("monitor index in a chunk never allocated", (long)index);
    return &chunk[slot & (SYNC_CHUNK_SIZE - 1)];
}

/* A fresh monitor's index: this thread's spare if it has one, else a new table slot. */
static uint32_t sync_monitor_new(jabs_sync_thread *self) {
    if (self->spare != 0) {
        uint32_t index = self->spare;
        self->spare = 0;
        return index;
    }
    sync_mutex_lock(&sync_table_lock);
    uint32_t slot = sync_monitors_used;
    uint32_t chunk = slot >> SYNC_CHUNK_BITS;
    if (chunk >= SYNC_MAX_CHUNKS) {
        sync_mutex_unlock(&sync_table_lock);
        sync_fail("more inflated monitors than the monitor table holds", (long)slot);
    }
    if (!sync_chunks[chunk]) {
        sync_chunks[chunk] = (sync_monitor *)calloc(SYNC_CHUNK_SIZE, sizeof(sync_monitor));
        if (!sync_chunks[chunk]) sync_fail("out of memory for monitors", (long)chunk);
    }
    sync_monitor *m = &sync_chunks[chunk][slot & (SYNC_CHUNK_SIZE - 1)];
    sync_mutex_init(&m->mutex);
    sync_cond_init(&m->entry);
    __atomic_store_n(&sync_monitors_used, slot + 1, __ATOMIC_RELEASE);
    sync_mutex_unlock(&sync_table_lock);
    return slot + 1;
}

/* The object's inflated monitor, inflating it now if needed. A thin lock's owner and depth move
 * into the monitor in the same compare-and-set that publishes it, so the owner (still running
 * its thin fast path) finds its own CAS failing and continues on the monitor at the right depth. */
static sync_monitor *sync_inflate(jabs_object *o, jabs_sync_thread *self) {
    uint32_t w = sync_word(o);
    for (uint32_t i = 0; i < SYNC_CAS_RETRIES; i++) {
        if (sync_is_inflated(w)) return sync_monitor_at(w >> 1);
        uint32_t index = sync_monitor_new(self);
        sync_monitor *m = sync_monitor_at(index);
        /* Not yet published: nobody else can see these fields. */
        m->owner = w == 0 ? 0 : sync_thin_owner(w);
        m->count = w == 0 ? 0 : sync_thin_count(w);
        if (sync_swap_word(o, &w, (index << 1) | SYNC_INFLATED, __ATOMIC_ACQ_REL)) return m;
        self->spare = index; /* the word changed: keep the monitor for the next inflation */
    }
    sync_fail("lock word kept changing while inflating", (long)w);
}

/* Relaxed is enough for an owner check: only a thread itself stores its own id as owner (or
 * an inflater does, publishing it with the word), so a thread always sees whether it is the
 * owner; any other value only tells it that it is not. */
static uint32_t sync_owner(sync_monitor *m) { return __atomic_load_n(&m->owner, __ATOMIC_RELAXED); }
static void sync_set_owner(sync_monitor *m, uint32_t id) { __atomic_store_n(&m->owner, id, __ATOMIC_RELAXED); }

/* Own an inflated monitor once more (blocking while another thread owns it). */
static void sync_inflated_enter(sync_monitor *m, jabs_sync_thread *self) {
    sync_mutex_lock(&m->mutex);
    if (m->owner == self->id) {
        if (m->count == UINT32_MAX) sync_fail("monitor recursion count overflow", (long)m->count);
        m->count++;
    } else {
        /* Ends when the owner releases and this thread gets in first; sleeps again otherwise. */
        while (m->owner != 0) {
            m->contenders++;
            sync_cond_wait(&m->entry, &m->mutex, -1);
            m->contenders--;
        }
        if (m->count != 0) sync_fail("free monitor with a recursion count", (long)m->count);
        sync_set_owner(m, self->id);
        m->count = 1;
    }
    sync_mutex_unlock(&m->mutex);
}

/* Give up every level of an owned inflated monitor, waking one contender; returns the depth.
 * The caller holds m->mutex. */
static uint32_t sync_release_all(sync_monitor *m) {
    uint32_t levels = m->count;
    m->count = 0;
    sync_set_owner(m, 0);
    if (m->contenders > 0) sync_cond_signal(&m->entry);
    return levels;
}

/* ---- monitorenter / monitorexit ---- */

void jabs_monitor_enter(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_enter(NULL): the caller must raise NullPointerException", 0);
    jabs_sync_thread *self = jabs_sync_current();
    uint32_t w = sync_word(o);
    for (uint32_t i = 0; i < SYNC_CAS_RETRIES; i++) {
        if (w == 0) {
            if (sync_swap_word(o, &w, sync_thin(self->id, 1), __ATOMIC_ACQUIRE)) { self->held++; return; }
            continue; /* w now holds what another thread put there */
        }
        if (!sync_is_inflated(w) && sync_thin_owner(w) == self->id && sync_thin_count(w) < SYNC_THIN_MAX_COUNT) {
            if (sync_swap_word(o, &w, w + (1u << SYNC_THIN_COUNT_SHIFT), __ATOMIC_ACQUIRE)) { self->held++; return; }
            continue; /* another thread inflated the lock under us */
        }
        /* Inflated, held by another thread (contention), or this thread's 2048th level. */
        sync_inflated_enter(sync_inflate(o, self), self);
        self->held++;
        return;
    }
    sync_fail("lock word kept changing in monitorenter", (long)w);
}

int jabs_monitor_exit(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_exit(NULL): the caller must raise NullPointerException", 0);
    jabs_sync_thread *self = jabs_sync_current();
    uint32_t w = sync_word(o);
    for (uint32_t i = 0; i < SYNC_CAS_RETRIES; i++) {
        if (w == 0) return 0;
        if (sync_is_inflated(w)) {
            sync_monitor *m = sync_monitor_at(w >> 1);
            sync_mutex_lock(&m->mutex);
            if (m->owner != self->id) { sync_mutex_unlock(&m->mutex); return 0; }
            if (m->count == 0) sync_fail("owned monitor with recursion count 0", 0);
            if (m->count == 1) sync_release_all(m);
            else m->count--;
            sync_mutex_unlock(&m->mutex);
            self->held--;
            return 1;
        }
        if (sync_thin_owner(w) != self->id) return 0;
        uint32_t count = sync_thin_count(w);
        uint32_t next = count == 1 ? 0 : w - (1u << SYNC_THIN_COUNT_SHIFT);
        if (sync_swap_word(o, &w, next, __ATOMIC_ACQ_REL)) { self->held--; return 1; }
        /* Another thread inflated the lock: go round and exit the monitor instead. */
    }
    sync_fail("lock word kept changing in monitorexit", (long)w);
}

int jabs_monitor_holds(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_holds(NULL): the caller must raise NullPointerException", 0);
    uint32_t w = sync_word(o);
    if (w == 0) return 0;
    uint32_t id = jabs_sync_current()->id;
    if (!sync_is_inflated(w)) return sync_thin_owner(w) == id;
    return sync_owner(sync_monitor_at(w >> 1)) == id;
}

/* ---- wait / notify ---- */

/* Take the first node out of the wait set and wake its thread; false when the set is empty.
 * The caller holds m->mutex and owns the monitor. */
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

/* notify (all = false) or notifyAll. A thin lock cannot have waiters (waiting inflates it), so
 * notifying one is only the ownership check, as in HotSpot; it is not inflated for that. */
static int sync_notify(jabs_object *o, bool all) {
    uint32_t w = sync_word(o);
    uint32_t id = jabs_sync_current()->id;
    if (w == 0) return 0;
    if (!sync_is_inflated(w)) return sync_thin_owner(w) == id;
    sync_monitor *m = sync_monitor_at(w >> 1);
    sync_mutex_lock(&m->mutex);
    if (m->owner != id) { sync_mutex_unlock(&m->mutex); return 0; }
    for (uint32_t i = 0; i <= SYNC_MAX_WAITERS; i++) {
        if (!sync_notify_first(m) || !all) { sync_mutex_unlock(&m->mutex); return 1; }
    }
    sync_fail("wait set longer than any thread count", (long)m->waiting);
}

int jabs_monitor_notify(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_notify(NULL): the caller must raise NullPointerException", 0);
    return sync_notify(o, false);
}

int jabs_monitor_notify_all(jabs_object *o) {
    if (!o) sync_fail("jabs_monitor_notify_all(NULL): the caller must raise NullPointerException", 0);
    return sync_notify(o, true);
}

/* Take a node that was not notified out of the wait set (its wait ended another way). The
 * caller holds m->mutex and owns the monitor. */
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
    /* The JVM checks ownership before a pending interrupt; neither needs a monitor. */
    if (!jabs_monitor_holds(o)) return JABS_WAIT_NOT_OWNER;
    if (jabs_sync_clear_interrupt(self)) return JABS_WAIT_INTERRUPTED;
    /* A wait set lives in a monitor: a thin lock becomes one, keeping its depth. */
    sync_monitor *m = sync_inflate(o, self);
    sync_mutex_lock(&m->mutex);
    if (m->owner != self->id) sync_fail("owner lost its monitor while inflating it for wait", (long)m->owner);
    if (m->waiting >= SYNC_MAX_WAITERS) sync_fail("wait set longer than any thread count", (long)m->waiting);

    sync_waiter node = { NULL, self, false };
    if (m->last) m->last->next = &node;
    else m->first = &node;
    m->last = &node;
    m->waiting++;
    /* Release every level; the depth comes back when this thread owns the monitor again. */
    uint32_t levels = sync_release_all(m);
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
    while (m->owner != 0) {
        m->contenders++;
        sync_cond_wait(&m->entry, &m->mutex, -1);
        m->contenders--;
    }
    sync_set_owner(m, self->id);
    m->count = levels;
    /* A notify may have picked this node after the wait ended another way (it saw the node
     * before this thread got the monitor back). Then the notification counts: otherwise it
     * would be lost to every other waiter. An interrupt consumed meanwhile stays pending, as
     * the JLS allows for a thread both notified and interrupted (17.2.4). */
    sync_mutex_lock(&self->lock);
    bool notified = node.notified;
    if (notified && outcome == JABS_WAIT_INTERRUPTED) self->interrupted = true;
    sync_mutex_unlock(&self->lock);
    if (!notified) sync_unlink(m, &node);
    sync_mutex_unlock(&m->mutex);
    return notified ? JABS_WAIT_NOTIFIED : outcome;
}
