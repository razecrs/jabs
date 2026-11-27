/* Java threads on OS threads: Thread.start0, the current thread, and a thread's end (join,
 * uncaught exceptions). Handwritten, OS-specific; generated code reaches it only through the
 * declarations in jabs_rt.h. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#include "jabs_runtime.h"

#ifdef _WIN32
#include <windows.h>
#include <process.h>
#else
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <time.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#endif
#endif

/* ---- OS primitives ---- */

#ifdef _WIN32
typedef SRWLOCK jabs_os_mutex;
typedef CONDITION_VARIABLE jabs_os_cond;
#define JABS_OS_MUTEX_INIT SRWLOCK_INIT
#define JABS_OS_COND_INIT CONDITION_VARIABLE_INIT
static void jabs_os_mutex_lock(jabs_os_mutex *m) { AcquireSRWLockExclusive(m); }
static void jabs_os_mutex_unlock(jabs_os_mutex *m) { ReleaseSRWLockExclusive(m); }
static void jabs_os_cond_wait(jabs_os_cond *c, jabs_os_mutex *m) {
    if (!SleepConditionVariableSRW(c, m, INFINITE, 0)) {
        fprintf(stderr, "jabs: SleepConditionVariableSRW failed (error %lu)\n", (unsigned long)GetLastError());
        abort();
    }
}
static void jabs_os_cond_broadcast(jabs_os_cond *c) { WakeAllConditionVariable(c); }
static bool jabs_os_sync_init(jabs_os_mutex *m, jabs_os_cond *c) {
    InitializeSRWLock(m);
    InitializeConditionVariable(c);
    return true;
}
static void jabs_os_sync_destroy(jabs_os_mutex *m, jabs_os_cond *c) { (void)m; (void)c; /* nothing to release */ }
/* Wait at most `nanos` (> 0); returns on timeout, wake-up or spuriously, so callers recheck. */
static void jabs_os_cond_wait_nanos(jabs_os_cond *c, jabs_os_mutex *m, int64_t nanos) {
    int64_t ms = nanos / 1000000 + (nanos % 1000000 != 0);
    DWORD wait = ms >= (int64_t)INFINITE ? INFINITE - 1 : (DWORD)ms;
    if (!SleepConditionVariableSRW(c, m, wait, 0) && GetLastError() != ERROR_TIMEOUT) {
        fprintf(stderr, "jabs: SleepConditionVariableSRW failed (error %lu)\n", (unsigned long)GetLastError());
        abort();
    }
}
void jabs_thread_yield(void) { (void)SwitchToThread(); /* FALSE only means nothing else was ready */ }
int32_t jabs_thread_processor_count(void) {
    DWORD count = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS);
    if (count == 0) { fprintf(stderr, "jabs: GetActiveProcessorCount failed (error %lu)\n", (unsigned long)GetLastError()); abort(); }
    return count > INT32_MAX ? INT32_MAX : (int32_t)count;
}
void jabs_thread_set_native_name(const char *name) {
    /* SetThreadDescription exists from Windows 10 1607 on; looked up so older systems still run. */
    typedef HRESULT(WINAPI * set_description)(HANDLE, PCWSTR);
    HMODULE kernel = GetModuleHandleW(L"kernel32.dll");
    set_description f = kernel ? (set_description)(void *)GetProcAddress(kernel, "SetThreadDescription") : NULL;
    if (!f) return;
    int n = MultiByteToWideChar(CP_UTF8, 0, name, -1, NULL, 0);
    wchar_t *wide = n > 0 ? (wchar_t *)malloc((size_t)n * sizeof(wchar_t)) : NULL;
    if (!wide) return;
    if (MultiByteToWideChar(CP_UTF8, 0, name, -1, wide, n) > 0) (void)f(GetCurrentThread(), wide); /* best effort */
    free(wide);
}

/* Every started thread gets this much stack. Thread's stackSize is a hint the JVM may ignore
 * (Thread javadoc); JABS frames are larger than HotSpot's, so a small requested size could
 * overflow where the JVM would not. 8 MiB is the Linux main thread's usual size; on Windows it is
 * only reserved address space until touched. */
#define JABS_THREAD_STACK_BYTES (8u * 1024u * 1024u)

typedef struct { void (*body)(void *); void *arg; } jabs_os_start;

static unsigned __stdcall jabs_os_thread_main(void *start) {
    jabs_os_start s = *(jabs_os_start *)start;
    free(start);
    s.body(s.arg);
    return 0;
}

