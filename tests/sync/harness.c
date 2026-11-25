/* C-level tests of runtime/jabs_sync.c alone: raw OS threads hammer the monitor functions on
 * plain jabs_objects, with no Java code involved. Run as `harness <test name>`; tests/sync.rs
 * runs each test in its own process under a watchdog. A failure prints what broke, where to
 * look, and expected vs actual, then exits with status 1. */
#include "jabs_unit.h"
#include "os_threads.h"

static const char *current_test = "?";
static int failed;

/* Only the first failing thread reports; the others park until it ends the process. */
static void claim_failure(void) {
    if (!__atomic_exchange_n(&failed, 1, __ATOMIC_SEQ_CST)) return;
    for (;;) harness_sleep_ms(1000);
}

#define CHECK(cond, where, ...)                                                        \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            claim_failure();                                                           \
            fprintf(stderr, "FAIL %s: ", current_test);                                \
            fprintf(stderr, __VA_ARGS__);                                              \
            fprintf(stderr, "\n  look at: %s\n  (%s:%d: %s)\n", where, __FILE__, __LINE__, #cond); \
            fflush(stderr);                                                            \
            exit(1);                                                                   \
        }                                                                              \
    } while (0)

static const jabs_class probe_class = { "SyncProbe", sizeof(jabs_object), NULL, NULL, NULL, 0, NULL, 0, 0, 0 };

static jabs_object *fresh_object(void) {
    jabs_object *o = (jabs_object *)calloc(1, sizeof *o);
    if (!o) { fprintf(stderr, "harness: out of memory\n"); exit(2); }
    o->cls = &probe_class;
    return o;
}

/* Spin until `count` threads have arrived, so they start their work together. */
static void arrive_and_wait(int *arrived, int count) {
    __atomic_add_fetch(arrived, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(arrived, __ATOMIC_SEQ_CST) < count) harness_sleep_ms(0);
}

#define ENTER_EXIT "jabs_monitor_enter/jabs_monitor_exit in runtime/jabs_sync.c"

/* ---- monitor_excludes_concurrent_owners ---- */

enum { EXCLUSION_THREADS = 8, EXCLUSION_ROUNDS = 20000 };
typedef struct {
    jabs_object *lock;
    int arrived;
    int inside;  /* threads between enter and exit: must never exceed 1 */
    long counter; /* incremented without atomics, only under the monitor */
} exclusion;

static void exclusion_worker(void *p) {
    exclusion *e = (exclusion *)p;
    arrive_and_wait(&e->arrived, EXCLUSION_THREADS);
    for (int i = 0; i < EXCLUSION_ROUNDS; i++) {
        jabs_monitor_enter(e->lock);
        int others = __atomic_fetch_add(&e->inside, 1, __ATOMIC_RELAXED);
        CHECK(others == 0, ENTER_EXIT, "two threads own the monitor at once: %d other owner(s) inside", others);
        e->counter++;
        __atomic_fetch_sub(&e->inside, 1, __ATOMIC_RELAXED);
        CHECK(jabs_monitor_exit(e->lock) == 1, ENTER_EXIT, "exit by the owner was refused");
    }
}

static void monitor_excludes_concurrent_owners(void) {
    exclusion e = { fresh_object(), 0, 0, 0 };
    harness_thread *t[EXCLUSION_THREADS];
    for (int i = 0; i < EXCLUSION_THREADS; i++) t[i] = harness_spawn(exclusion_worker, &e);
    for (int i = 0; i < EXCLUSION_THREADS; i++) harness_join(t[i]);
    long expected = (long)EXCLUSION_THREADS * EXCLUSION_ROUNDS;
    CHECK(e.counter == expected, ENTER_EXIT, "lost updates under the monitor: expected %ld, got %ld", expected, e.counter);
}

/* ---- monitor_first_use_race_installs_one_monitor ---- */

enum { RACE_THREADS = 8, RACE_OBJECTS = 200 };
typedef struct {
    jabs_object *objects[RACE_OBJECTS];
    int arrived[RACE_OBJECTS];
    int inside[RACE_OBJECTS];
} first_use;

