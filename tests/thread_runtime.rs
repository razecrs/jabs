//! C-level tests of the thread runtime (runtime/jabs_thread.c and the thread-safe parts of
//! runtime/jabs_rt.h), without Java: a small C harness linked against the unit JABS generates for
//! no classes at all, so every runtime file is present and nothing else is. Each test runs one
//! case of the harness in its own process under a watchdog, so a hang fails within seconds and
//! names the mechanism.
//!
//! Needs `clang` on PATH; missing, the tests skip locally and fail with JABS_REQUIRE_TOOLS=1.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

/// A case that has not finished by then is hung (each one needs well under a second).
const CASE_LIMIT: Duration = Duration::from_secs(30);

/// The harness: `harness <case>` prints "ok" and exits 0, or names what broke and exits 1.
const HARNESS: &str = r#"#include "jabs_unit.h"
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#include <time.h>
#endif

static _Noreturn void fail(const char *what) {
    fprintf(stderr, "FAIL: %s\n", what);
    fflush(stderr);
    exit(1);
}

/* Test threads, made directly with the OS so the runtime under test is not what starts them. */
typedef struct { void (*body)(void *); void *arg; } test_start;
#ifdef _WIN32
typedef HANDLE test_thread;
static DWORD WINAPI test_main(LPVOID p) { test_start *s = (test_start *)p; s->body(s->arg); return 0; }
static test_thread test_spawn(test_start *s) {
    HANDLE h = CreateThread(NULL, 0, test_main, s, 0, NULL);
    if (!h) fail("harness: CreateThread failed");
    return h;
}
static void test_join(test_thread t) {
    if (WaitForSingleObject(t, INFINITE) != WAIT_OBJECT_0) fail("harness: WaitForSingleObject failed");
    if (!CloseHandle(t)) fail("harness: CloseHandle failed");
}
static void test_pause_ms(int ms) { Sleep((DWORD)ms); }
#else
typedef pthread_t test_thread;
static void *test_main(void *p) { test_start *s = (test_start *)p; s->body(s->arg); return NULL; }
static test_thread test_spawn(test_start *s) {
    pthread_t t;
    if (pthread_create(&t, NULL, test_main, s) != 0) fail("harness: pthread_create failed");
    return t;
}
static void test_join(test_thread t) { if (pthread_join(t, NULL) != 0) fail("harness: pthread_join failed"); }
static void test_pause_ms(int ms) {
    struct timespec d = { ms / 1000, (long)(ms % 1000) * 1000000L };
    if (nanosleep(&d, NULL) != 0) fail("harness: nanosleep failed");
}
#endif

/* ---- class initialization (jabs_class_init_begin / jabs_class_init_end) ---- */

typedef struct { jabs_class_init *c; _Atomic int result; _Atomic int finished; const char *pending; } init_request;

static void request_init(void *p) {
    init_request *r = (init_request *)p;
    jabs_pending = NULL;
    int result = jabs_class_init_begin(r->c);
    r->pending = jabs_pending ? jabs_pending->cls->name : NULL;
    jabs_pending = NULL;
    atomic_store(&r->result, result);
    atomic_store(&r->finished, 1);
}

static void init_waits(void) {
    static jabs_class_init c;
    if (jabs_class_init_begin(&c) != JABS_INIT_RUN) fail("first request did not get JABS_INIT_RUN");
    init_request r = { &c, -1, 0, NULL };
    test_start s = { request_init, &r };
    test_thread t = test_spawn(&s);
    test_pause_ms(300);
    if (atomic_load(&r.finished)) fail("second thread did not wait for the in-progress initialization: the wait loop in jabs_class_init_begin (runtime/jabs_thread.c)");
    jabs_class_init_end(&c, true);
    test_join(t);
    if (atomic_load(&r.result) != JABS_INIT_DONE) fail("waiting thread did not get JABS_INIT_DONE after the initialization ended: jabs_class_init_end's broadcast or begin's recheck");
    if (!jabs_class_initialized(&c)) fail("class not marked initialized: jabs_class_init_end");
}

static void init_recursive(void) {
    static jabs_class_init c;
    if (jabs_class_init_begin(&c) != JABS_INIT_RUN) fail("first request did not get JABS_INIT_RUN");
    if (jabs_class_init_begin(&c) != JABS_INIT_DONE) fail("recursive request by the initializing thread did not return JABS_INIT_DONE at once (JVMS 5.5 step 3): owner check in jabs_class_init_begin");
    if (jabs_class_initialized(&c)) fail("class looked initialized before jabs_class_init_end");
    jabs_class_init_end(&c, true);
    if (!jabs_class_initialized(&c)) fail("class not marked initialized: jabs_class_init_end");
}