/* Run body(arg) on a new OS thread nobody joins; false when the OS refuses. */
static bool jabs_os_thread_spawn(void (*body)(void *), void *arg) {
    jabs_os_start *start = (jabs_os_start *)malloc(sizeof *start);
    if (!start) return false;
    start->body = body;
    start->arg = arg;
    /* _beginthreadex rather than CreateThread: the C runtime's per-thread state must be set up. */
    uintptr_t handle = _beginthreadex(NULL, JABS_THREAD_STACK_BYTES, jabs_os_thread_main, start, STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
    if (handle == 0) { free(start); return false; }
    if (!CloseHandle((HANDLE)handle)) {
        fprintf(stderr, "jabs: CloseHandle of a new thread failed (error %lu)\n", (unsigned long)GetLastError());
        abort();
    }
    return true;
}
#else
typedef pthread_mutex_t jabs_os_mutex;
#define JABS_OS_MUTEX_INIT PTHREAD_MUTEX_INITIALIZER
static void jabs_os_mutex_lock(jabs_os_mutex *m) {
    int error = pthread_mutex_lock(m);
    if (error != 0) { fprintf(stderr, "jabs: pthread_mutex_lock failed (error %d)\n", error); abort(); }
}
static void jabs_os_mutex_unlock(jabs_os_mutex *m) {
    int error = pthread_mutex_unlock(m);
    if (error != 0) { fprintf(stderr, "jabs: pthread_mutex_unlock failed (error %d)\n", error); abort(); }
}
typedef pthread_cond_t jabs_os_cond;
#define JABS_OS_COND_INIT PTHREAD_COND_INITIALIZER
static void jabs_os_cond_wait(jabs_os_cond *c, jabs_os_mutex *m) {
    int error = pthread_cond_wait(c, m);
    if (error != 0) { fprintf(stderr, "jabs: pthread_cond_wait failed (error %d)\n", error); abort(); }
}
static void jabs_os_cond_broadcast(jabs_os_cond *c) {
    int error = pthread_cond_broadcast(c);
    if (error != 0) { fprintf(stderr, "jabs: pthread_cond_broadcast failed (error %d)\n", error); abort(); }
}

static bool jabs_os_sync_init(jabs_os_mutex *m, jabs_os_cond *c) {
    if (pthread_mutex_init(m, NULL) != 0) return false;
    pthread_condattr_t attr;
    if (pthread_condattr_init(&attr) != 0) { (void)pthread_mutex_destroy(m); return false; }
    int error = 0;
#ifndef __APPLE__
    /* Timed waits measure against the monotonic clock, so a wall-clock change cannot stretch or
     * cut a sleep. */
    error = pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
#endif
    if (error == 0) error = pthread_cond_init(c, &attr);
    if (pthread_condattr_destroy(&attr) != 0) { fprintf(stderr, "jabs: pthread_condattr_destroy failed\n"); abort(); }
    if (error != 0) { (void)pthread_mutex_destroy(m); return false; }
    return true;
}
static void jabs_os_sync_destroy(jabs_os_mutex *m, jabs_os_cond *c) {
    if (pthread_cond_destroy(c) != 0 || pthread_mutex_destroy(m) != 0) { fprintf(stderr, "jabs: destroying a thread's sleep lock failed\n"); abort(); }
}
/* Wait at most `nanos` (> 0); returns on timeout, wake-up or spuriously, so callers recheck. */
static void jabs_os_cond_wait_nanos(jabs_os_cond *c, jabs_os_mutex *m, int64_t nanos) {
    /* Cap each POSIX wait at 1s so large durations (e.g. INT64_MAX) never overflow time_t. */
    if (nanos > 1000000000LL) nanos = 1000000000LL;
    int error;
#ifdef __APPLE__
    struct timespec relative = { (time_t)(nanos / 1000000000), (long)(nanos % 1000000000) };
    error = pthread_cond_timedwait_relative_np(c, m, &relative);
#else
    struct timespec deadline;
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) { fprintf(stderr, "jabs: clock_gettime(CLOCK_MONOTONIC) failed\n"); abort(); }
    deadline.tv_sec += (time_t)(nanos / 1000000000);
    deadline.tv_nsec += (long)(nanos % 1000000000);
    if (deadline.tv_nsec >= 1000000000L) { deadline.tv_sec++; deadline.tv_nsec -= 1000000000L; }
    error = pthread_cond_timedwait(c, m, &deadline);
#endif
    if (error != 0 && error != ETIMEDOUT) { fprintf(stderr, "jabs: pthread_cond_timedwait failed (error %d)\n", error); abort(); }
}
void jabs_thread_yield(void) {
    if (sched_yield() != 0) { fprintf(stderr, "jabs: sched_yield failed\n"); abort(); }
}
int32_t jabs_thread_processor_count(void) {
    long count = sysconf(_SC_NPROCESSORS_ONLN);
    if (count < 1) { fprintf(stderr, "jabs: sysconf(_SC_NPROCESSORS_ONLN) failed\n"); abort(); }
    return count > INT32_MAX ? INT32_MAX : (int32_t)count;
}
void jabs_thread_set_native_name(const char *name) {
#ifdef __linux__
    /* The kernel keeps 15 bytes and a terminator, as HotSpot's os::set_native_thread_name. */
    char shortened[16];
    snprintf(shortened, sizeof shortened, "%s", name);
    (void)prctl(PR_SET_NAME, (unsigned long)(uintptr_t)shortened, 0UL, 0UL, 0UL); /* best effort */
#else
    (void)name;
#endif
}