static void first_use_worker(void *p) {
    first_use *f = (first_use *)p;
    for (int i = 0; i < RACE_OBJECTS; i++) {
        arrive_and_wait(&f->arrived[i], RACE_THREADS);
        jabs_monitor_enter(f->objects[i]);
        int others = __atomic_fetch_add(&f->inside[i], 1, __ATOMIC_RELAXED);
        CHECK(others == 0, "sync_monitor_of in runtime/jabs_sync.c (two monitors installed for one object?)",
              "object %d: two threads own its monitor at once after racing to lock it first", i);
        __atomic_fetch_sub(&f->inside[i], 1, __ATOMIC_RELAXED);
        CHECK(jabs_monitor_exit(f->objects[i]) == 1, ENTER_EXIT, "object %d: exit by the owner was refused", i);
    }
}

static void monitor_first_use_race_installs_one_monitor(void) {
    static first_use f;
    for (int i = 0; i < RACE_OBJECTS; i++) f.objects[i] = fresh_object();
    harness_thread *t[RACE_THREADS];
    for (int i = 0; i < RACE_THREADS; i++) t[i] = harness_spawn(first_use_worker, &f);
    for (int i = 0; i < RACE_THREADS; i++) harness_join(t[i]);
    for (int i = 0; i < RACE_OBJECTS; i++) {
        uint32_t a = f.objects[i]->monitor;
        CHECK(a != 0, "sync_monitor_of in runtime/jabs_sync.c", "object %d: header has no monitor index after being locked", i);
        for (int j = 0; j < i; j++) {
            CHECK(f.objects[j]->monitor != a, "sync_monitor_of in runtime/jabs_sync.c",
                  "objects %d and %d share monitor index %u", j, i, (unsigned)a);
        }
    }
}

/* ---- monitor_reentry_counts_levels ---- */

static void monitor_reentry_counts_levels(void) {
    jabs_object *o = fresh_object();
    for (int i = 0; i < 3; i++) jabs_monitor_enter(o);
    for (int i = 3; i > 0; i--) {
        CHECK(jabs_monitor_holds(o), ENTER_EXIT, "owner lost the monitor with %d level(s) still entered", i);
        CHECK(jabs_monitor_exit(o) == 1, ENTER_EXIT, "exit with %d level(s) entered: expected 1, got 0", i);
    }
    CHECK(!jabs_monitor_holds(o), ENTER_EXIT, "owner not cleared on the final exit");
    CHECK(jabs_monitor_exit(o) == 0, ENTER_EXIT, "an exit beyond the entered levels: expected 0 (IllegalMonitorStateException), got 1");
}

/* ---- monitor_unlocked_object_is_not_owned ---- */

static void monitor_unlocked_object_is_not_owned(void) {
    jabs_object *o = fresh_object();
    CHECK(!jabs_monitor_holds(o), "jabs_monitor_holds in runtime/jabs_sync.c", "a never-locked object reports an owner");
    CHECK(jabs_monitor_exit(o) == 0, ENTER_EXIT, "exit of a never-locked object: expected 0, got 1");
    CHECK(o->monitor == 0, "sync_monitor_if_any in runtime/jabs_sync.c",
          "queries on a never-locked object installed monitor %u; only locking should", (unsigned)o->monitor);
}

/* ---- monitor_exit_by_non_owner_is_refused / monitor_blocks_until_owner_exits ---- */

typedef struct {
    jabs_object *lock;
    int owner_released; /* set by the owner just before its final exit */
    int other_result;
} handoff;

static void non_owner_worker(void *p) {
    handoff *h = (handoff *)p;
    CHECK(!jabs_monitor_holds(h->lock), "jabs_monitor_holds in runtime/jabs_sync.c", "a thread that never entered sees itself as owner");
    h->other_result = jabs_monitor_exit(h->lock);
}

static void monitor_exit_by_non_owner_is_refused(void) {
    handoff h = { fresh_object(), 0, -1 };
    jabs_monitor_enter(h.lock);
    harness_join(harness_spawn(non_owner_worker, &h));
    CHECK(h.other_result == 0, ENTER_EXIT, "exit by a thread that does not own the monitor: expected 0, got %d", h.other_result);
    CHECK(jabs_monitor_holds(h.lock), ENTER_EXIT, "the owner lost the monitor when another thread tried to exit it");
    CHECK(jabs_monitor_exit(h.lock) == 1, ENTER_EXIT, "the owner's exit was refused after a refused foreign exit");
}

