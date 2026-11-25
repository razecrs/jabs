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

typedef struct { const char *name; void (*run)(void); } harness_test;
static const harness_test tests[] = {
    { "monitor_excludes_concurrent_owners", monitor_excludes_concurrent_owners },
    { "monitor_first_use_race_installs_one_monitor", monitor_first_use_race_installs_one_monitor },
    { "monitor_reentry_counts_levels", monitor_reentry_counts_levels },
    { "monitor_unlocked_object_is_not_owned", monitor_unlocked_object_is_not_owned },
    { "monitor_exit_by_non_owner_is_refused", monitor_exit_by_non_owner_is_refused },
    { "monitor_blocks_until_owner_exits", monitor_blocks_until_owner_exits },
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