/* See the Windows definition for why every thread gets this size. */
#define JABS_THREAD_STACK_BYTES (8u * 1024u * 1024u)

typedef struct { void (*body)(void *); void *arg; } jabs_os_start;

static void *jabs_os_thread_main(void *start) {
    jabs_os_start s = *(jabs_os_start *)start;
    free(start);
    s.body(s.arg);
    return NULL;
}

/* Run body(arg) on a new detached OS thread; false when the OS refuses. */
static bool jabs_os_thread_spawn(void (*body)(void *), void *arg) {
    jabs_os_start *start = (jabs_os_start *)malloc(sizeof *start);
    if (!start) return false;
    start->body = body;
    start->arg = arg;
    pthread_attr_t attr;
    int error = pthread_attr_init(&attr);
    if (error != 0) { free(start); return false; }
    error = pthread_attr_setstacksize(&attr, JABS_THREAD_STACK_BYTES);
    if (error == 0) error = pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    pthread_t thread;
    if (error == 0) error = pthread_create(&thread, &attr, jabs_os_thread_main, start);
    int destroyed = pthread_attr_destroy(&attr);
    if (destroyed != 0) { fprintf(stderr, "jabs: pthread_attr_destroy failed (error %d)\n", destroyed); abort(); }
    if (error != 0) { free(start); return false; }
    return true;
}
#endif

/* This thread's identity for owner fields: the address of a thread-local, distinct for every
 * thread alive at the same time. */
static _Thread_local char jabs_thread_tag;
static const void *jabs_thread_identity(void) { return &jabs_thread_tag; }

/* ---- Runtime locks for shared tables (see jabs_lock in jabs_rt.h) ---- */

_Static_assert(JABS_LOCK_COUNT == 4, "one initializer per runtime lock");
static jabs_os_mutex jabs_runtime_locks[JABS_LOCK_COUNT] = { JABS_OS_MUTEX_INIT, JABS_OS_MUTEX_INIT, JABS_OS_MUTEX_INIT, JABS_OS_MUTEX_INIT };

void jabs_lock(int which) {
    if (which < 0 || which >= JABS_LOCK_COUNT) { fprintf(stderr, "jabs: jabs_lock(%d): no such runtime lock\n", which); abort(); }
    jabs_os_mutex_lock(&jabs_runtime_locks[which]);
}

void jabs_unlock(int which) {
    if (which < 0 || which >= JABS_LOCK_COUNT) { fprintf(stderr, "jabs: jabs_unlock(%d): no such runtime lock\n", which); abort(); }
    jabs_os_mutex_unlock(&jabs_runtime_locks[which]);
}

/* ---- Class initialization (JVMS 5.5) ---- */

/* One lock and one condition for every class. The lock is held only to read or change states,
 * never while <clinit> runs, so it is never held for long; every end of an initialization wakes
 * all waiters and each rechecks its own class. */
static jabs_os_mutex jabs_init_mutex = JABS_OS_MUTEX_INIT;
static jabs_os_cond jabs_init_done = JABS_OS_COND_INIT;
/* Classes whose initialization completed (VMManagementImpl.getInitializedClassCount); changed and
 * read under jabs_init_mutex. */
static int64_t jabs_classes_initialized;

int64_t jabs_class_initialized_count(void) {
    jabs_os_mutex_lock(&jabs_init_mutex);
    int64_t count = jabs_classes_initialized;
    jabs_os_mutex_unlock(&jabs_init_mutex);
    return count;
}