static void blocked_worker(void *p) {
    handoff *h = (handoff *)p;
    jabs_monitor_enter(h->lock);
    h->other_result = __atomic_load_n(&h->owner_released, __ATOMIC_SEQ_CST);
    jabs_monitor_exit(h->lock);
}

static void monitor_blocks_until_owner_exits(void) {
    handoff h = { fresh_object(), 0, -1 };
    jabs_monitor_enter(h.lock);
    jabs_monitor_enter(h.lock);
    harness_thread *t = harness_spawn(blocked_worker, &h);
    harness_sleep_ms(100);
    CHECK(jabs_monitor_exit(h.lock) == 1, ENTER_EXIT, "inner exit refused");
    harness_sleep_ms(50); /* one level still held: the other thread must still be blocked */
    __atomic_store_n(&h.owner_released, 1, __ATOMIC_SEQ_CST);
    CHECK(jabs_monitor_exit(h.lock) == 1, ENTER_EXIT, "final exit refused");
    harness_join(t);
    CHECK(h.other_result == 1, ENTER_EXIT, "another thread entered while the owner still held the monitor (expected to block until the final exit)");
}

/* ---- wait / notify ---- */

#define WAIT_NOTIFY "jabs_monitor_wait/jabs_monitor_notify in runtime/jabs_sync.c"

/* Lock `o` and come back once `ready` reached `count`. Waiters bump `ready` while they own the
 * monitor and only release it by waiting, so owning it with ready == count means every one of
 * them is in the wait set. Returns owning the monitor. */
static void enter_when_waiting(jabs_object *o, int *ready, int count) {
    for (;;) {
        jabs_monitor_enter(o);
        if (__atomic_load_n(ready, __ATOMIC_SEQ_CST) == count) return;
        jabs_monitor_exit(o);
        harness_sleep_ms(1);
    }
}

typedef struct {
    jabs_object *lock;
    int ready;
    int woke;
    int results[8];
    int other_held; /* the second thread saw itself as owner while the first waited */
    jabs_sync_thread *waiter;
} waiting;

static void notifier_while_waiting(void *p) {
    waiting *w = (waiting *)p;
    enter_when_waiting(w->lock, &w->ready, 1);
    w->other_held = jabs_monitor_holds(w->lock);
    CHECK(jabs_monitor_notify(w->lock) == 1, WAIT_NOTIFY, "notify by the owner was refused");
    CHECK(jabs_monitor_exit(w->lock) == 1, ENTER_EXIT, "exit after notify was refused");
}

static void wait_releases_all_levels_and_restores_count(void) {
    waiting w = { fresh_object(), 0, 0, { 0 }, 0, NULL };
    for (int i = 0; i < 3; i++) jabs_monitor_enter(w.lock);
    harness_thread *t = harness_spawn(notifier_while_waiting, &w);
    __atomic_store_n(&w.ready, 1, __ATOMIC_SEQ_CST);
    int r = jabs_monitor_wait(w.lock, 0);
    harness_join(t);
    CHECK(w.other_held == 1, WAIT_NOTIFY, "another thread could not own the monitor while its owner waited with 3 levels entered (wait must release every level)");
    CHECK(r == JABS_WAIT_NOTIFIED, WAIT_NOTIFY, "wait woken by notify: expected JABS_WAIT_NOTIFIED (%d), got %d", JABS_WAIT_NOTIFIED, r);
    for (int i = 3; i > 0; i--) {
        CHECK(jabs_monitor_holds(w.lock), WAIT_NOTIFY, "after wait the monitor was released with %d level(s) left (count not restored)", i);
        CHECK(jabs_monitor_exit(w.lock) == 1, WAIT_NOTIFY, "exit %d after wait refused (count not restored)", 4 - i);
    }
    CHECK(jabs_monitor_exit(w.lock) == 0, WAIT_NOTIFY, "a fourth exit after wait succeeded: wait restored more levels than it released");
}

static void foreign_wait(void *p) {
    waiting *w = (waiting *)p;
    w->results[0] = jabs_monitor_wait(w->lock, 1);
    w->results[1] = jabs_monitor_notify(w->lock);
    w->results[2] = jabs_monitor_notify_all(w->lock);
}