static void init_failure(void) {
    static jabs_class_init c;
    if (jabs_class_init_begin(&c) != JABS_INIT_RUN) fail("first request did not get JABS_INIT_RUN");
    init_request r = { &c, -1, 0, NULL };
    test_start s = { request_init, &r };
    test_thread t = test_spawn(&s);
    test_pause_ms(100);
    jabs_class_init_end(&c, false);
    test_join(t);
    if (atomic_load(&r.result) != JABS_INIT_FAILED) fail("thread waiting on a failed initialization did not get JABS_INIT_FAILED (JVMS 5.5 step 5): ERRONEOUS case in jabs_class_init_begin");
    if (!r.pending || strcmp(r.pending, "java/lang/NoClassDefFoundError") != 0) fail("failed initialization did not leave NoClassDefFoundError pending in the waiting thread");
    jabs_pending = NULL;
    if (jabs_class_init_begin(&c) != JABS_INIT_FAILED) fail("later request for an erroneous class did not get JABS_INIT_FAILED");
    if (!jabs_pending || strcmp(jabs_pending->cls->name, "java/lang/NoClassDefFoundError") != 0) fail("later request for an erroneous class did not raise NoClassDefFoundError");
    jabs_pending = NULL;
}

#define RACERS 16
#define ROUNDS 50
typedef struct { jabs_class_init *c; _Atomic int *go; _Atomic int *runs; _Atomic int *saw_unfinished; } racer;

static void race_init(void *p) {
    racer *r = (racer *)p;
    for (int spin = 0; spin < 100000000 && !atomic_load(r->go); spin++) {}
    int result = jabs_class_init_begin(r->c);
    if (result == JABS_INIT_RUN) {
        atomic_fetch_add(r->runs, 1);
        test_pause_ms(5);
        jabs_class_init_end(r->c, true);
    } else if (result != JABS_INIT_DONE || !jabs_class_initialized(r->c)) {
        atomic_fetch_add(r->saw_unfinished, 1);
    }
}

static void init_once(void) {
    for (int round = 0; round < ROUNDS; round++) {
        jabs_class_init c = { 0, NULL };
        _Atomic int go = 0, runs = 0, saw_unfinished = 0;
        racer racers[RACERS];
        test_start starts[RACERS];
        test_thread threads[RACERS];
        for (int i = 0; i < RACERS; i++) {
            racers[i] = (racer){ &c, &go, &runs, &saw_unfinished };
            starts[i] = (test_start){ race_init, &racers[i] };
            threads[i] = test_spawn(&starts[i]);
        }
        atomic_store(&go, 1);
        for (int i = 0; i < RACERS; i++) test_join(threads[i]);
        if (atomic_load(&runs) != 1) { fprintf(stderr, "round %d: %d threads ran the initialization\n", round, atomic_load(&runs)); fail("initialization did not run exactly once among racing threads: UNINITIALIZED case in jabs_class_init_begin"); }
        if (atomic_load(&saw_unfinished) != 0) fail("a racing thread returned from jabs_class_init_begin before the class was initialized");
    }
}

/* ---- runtime locks and the identity hash ---- */

#define LOCKERS 8
#define INCREMENTS 100000
static long locked_counter;
static void add_under_lock(void *p) {
    (void)p;
    for (int i = 0; i < INCREMENTS; i++) {
        jabs_lock(JABS_LOCK_STRINGS);
        locked_counter++;
        jabs_unlock(JABS_LOCK_STRINGS);
    }
}

static void lock_excludes(void) {
    test_start s = { add_under_lock, NULL };
    test_thread threads[LOCKERS];
    for (int i = 0; i < LOCKERS; i++) threads[i] = test_spawn(&s);
    for (int i = 0; i < LOCKERS; i++) test_join(threads[i]);
    if (locked_counter != (long)LOCKERS * INCREMENTS) fail("increments under jabs_lock were lost: jabs_lock/jabs_unlock in runtime/jabs_thread.c do not exclude");
}

#define HASHERS 8
#define OBJECTS 2000
static jabs_object objects[OBJECTS];
static int32_t hashes[HASHERS][OBJECTS];
static _Atomic int hash_go;
static void hash_all(void *p) {
    int32_t *out = (int32_t *)p;
    for (int spin = 0; spin < 100000000 && !atomic_load(&hash_go); spin++) {}
    for (int i = 0; i < OBJECTS; i++) out[i] = jabs_identity_hash(&objects[i]);
}

static void hash_agrees(void) {
    test_start starts[HASHERS];
    test_thread threads[HASHERS];
    for (int i = 0; i < HASHERS; i++) { starts[i] = (test_start){ hash_all, hashes[i] }; threads[i] = test_spawn(&starts[i]); }
    atomic_store(&hash_go, 1);
    for (int i = 0; i < HASHERS; i++) test_join(threads[i]);
    for (int i = 0; i < OBJECTS; i++) {
        if (hashes[0][i] == 0) fail("identity hash 0 (it must never be 0)");
        for (int k = 1; k < HASHERS; k++) {
            if (hashes[k][i] != hashes[0][i]) fail("threads hashing one object at once got different identity hashes: the compare-and-swap in jabs_identity_hash (runtime/jabs_rt.h)");
        }
    }
}