int jabs_class_init_begin(jabs_class_init *c) {
    const void *me = jabs_thread_identity();
    jabs_os_mutex_lock(&jabs_init_mutex);
    /* Step 2: wait while another thread initializes. No cap: a <clinit> may legitimately take as
     * long as it takes, and the JVM waits too. */
    while (atomic_load_explicit(&c->state, memory_order_relaxed) == JABS_CLASS_INITIALIZING && c->owner != me) {
        jabs_os_cond_wait(&jabs_init_done, &jabs_init_mutex);
    }
    int32_t state = atomic_load_explicit(&c->state, memory_order_relaxed);
    switch (state) {
    case JABS_CLASS_INITIALIZED:
    case JABS_CLASS_INITIALIZING: /* step 3: this thread's own initialization in progress (recursive request) */
        jabs_os_mutex_unlock(&jabs_init_mutex);
        return JABS_INIT_DONE;
    case JABS_CLASS_ERRONEOUS:
        /* Step 5. Raised outside the lock: creating the exception may initialize its class. */
        jabs_os_mutex_unlock(&jabs_init_mutex);
        jabs_pending = jabs_vm_throwable(JABS_VM_NO_CLASS_DEF_FOUND);
        return JABS_INIT_FAILED;
    case JABS_CLASS_UNINITIALIZED:
        /* Step 6: this thread initializes it. */
        c->owner = me;
        atomic_store_explicit(&c->state, JABS_CLASS_INITIALIZING, memory_order_relaxed);
        jabs_os_mutex_unlock(&jabs_init_mutex);
        return JABS_INIT_RUN;
    default:
        jabs_os_mutex_unlock(&jabs_init_mutex);
        fprintf(stderr, "jabs: class initialization state %d is not a state (memory corruption?)\n", (int)state);
        abort();
    }
}

/* The end of a class's initialization, or (counted false) of a once-only runtime step that uses
 * the same gate without being a class (jabs_runtime_once_end). */
static void jabs_init_gate_end(jabs_class_init *c, bool ok, bool counted) {
    const void *me = jabs_thread_identity();
    jabs_os_mutex_lock(&jabs_init_mutex);
    if (atomic_load_explicit(&c->state, memory_order_relaxed) != JABS_CLASS_INITIALIZING || c->owner != me) {
        jabs_os_mutex_unlock(&jabs_init_mutex);
        fprintf(stderr, "jabs: jabs_init_gate_end on a gate this thread is not initializing\n");
        abort();
    }
    c->owner = NULL;
    if (ok && counted) {
        if (jabs_classes_initialized == INT64_MAX) { jabs_os_mutex_unlock(&jabs_init_mutex); fprintf(stderr, "jabs: initialized class count overflow\n"); abort(); }
        jabs_classes_initialized++;
    }
    /* Steps 10 and 11: release pairs with the fast path's acquire load in jabs_class_initialized,
     * so a thread that sees INITIALIZED also sees every static <clinit> wrote. */
    atomic_store_explicit(&c->state, ok ? JABS_CLASS_INITIALIZED : JABS_CLASS_ERRONEOUS, memory_order_release);
    jabs_os_cond_broadcast(&jabs_init_done);
    jabs_os_mutex_unlock(&jabs_init_mutex);
}

void jabs_class_init_end(jabs_class_init *c, bool ok) {
    jabs_init_gate_end(c, ok, true);
}

void jabs_runtime_once_end(jabs_class_init *c, bool ok) {
    jabs_init_gate_end(c, ok, false);
}

/* ---- Threads and the current thread ---- */

/* What the runtime keeps per Java thread beside its Thread object. Thread.eetop points at it while
 * the thread is alive. Never freed, like every Java object without a GC. */
typedef struct jabs_thread_record {
    jabs_object *thread;
    /* Counted in jabs_live_non_daemon while it runs (started non-daemon threads only). */
    bool counted;
    /* Counted in the management counts (jabs_mgmt_live, and jabs_mgmt_daemon when daemon) from
     * start0 until its end; daemon is fixed at start, as Thread.setDaemon refuses a started thread. */
    bool live;
    bool daemon;
    /* Seeds this thread's identity-hash sequence. */
    uint32_t number;
    /* The monitors' record of this OS thread (for interrupting Object.wait); NULL until the
     * thread runs. Set and read under jabs_threads_mutex. */
    jabs_sync_thread *sync;
    /* Sleeping and Unsafe.park wait on sleep_wake; interrupt0 and unpark signal it. */
    jabs_os_mutex sleep_lock;
    jabs_os_cond sleep_wake;
    bool park_permit;
} jabs_thread_record;

/* Guards the main record's attachment, every thread's start and end (threadStatus, eetop) and
 * the count of running non-daemon threads, whose reaching zero is signalled on jabs_no_live. */
static jabs_os_mutex jabs_threads_mutex = JABS_OS_MUTEX_INIT;
static jabs_os_cond jabs_no_live = JABS_OS_COND_INIT;
static uint32_t jabs_live_non_daemon;
static uint32_t jabs_threads_started;
/* The main thread is live (for java.lang.management) from the start: it runs before any count is
 * read, as HotSpot's main JavaThread does. */
static jabs_thread_record jabs_main_record = { .live = true };
/* java.lang.management's thread counts, as HotSpot's ThreadService keeps them: a thread counts
 * from Thread.start0 (before it returns) and stops counting at its end before join() waiters
 * wake, so a joined thread is never live. Started counts every thread ever started plus main.
 * All four change and are read only under jabs_threads_mutex; they are bounded by
 * jabs_threads_started (UINT32_MAX), so int64_t never overflows. */