static void wait_and_notify_need_ownership(void) {
    jabs_object *never = fresh_object();
    int r = jabs_monitor_wait(never, 1);
    CHECK(r == JABS_WAIT_NOT_OWNER, WAIT_NOTIFY, "wait on a never-locked object: expected JABS_WAIT_NOT_OWNER, got %d", r);
    CHECK(jabs_monitor_notify(never) == 0, WAIT_NOTIFY, "notify on a never-locked object: expected 0, got 1");
    CHECK(jabs_monitor_notify_all(never) == 0, WAIT_NOTIFY, "notifyAll on a never-locked object: expected 0, got 1");
    waiting w = { fresh_object(), 0, 0, { -1, -1, -1 }, 0, NULL };
    jabs_monitor_enter(w.lock);
    harness_join(harness_spawn(foreign_wait, &w));
    CHECK(w.results[0] == JABS_WAIT_NOT_OWNER, WAIT_NOTIFY, "wait by a thread that does not own the monitor: expected JABS_WAIT_NOT_OWNER, got %d", w.results[0]);
    CHECK(w.results[1] == 0 && w.results[2] == 0, WAIT_NOTIFY, "notify/notifyAll by a non-owner: expected 0/0, got %d/%d", w.results[1], w.results[2]);
    CHECK(jabs_monitor_holds(w.lock), WAIT_NOTIFY, "the owner lost the monitor when another thread tried to wait on it");
    jabs_monitor_exit(w.lock);
    CHECK(jabs_monitor_wait(w.lock, 1) == JABS_WAIT_NOT_OWNER, WAIT_NOTIFY, "wait after the final exit was allowed");
}

static void timed_wait_times_out_after_the_limit(void) {
    jabs_object *o = fresh_object();
    jabs_monitor_enter(o);
    jabs_monitor_enter(o);
    int64_t start = harness_now_ms();
    int r = jabs_monitor_wait(o, 50);
    int64_t took = harness_now_ms() - start;
    CHECK(r == JABS_WAIT_TIMED_OUT, WAIT_NOTIFY, "unnotified wait(50): expected JABS_WAIT_TIMED_OUT (%d), got %d", JABS_WAIT_TIMED_OUT, r);
    /* GetTickCount64 ticks in ~16 ms steps; one step of slack, never more. */
    CHECK(took >= 50 - 16, WAIT_NOTIFY, "wait(50) returned after %lld ms (too early: the deadline in jabs_monitor_wait)", (long long)took);
    CHECK(jabs_monitor_exit(o) == 1 && jabs_monitor_exit(o) == 1, WAIT_NOTIFY, "a timed-out wait did not restore both levels");
}

static void notify_before_wait_is_not_remembered(void) {
    jabs_object *o = fresh_object();
    jabs_monitor_enter(o);
    CHECK(jabs_monitor_notify(o) == 1 && jabs_monitor_notify_all(o) == 1, WAIT_NOTIFY, "notify with an empty wait set was refused");
    int r = jabs_monitor_wait(o, 30);
    CHECK(r == JABS_WAIT_TIMED_OUT, WAIT_NOTIFY, "a notify before the wait woke it: expected JABS_WAIT_TIMED_OUT, got %d", r);
    jabs_monitor_exit(o);
}

enum { NOTIFY_WAITERS = 4 };

static void counted_waiter(void *p) {
    waiting *w = (waiting *)p;
    jabs_monitor_enter(w->lock);
    int me = __atomic_fetch_add(&w->ready, 1, __ATOMIC_SEQ_CST);
    w->results[me] = jabs_monitor_wait(w->lock, 0);
    __atomic_add_fetch(&w->woke, 1, __ATOMIC_SEQ_CST);
    CHECK(jabs_monitor_holds(w->lock), WAIT_NOTIFY, "a waiter returned from wait without owning the monitor");
    jabs_monitor_exit(w->lock);
}