int main(int argc, char **argv) {
    if (argc != 2) fail("usage: harness <case>");
    static const struct { const char *name; void (*run)(void); } cases[] = {
        { "init_waits", init_waits },
        { "init_recursive", init_recursive },
        { "init_failure", init_failure },
        { "init_once", init_once },
        { "lock_excludes", lock_excludes },
        { "hash_agrees", hash_agrees },
    };
    for (size_t i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        if (strcmp(argv[1], cases[i].name) == 0) { cases[i].run(); printf("ok\n"); return 0; }
    }
    fail("no such case");
}
"#;

fn tool_available(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Clang target arguments that can find a C library (as in tests/differential.rs).
fn clang_target_args(dir: &Path) -> Vec<String> {
    if let Ok(args) = std::env::var("JABS_CLANG_ARGS") {
        return args.split_whitespace().map(String::from).collect();
    }
    let probe = dir.join("probe.c");
    std::fs::write(&probe, "#include <stdio.h>\nint main(void) { return 0; }\n").unwrap();
    let mut candidates = vec![Vec::new()];
    if cfg!(windows) {
        candidates.push(vec!["--target=x86_64-w64-windows-gnu".to_string()]);
    }
    for args in candidates {
        let ok = Command::new("clang")
            .args(&args)
            .arg(&probe)
            .arg("-o")
            .arg(dir.join("probe.exe"))
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return args;
        }
    }
    panic!("clang cannot build a C program for any known target; set JABS_CLANG_ARGS");
}

/// The built harness, or None when clang is missing and not required. Built once for all tests.
fn harness() -> Option<&'static PathBuf> {
    static BUILT: OnceLock<Option<(tempfile::TempDir, PathBuf)>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            if !tool_available("clang", "--version") {
                assert!(
                    std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
                    "required tool missing: clang"
                );
                eprintln!("skipping thread runtime tests; clang is missing");
                return None;
            }
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let unit = jabs::cgen::compile_unit(&[]);
            let mut sources = Vec::new();
            for (name, contents) in &unit.c_files {
                std::fs::write(root.join(name), contents).unwrap();
                if name.ends_with(".c") {
                    sources.push(root.join(name));
                }
            }
            let harness = root.join("harness.c");
            std::fs::write(&harness, HARNESS).unwrap();
            sources.push(harness);
            let exe = root.join(if cfg!(windows) {
                "harness.exe"
            } else {
                "harness"
            });
            let output = Command::new("clang")
                .args(clang_target_args(root))
                .args([
                    "-std=c11",
                    "-O2",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "-Wno-unused-function",
                    "-Wno-unused-label",
                    "-D_CRT_SECURE_NO_WARNINGS",
                ])
                .args(&sources)
                .arg("-o")
                .arg(&exe)
                .args(if cfg!(windows) {
                    &["-lws2_32", "-liphlpapi"][..]
                } else {
                    &["-pthread", "-lm"][..]
                })
                .output()
                .expect("spawn clang");
            assert!(
                output.status.success(),
                "building the thread runtime harness failed (generated unit for no classes + runtime/*.c):\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some((dir, exe))
        })
        .as_ref()
        .map(|(_, exe)| exe)
}

/// Run one harness case under the watchdog; `look` says where to look when it fails.
fn case(name: &str, look: &str) {
    let Some(exe) = harness() else {
        return;
    };
    let mut child = Command::new(exe)
        .arg(name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harness");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + CASE_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for harness") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    let Some(status) = status else {
        panic!(
            "{name}: hung, killed after {CASE_LIMIT:?}; look at {look}\nstdout:\n{out}\nstderr:\n{err}"
        );
    };
    assert!(
        status.success() && out.trim() == "ok",
        "{name}: failed ({status}); look at {look}\nstdout:\n{out}\nstderr:\n{err}"
    );
}

#[test]
fn class_init_second_thread_waits_for_initializer() {
    case(
        "init_waits",
        "jabs_class_init_begin's wait loop and jabs_class_init_end's broadcast in runtime/jabs_thread.c",
    );
}

#[test]
fn class_init_recursive_request_returns_at_once() {
    case(
        "init_recursive",
        "the owner check (JVMS 5.5 step 3) in jabs_class_init_begin, runtime/jabs_thread.c",
    );
}

#[test]
fn class_init_failure_reaches_waiters_as_no_class_def_found() {
    case(
        "init_failure",
        "the ERRONEOUS case of jabs_class_init_begin and jabs_class_init_end(false), runtime/jabs_thread.c",
    );
}

#[test]
fn class_init_runs_once_among_racing_threads() {
    case(
        "init_once",
        "the UNINITIALIZED case of jabs_class_init_begin (only one thread may get JABS_INIT_RUN), runtime/jabs_thread.c",
    );
}

#[test]
fn runtime_lock_excludes_other_threads() {
    case(
        "lock_excludes",
        "jabs_lock/jabs_unlock and the OS mutex wrappers in runtime/jabs_thread.c",
    );
}

#[test]
fn identity_hash_is_the_same_for_racing_threads() {
    case(
        "hash_agrees",
        "jabs_identity_hash in runtime/jabs_rt.h (compare-and-swap on jabs_object.hash)",
    );
}