static int64_t jabs_mgmt_live = 1, jabs_mgmt_daemon, jabs_mgmt_peak = 1, jabs_mgmt_started = 1;

jabs_thread_counts jabs_thread_management_counts(void) {
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_thread_counts counts = { jabs_mgmt_live, jabs_mgmt_daemon, jabs_mgmt_peak, jabs_mgmt_started };
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    return counts;
}

void jabs_thread_reset_peak(void) {
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_mgmt_peak = jabs_mgmt_live;
    jabs_os_mutex_unlock(&jabs_threads_mutex);
}
static bool jabs_main_attached;
static _Thread_local jabs_thread_record *jabs_self;

jabs_object *jabs_current_thread(void) {
    jabs_thread_record *self = jabs_self;
    if (self) return self->thread;
    /* Only the main thread has no record yet: its Thread is built on this first request. */
    return jabs_main_thread();
}

void jabs_thread_attach_main(jabs_object *thread) {
    if (!thread) { fprintf(stderr, "jabs: jabs_thread_attach_main(NULL)\n"); abort(); }
    if (jabs_self) { fprintf(stderr, "jabs: the main Thread was built on a thread that already has one\n"); abort(); }
    jabs_os_mutex_lock(&jabs_threads_mutex);
    if (jabs_main_attached) {
        jabs_os_mutex_unlock(&jabs_threads_mutex);
        fprintf(stderr, "jabs: a second OS thread asked for the main Thread; only the main thread and threads started by Thread.start run Java code\n");
        abort();
    }
    jabs_main_attached = true;
    if (!jabs_os_sync_init(&jabs_main_record.sleep_lock, &jabs_main_record.sleep_wake)) {
        jabs_os_mutex_unlock(&jabs_threads_mutex);
        fprintf(stderr, "jabs: cannot create the main thread's sleep lock\n");
        abort();
    }
    jabs_main_record.thread = thread;
    jabs_main_record.sync = jabs_sync_current();
    __atomic_store_n(jabs_thread_eetop_slot(thread), (int64_t)(intptr_t)&jabs_main_record, __ATOMIC_SEQ_CST);
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    jabs_self = &jabs_main_record;
}

/* Thread.eetop, holder.threadStatus and Thread.interrupted are volatile Java fields, which
 * compiled Java code accesses as sequentially consistent atomics; the runtime does the same. */
static int64_t jabs_get_eetop(jabs_object *t) { return __atomic_load_n(jabs_thread_eetop_slot(t), __ATOMIC_SEQ_CST); }
static void jabs_set_eetop(jabs_object *t, int64_t v) { __atomic_store_n(jabs_thread_eetop_slot(t), v, __ATOMIC_SEQ_CST); }
static int32_t jabs_get_status(int32_t *slot) { return __atomic_load_n(slot, __ATOMIC_SEQ_CST); }
static void jabs_set_status(int32_t *slot, int32_t v) { __atomic_store_n(slot, v, __ATOMIC_SEQ_CST); }
static bool jabs_get_interrupted(jabs_object *t) { return __atomic_load_n(jabs_thread_interrupted_slot(t), __ATOMIC_SEQ_CST) != 0; }
static void jabs_clear_interrupted(jabs_object *t) { __atomic_store_n(jabs_thread_interrupted_slot(t), (uint8_t)0, __ATOMIC_SEQ_CST); }

/* JVMTI thread states, as java.lang.Thread$FieldHolder.threadStatus holds them. */
enum {
    JABS_THREAD_NEW = 0,
    JABS_THREAD_RUNNABLE = 0x0005 /* alive | runnable */,
    JABS_THREAD_WAITING = 0x0291 /* alive | waiting | waiting_indefinitely | parked */,
    JABS_THREAD_TIMED_WAITING = 0x02A1 /* alive | waiting | waiting_with_timeout | parked */,
    JABS_THREAD_TERMINATED = 0x0002
};

/* The end of a thread, as HotSpot's ensure_join: under the Thread's monitor, TERMINATED and no
 * longer alive, then every join() waiter woken; then it no longer counts as running. */