static void notify_wakes_exactly_one_waiter(void) {
    waiting w = { fresh_object(), 0, 0, { 0 }, 0, NULL };
    harness_thread *t[NOTIFY_WAITERS];
    for (int i = 0; i < NOTIFY_WAITERS; i++) t[i] = harness_spawn(counted_waiter, &w);
    enter_when_waiting(w.lock, &w.ready, NOTIFY_WAITERS);
    jabs_monitor_notify(w.lock);
    jabs_monitor_exit(w.lock);
    while (__atomic_load_n(&w.woke, __ATOMIC_SEQ_CST) < 1) harness_sleep_ms(1);
    harness_sleep_ms(200); /* time for a wrongly woken second waiter to show up */
    int woke = __atomic_load_n(&w.woke, __ATOMIC_SEQ_CST);
    CHECK(woke == 1, WAIT_NOTIFY, "one notify with %d waiters: expected 1 to wake, %d woke", NOTIFY_WAITERS, woke);
    jabs_monitor_enter(w.lock);
    jabs_monitor_notify_all(w.lock);
    jabs_monitor_exit(w.lock);
    for (int i = 0; i < NOTIFY_WAITERS; i++) harness_join(t[i]);
    for (int i = 0; i < NOTIFY_WAITERS; i++) {
        CHECK(w.results[i] == JABS_WAIT_NOTIFIED, WAIT_NOTIFY, "waiter %d: expected JABS_WAIT_NOTIFIED, got %d", i, w.results[i]);
    }
}

enum { ALL_WAITERS = 8 };

static void notify_all_wakes_every_waiter(void) {
    waiting w = { fresh_object(), 0, 0, { 0 }, 0, NULL };
    harness_thread *t[ALL_WAITERS];
    for (int i = 0; i < ALL_WAITERS; i++) t[i] = harness_spawn(counted_waiter, &w);
    enter_when_waiting(w.lock, &w.ready, ALL_WAITERS);
    CHECK(jabs_monitor_notify_all(w.lock) == 1, WAIT_NOTIFY, "notifyAll by the owner was refused");
    jabs_monitor_exit(w.lock);
    for (int i = 0; i < ALL_WAITERS; i++) harness_join(t[i]);
    for (int i = 0; i < ALL_WAITERS; i++) {
        CHECK(w.results[i] == JABS_WAIT_NOTIFIED, WAIT_NOTIFY, "waiter %d after notifyAll: expected JABS_WAIT_NOTIFIED, got %d", i, w.results[i]);
    }
}

/* Two threads take turns through one monitor with plain notify. A lost wakeup leaves both
 * waiting forever, which the watchdog turns into a failure. */
enum { PING_PONG_ROUNDS = 20000 };
typedef struct { jabs_object *lock; int turn; } ping_pong;
typedef struct { ping_pong *game; int me; } player;

static void play(void *p) {
    player *pl = (player *)p;
    ping_pong *g = pl->game;
    for (int i = 0; i < PING_PONG_ROUNDS; i++) {
        jabs_monitor_enter(g->lock);
        for (int spins = 0; g->turn != pl->me; spins++) {
            int r = jabs_monitor_wait(g->lock, 0);
            CHECK(r == JABS_WAIT_NOTIFIED, WAIT_NOTIFY, "round %d: untimed wait ended with %d instead of a notification", i, r);
            CHECK(spins < 2, WAIT_NOTIFY, "round %d: woken %d times without the turn changing (wakeup for the wrong thread)", i, spins + 1);
        }
        g->turn = 1 - pl->me;
        jabs_monitor_notify(g->lock);
        jabs_monitor_exit(g->lock);
    }
}

static void wait_notify_ping_pong_loses_no_wakeups(void) {
    ping_pong g = { fresh_object(), 0 };
    player a = { &g, 0 }, b = { &g, 1 };
    harness_thread *ta = harness_spawn(play, &a);
    harness_thread *tb = harness_spawn(play, &b);
    harness_join(ta);
    harness_join(tb);
}

/* ---- the interrupt hook ---- */

#define INTERRUPT "jabs_sync_interrupt and the wait loop in jabs_monitor_wait (runtime/jabs_sync.c)"

static void interruptible_waiter(void *p) {
    waiting *w = (waiting *)p;
    jabs_monitor_enter(w->lock);
    __atomic_store_n(&w->waiter, jabs_sync_current(), __ATOMIC_SEQ_CST);
    __atomic_add_fetch(&w->ready, 1, __ATOMIC_SEQ_CST);
    w->results[0] = jabs_monitor_wait(w->lock, 0);
    w->results[1] = jabs_monitor_holds(w->lock);
    w->results[2] = jabs_sync_clear_interrupt(jabs_sync_current());
    jabs_monitor_exit(w->lock);
}