static void jabs_thread_terminate(jabs_thread_record *record) {
    jabs_object *thread = record->thread;
    int32_t *status = jabs_thread_status_slot(thread);
    if (!status) { fprintf(stderr, "jabs: a running Thread lost its FieldHolder\n"); abort(); }
    jabs_monitor_enter(thread);
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_set_status(status, JABS_THREAD_TERMINATED);
    jabs_set_eetop(thread, 0);
    /* No longer live for management, in the same step that makes it not alive: whoever sees it
     * TERMINATED (join) then reads counts without it, as the reference JVM reports after a join
     * (tests/management.rs, threadCounts). */
    if (record->live) {
        if (jabs_mgmt_live <= 0 || (record->daemon && jabs_mgmt_daemon <= 0)) { jabs_os_mutex_unlock(&jabs_threads_mutex); fprintf(stderr, "jabs: live thread count underflow\n"); abort(); }
        record->live = false;
        jabs_mgmt_live--;
        if (record->daemon) jabs_mgmt_daemon--;
    }
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    if (!jabs_monitor_notify_all(thread) || !jabs_monitor_exit(thread)) {
        fprintf(stderr, "jabs: a terminating thread does not own its Thread's monitor\n");
        abort();
    }
    jabs_os_mutex_lock(&jabs_threads_mutex);
    if (record->counted) {
        if (jabs_live_non_daemon == 0) { jabs_os_mutex_unlock(&jabs_threads_mutex); fprintf(stderr, "jabs: running non-daemon thread count underflow\n"); abort(); }
        record->counted = false;
        jabs_live_non_daemon--;
        if (jabs_live_non_daemon == 0) jabs_os_cond_broadcast(&jabs_no_live);
    }
    jabs_os_mutex_unlock(&jabs_threads_mutex);
}

/* A started thread: run(), then what HotSpot's JavaThread::exit does. Exceptions thrown by the
 * uncaught-exception handler or by Thread.exit are discarded, as HotSpot discards them. */
static void jabs_thread_main(void *arg) {
    jabs_thread_record *record = (jabs_thread_record *)arg;
    jabs_self = record;
    /* An interrupt that came before the monitors knew this thread only set the Java field
     * (interrupt0 found no sync record): carry it over, under the lock interrupt0 reads with. */
    jabs_sync_thread *sync = jabs_sync_current();
    jabs_os_mutex_lock(&jabs_threads_mutex);
    record->sync = sync;
    bool interrupted_early = jabs_get_interrupted(record->thread);
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    if (interrupted_early) jabs_sync_interrupt(sync);
    /* Distinct per thread, never 0 (xorshift's fixed point). */
    jabs_hash_next = 0x9e3779b9u ^ (record->number * 0x85ebca6bu);
    if (jabs_hash_next == 0) jabs_hash_next = 1;
    jabs_object *thread = record->thread;
    jabs_thread_run_java(thread);
    jabs_object *uncaught = jabs_pending;
    jabs_pending = NULL;
    if (uncaught && !jabs_thread_dispatch_uncaught(thread, uncaught)) {
        jabs_not_compiled("Thread.dispatchUncaughtException (an exception escaped a thread's run())");
    }
    jabs_pending = NULL;
    if (!jabs_thread_exit_java(thread)) jabs_not_compiled("Thread.exit (the end of a started thread)");
    jabs_pending = NULL;
    jabs_thread_terminate(record);
    /* Last: the thread holds no monitor level now (terminate released the Thread's), so its lock
     * id can go to a new thread. Without this, ids are never reused and about a million threads
     * over a program's life would exhaust them. The sync record itself stays allocated, so a late
     * interrupt through record->sync still finds valid memory. */
    jabs_sync_thread_end();
}