static void interrupt_wakes_a_waiting_thread(void) {
    waiting w = { fresh_object(), 0, 0, { -1, -1, -1 }, 0, NULL };
    harness_thread *t = harness_spawn(interruptible_waiter, &w);
    enter_when_waiting(w.lock, &w.ready, 1);
    jabs_monitor_exit(w.lock);
    jabs_sync_interrupt(__atomic_load_n(&w.waiter, __ATOMIC_SEQ_CST));
    harness_join(t);
    CHECK(w.results[0] == JABS_WAIT_INTERRUPTED, INTERRUPT, "interrupted wait: expected JABS_WAIT_INTERRUPTED (%d), got %d", JABS_WAIT_INTERRUPTED, w.results[0]);
    CHECK(w.results[1] == 1, INTERRUPT, "an interrupted wait returned without owning the monitor");
    CHECK(w.results[2] == 0, INTERRUPT, "the interrupt was still pending after wait reported it (wait must consume it)");
}

static void interrupt_before_wait_returns_at_once(void) {
    jabs_object *o = fresh_object();
    jabs_monitor_enter(o);
    jabs_sync_interrupt(jabs_sync_current());
    int r = jabs_monitor_wait(o, 0);
    CHECK(r == JABS_WAIT_INTERRUPTED, INTERRUPT, "wait with an interrupt already pending: expected JABS_WAIT_INTERRUPTED, got %d", r);
    CHECK(jabs_monitor_holds(o), INTERRUPT, "wait refused for a pending interrupt but released the monitor");
    jabs_sync_interrupt(jabs_sync_current());
    CHECK(jabs_sync_clear_interrupt(jabs_sync_current()), INTERRUPT, "jabs_sync_clear_interrupt lost a pending interrupt");
    r = jabs_monitor_wait(o, 20);
    CHECK(r == JABS_WAIT_TIMED_OUT, INTERRUPT, "wait after the interrupt was cleared: expected JABS_WAIT_TIMED_OUT, got %d", r);
    jabs_monitor_exit(o);
    jabs_sync_interrupt(jabs_sync_current());
    r = jabs_monitor_wait(o, 0);
    CHECK(r == JABS_WAIT_NOT_OWNER, INTERRUPT, "wait without ownership and with an interrupt pending: expected JABS_WAIT_NOT_OWNER first (as the JVM), got %d", r);
}

/* ---- atomics, as generated code uses them ---- */

#define ATOMICS "jabs_atomic_add_float/double in runtime/jabs_rt.h, or the __atomic code var_handle_access and unsafe_intrinsic emit (src/cgen.rs)"

enum { ADD_THREADS = 8, ADD_ROUNDS = 20000 };
typedef struct { double d; float f; int arrived; } adds;

static void adder(void *p) {
    adds *a = (adds *)p;
    arrive_and_wait(&a->arrived, ADD_THREADS);
    for (int i = 0; i < ADD_ROUNDS; i++) {
        jabs_atomic_add_double(&a->d, 1.0, __ATOMIC_SEQ_CST);
        jabs_atomic_add_float(&a->f, 1.0f, __ATOMIC_ACQ_REL);
    }
}

/* Sums stay exact far below 2^24 (float) and 2^53 (double), so a lost update shows. */
static void atomic_float_add_loses_no_updates(void) {
    adds a = { 0.0, 0.0f, 0 };
    harness_thread *t[ADD_THREADS];
    for (int i = 0; i < ADD_THREADS; i++) t[i] = harness_spawn(adder, &a);
    for (int i = 0; i < ADD_THREADS; i++) harness_join(t[i]);
    double want = (double)ADD_THREADS * ADD_ROUNDS;
    CHECK(a.d == want, ATOMICS, "double getAndAdd under contention: expected %.0f, got %.0f", want, a.d);
    CHECK(a.f == (float)want, ATOMICS, "float getAndAdd under contention: expected %.0f, got %.0f", want, (double)a.f);
}

enum { CAS_THREADS = 8, CAS_ROUNDS = 50000 };
typedef struct { int64_t counter; int arrived; } cas_race;

/* The compare-and-set Unsafe.compareAndSetLong compiles to, in a retry loop like
 * AtomicLong.getAndIncrement's. */