void jabs_thread_start(jabs_object *thread) {
    if (!thread) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    int32_t *status = jabs_thread_status_slot(thread);
    if (!status) jabs_not_compiled("Thread.start0 of a Thread without a FieldHolder (a virtual thread?)");
    bool daemon = jabs_thread_is_daemon(thread);
    jabs_thread_record *record = (jabs_thread_record *)calloc(1, sizeof *record);
    if (!record) { jabs_thread_raise(JABS_VM_OUT_OF_MEMORY, "unable to create native thread: out of memory"); return; }
    if (!jabs_os_sync_init(&record->sleep_lock, &record->sleep_wake)) {
        free(record);
        jabs_thread_raise(JABS_VM_OUT_OF_MEMORY, "unable to create native thread: no sleep lock");
        return;
    }
    record->thread = thread;
    jabs_os_mutex_lock(&jabs_threads_mutex);
    /* Thread.start checked threadStatus under the Thread's monitor, so this cannot be alive. */
    if (jabs_get_eetop(thread) != 0 || jabs_get_status(status) != JABS_THREAD_NEW) {
        jabs_os_mutex_unlock(&jabs_threads_mutex);
        fprintf(stderr, "jabs: Thread.start0 on a thread that was already started (status %d)\n", (int)jabs_get_status(status));
        abort();
    }
    if (jabs_threads_started == UINT32_MAX) {
        jabs_os_mutex_unlock(&jabs_threads_mutex);
        jabs_os_sync_destroy(&record->sleep_lock, &record->sleep_wake);
        free(record);
        jabs_thread_raise(JABS_VM_OUT_OF_MEMORY, "unable to create native thread: thread counter exhausted");
        return;
    }
    record->number = ++jabs_threads_started;
    record->counted = !daemon;
    /* Alive and RUNNABLE before start() returns, as HotSpot's JVM_StartThread does. */
    jabs_set_status(status, JABS_THREAD_RUNNABLE);
    jabs_set_eetop(thread, (int64_t)(intptr_t)record);
    if (record->counted) jabs_live_non_daemon++;
    /* Live for management before start() returns, like alive: counted from here, never late. */
    int64_t peak_before = jabs_mgmt_peak;
    record->live = true;
    record->daemon = daemon;
    jabs_mgmt_live++;
    if (daemon) jabs_mgmt_daemon++;
    jabs_mgmt_started++;
    if (jabs_mgmt_live > jabs_mgmt_peak) jabs_mgmt_peak = jabs_mgmt_live;
    int64_t peak_set = jabs_mgmt_peak;
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    if (jabs_os_thread_spawn(jabs_thread_main, record)) return;
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_set_status(status, JABS_THREAD_NEW);
    jabs_set_eetop(thread, 0);
    if (record->counted) {
        jabs_live_non_daemon--;
        if (jabs_live_non_daemon == 0) jabs_os_cond_broadcast(&jabs_no_live);
    }
    /* The thread never ran: it was never started for management either. A peak this start raised
     * goes back; if another thread started meanwhile the peak may stay one too high (accepted:
     * it needs the OS to refuse a thread while another starts). */
    jabs_mgmt_live--;
    if (daemon) jabs_mgmt_daemon--;
    jabs_mgmt_started--;
    record->live = false;
    if (jabs_mgmt_peak == peak_set && peak_set > peak_before) jabs_mgmt_peak = peak_before > jabs_mgmt_live ? peak_before : jabs_mgmt_live;
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    jabs_os_sync_destroy(&record->sleep_lock, &record->sleep_wake);
    free(record);
    jabs_thread_raise(JABS_VM_OUT_OF_MEMORY, "unable to create native thread: possibly out of memory or process/resource limits reached");
}

void jabs_destroy_java_vm(void) {
    jabs_pending = NULL;
    if (jabs_self == &jabs_main_record) {
        /* Thread.exit is in the program whenever another thread could observe this one's end
         * (start0 keeps it reachable); without it there is nobody to tell. */
        if (jabs_thread_exit_java(jabs_main_record.thread)) {
            jabs_pending = NULL;
            jabs_thread_terminate(&jabs_main_record);
        }
    }
    jabs_os_mutex_lock(&jabs_threads_mutex);
    /* No cap: the JVM waits for its non-daemon threads however long they run. */
    while (jabs_live_non_daemon > 0) jabs_os_cond_wait(&jabs_no_live, &jabs_threads_mutex);
    jabs_os_mutex_unlock(&jabs_threads_mutex);
}

void jabs_thread_sleep(int64_t nanos) {
    jabs_object *thread = jabs_current_thread();
    if (!thread) return; /* building the main Thread failed: its exception is pending */
    jabs_thread_record *self = jabs_self;
    if (!self || self->thread != thread) { fprintf(stderr, "jabs: sleeping thread has no record\n"); abort(); }
    int64_t start = jabs_nano_time();
    bool woken_by_interrupt = false;
    jabs_os_mutex_lock(&self->sleep_lock);
    /* Not capped by a count: each pass either ends (deadline or interrupt) or waits for the
     * remaining time, and the clock only moves forward. interrupt0 signals under sleep_lock after
     * Thread.interrupt set the flag, so a check under the lock cannot miss it. */
    for (;;) {
        if (jabs_get_interrupted(thread)) {
            /* HotSpot's JVM_SleepNanos clears the status when it throws. */
            jabs_clear_interrupted(thread);
            woken_by_interrupt = true;
            break;
        }
        int64_t elapsed = jabs_nano_time() - start;
        if (nanos <= 0 || elapsed >= nanos) break;
        jabs_os_cond_wait_nanos(&self->sleep_wake, &self->sleep_lock, nanos - elapsed);
    }
    jabs_os_mutex_unlock(&self->sleep_lock);
    if (woken_by_interrupt) {
        /* The same interrupt also marked the monitors' record; it is consumed here. */
        if (self->sync) (void)jabs_sync_clear_interrupt(self->sync);
        jabs_thread_raise(JABS_VM_INTERRUPTED, "sleep interrupted");
        return;
    }
    if (nanos <= 0) jabs_thread_yield();
}