static void cas_worker(void *p) {
    cas_race *c = (cas_race *)p;
    arrive_and_wait(&c->arrived, CAS_THREADS);
    for (int i = 0; i < CAS_ROUNDS; i++) {
        int64_t e;
        __atomic_load(&c->counter, &e, __ATOMIC_SEQ_CST);
        for (;;) {
            int64_t n = e + 1;
            if (__atomic_compare_exchange(&c->counter, &e, &n, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) break;
        }
    }
}

static void atomic_compare_and_set_loses_no_updates(void) {
    cas_race c = { 0, 0 };
    harness_thread *t[CAS_THREADS];
    for (int i = 0; i < CAS_THREADS; i++) t[i] = harness_spawn(cas_worker, &c);
    for (int i = 0; i < CAS_THREADS; i++) harness_join(t[i]);
    int64_t want = (int64_t)CAS_THREADS * CAS_ROUNDS;
    CHECK(c.counter == want, ATOMICS, "compare-and-set increments: expected %lld, got %lld", (long long)want, (long long)c.counter);
}

/* A volatile long/double must be single-copy atomic: a reader never sees half of one write. */
enum { TEAR_ROUNDS = 2000000 };
typedef struct { int64_t value; int done; } tearing;

static void tear_writer(void *p) {
    tearing *t = (tearing *)p;
    for (int i = 0; i < TEAR_ROUNDS; i++) {
        int64_t v = (i & 1) ? -1 : 0;
        __atomic_store(&t->value, &v, __ATOMIC_SEQ_CST);
    }
    __atomic_store_n(&t->done, 1, __ATOMIC_SEQ_CST);
}

static void volatile_long_is_never_torn(void) {
    static tearing t;
    harness_thread *w = harness_spawn(tear_writer, &t);
    while (!__atomic_load_n(&t.done, __ATOMIC_SEQ_CST)) {
        int64_t v;
        __atomic_load(&t.value, &v, __ATOMIC_SEQ_CST);
        CHECK(v == 0 || v == -1, ATOMICS,
              "read a torn 64-bit value %016llx (expected 0 or -1)", (unsigned long long)v);
    }
    harness_join(w);
}

typedef struct { const char *name; void (*run)(void); } harness_test;
static const harness_test tests[] = {
    { "monitor_excludes_concurrent_owners", monitor_excludes_concurrent_owners },
    { "monitor_first_use_race_installs_one_monitor", monitor_first_use_race_installs_one_monitor },
    { "monitor_reentry_counts_levels", monitor_reentry_counts_levels },
    { "monitor_unlocked_object_is_not_owned", monitor_unlocked_object_is_not_owned },
    { "monitor_exit_by_non_owner_is_refused", monitor_exit_by_non_owner_is_refused },
    { "monitor_blocks_until_owner_exits", monitor_blocks_until_owner_exits },
    { "wait_releases_all_levels_and_restores_count", wait_releases_all_levels_and_restores_count },
    { "wait_and_notify_need_ownership", wait_and_notify_need_ownership },
    { "timed_wait_times_out_after_the_limit", timed_wait_times_out_after_the_limit },
    { "notify_before_wait_is_not_remembered", notify_before_wait_is_not_remembered },
    { "notify_wakes_exactly_one_waiter", notify_wakes_exactly_one_waiter },
    { "notify_all_wakes_every_waiter", notify_all_wakes_every_waiter },
    { "wait_notify_ping_pong_loses_no_wakeups", wait_notify_ping_pong_loses_no_wakeups },
    { "interrupt_wakes_a_waiting_thread", interrupt_wakes_a_waiting_thread },
    { "interrupt_before_wait_returns_at_once", interrupt_before_wait_returns_at_once },
    { "atomic_float_add_loses_no_updates", atomic_float_add_loses_no_updates },
    { "atomic_compare_and_set_loses_no_updates", atomic_compare_and_set_loses_no_updates },
    { "volatile_long_is_never_torn", volatile_long_is_never_torn },
};

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: harness <test>\n"); return 2; }
    for (size_t i = 0; i < sizeof tests / sizeof *tests; i++) {
        if (strcmp(tests[i].name, argv[1]) != 0) continue;
        current_test = tests[i].name;
        harness_start_watchdog(current_test, 20);
        tests[i].run();
        printf("ok %s\n", current_test);
        return 0;
    }
    fprintf(stderr, "harness: no test named %s\n", argv[1]);
    return 2;
}