void jabs_thread_interrupt(jabs_object *thread) {
    if (!thread) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    /* Keep jabs_threads_mutex -> sleep_lock order so a concurrent failed start0 cannot destroy
     * and free target while sleep_wake is being signalled. */
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_thread_record *target = (jabs_thread_record *)(intptr_t)jabs_get_eetop(thread);
    jabs_sync_thread *sync = target ? target->sync : NULL;
    if (target) {
        jabs_os_mutex_lock(&target->sleep_lock);
        jabs_os_cond_broadcast(&target->sleep_wake);
        jabs_os_mutex_unlock(&target->sleep_lock);
    }
    jabs_os_mutex_unlock(&jabs_threads_mutex);
    /* Not alive: the Java field Thread.interrupt set is the whole interrupt status. */
    if (!target) return;
    /* Wakes it from Object.wait (or makes its next wait end at once). NULL: it has not run yet,
     * and jabs_thread_main carries the Java field over when it does. */
    if (sync) jabs_sync_interrupt(sync);
}

void jabs_thread_clear_interrupt_event(void) {
    (void)jabs_sync_clear_interrupt(jabs_sync_current());
}

void jabs_thread_wait_interrupted(void) {
    jabs_object *thread = jabs_current_thread();
    if (!thread) return; /* building the main Thread failed: its exception is pending */
    jabs_clear_interrupted(thread);
    jabs_pending = jabs_vm_throwable(JABS_VM_INTERRUPTED);
}

static bool jabs_is_thread_object(jabs_object *o) {
    for (const jabs_class *c = o->cls; c; c = c->super) {
        if (strcmp(c->name, "java/lang/Thread") == 0) return true;
    }
    return false;
}

void jabs_thread_park(int32_t is_absolute, int64_t time) {
    jabs_object *thread = jabs_current_thread();
    if (!thread) return; /* building the main Thread failed: its exception is pending */
    jabs_thread_record *self = jabs_self;
    if (!self || self->thread != thread) { fprintf(stderr, "jabs: parking thread has no record\n"); abort(); }
    if (time < 0 || (is_absolute && time == 0)) return;
    int64_t start = is_absolute ? 0 : jabs_nano_time();
    int32_t *status = jabs_thread_status_slot(thread);
    int32_t parked_state = (!is_absolute && time == 0) ? JABS_THREAD_WAITING : JABS_THREAD_TIMED_WAITING;
    jabs_os_mutex_lock(&self->sleep_lock);
    for (;;) {
        if (self->park_permit) {
            self->park_permit = false;
            break;
        }
        if (jabs_get_interrupted(thread)) break;
        if (status) jabs_set_status(status, parked_state);
        if (is_absolute) {
            int64_t now_ms = jabs_current_millis();
            if (now_ms >= time) break;
            uint64_t rem_ms = (uint64_t)time - (uint64_t)now_ms;
            /* Wait in bounded chunks so wall-clock adjustments are observed promptly. */
            uint64_t chunk_ms = rem_ms > 1000 ? 1000 : rem_ms;
            jabs_os_cond_wait_nanos(&self->sleep_wake, &self->sleep_lock, (int64_t)(chunk_ms * 1000000ULL));
        } else if (time == 0) {
            jabs_os_cond_wait(&self->sleep_wake, &self->sleep_lock);
        } else {
            int64_t now = jabs_nano_time();
            uint64_t elapsed = now >= start ? (uint64_t)now - (uint64_t)start : 0;
            if (elapsed >= (uint64_t)time) break;
            uint64_t rem = (uint64_t)time - elapsed;
            int64_t chunk = rem > 1000000000ULL ? 1000000000LL : (int64_t)rem;
            jabs_os_cond_wait_nanos(&self->sleep_wake, &self->sleep_lock, chunk);
        }
    }
    if (status) jabs_set_status(status, JABS_THREAD_RUNNABLE);
    jabs_os_mutex_unlock(&self->sleep_lock);
}

void jabs_thread_unpark(jabs_object *thread) {
    if (!thread || !jabs_is_thread_object(thread)) return;
    /* Keep jabs_threads_mutex -> sleep_lock order; sleep_lock never takes jabs_threads_mutex,
     * and holding jabs_threads_mutex prevents a failed Thread.start0 from freeing target. */
    jabs_os_mutex_lock(&jabs_threads_mutex);
    jabs_thread_record *target = (jabs_thread_record *)(intptr_t)jabs_get_eetop(thread);
    if (!target && jabs_main_attached && thread == jabs_main_record.thread && jabs_main_record.live) {
        target = &jabs_main_record;
    }
    if (target) {
        jabs_os_mutex_lock(&target->sleep_lock);
        target->park_permit = true;
        jabs_os_cond_broadcast(&target->sleep_wake);
        jabs_os_mutex_unlock(&target->sleep_lock);
    }
    jabs_os_mutex_unlock(&jabs_threads_mutex);
}
