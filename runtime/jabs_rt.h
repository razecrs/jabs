/* JABS runtime prelude: Java primitive semantics in portable C11.
 *
 * Requirements: two's-complement integers, IEEE-754 binary32/binary64 with
 * FLT_EVAL_METHOD == 0 (SSE2 on x86-64, any AArch64), and no -ffast-math.
 * Signed results are produced through unsigned arithmetic so overflow wraps
 * as Java requires instead of being undefined behaviour in C.
 */
#ifndef JABS_RT_H
#define JABS_RT_H

/* Feature-test macros for the runtime when compiled as a single C file (c_source).
 * Must precede the first standard-library header include below. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#ifndef _XOPEN_SOURCE
#define _XOPEN_SOURCE 700
#endif
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#else
#ifndef _CRT_RAND_S
#define _CRT_RAND_S
#endif
#endif

#include <float.h>
#include <math.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if FLT_EVAL_METHOD != 0
#error "JABS requires FLT_EVAL_METHOD == 0 for Java floating-point semantics"
#endif

/* One interface-table entry: a method signature's selector and the implementation chosen for it. */
typedef struct jabs_itable_entry {
    uint64_t selector;
    void (*fn)(void);
} jabs_itable_entry;

/* A class's runtime record: name, instance size, vtable (one slot per overridable method),
 * superclass record (NULL directly below java/lang/Object), interface table sorted by
 * selector, every superinterface, and whether the record is itself an interface. */
typedef struct jabs_class {
    const char *name;
    size_t size;
    void (*const *vtable)(void);
    const struct jabs_class *super;
    const jabs_itable_entry *itable;
    uint32_t itable_len;
    const struct jabs_class *const *interfaces;
    uint32_t interfaces_len;
    int32_t is_interface;
    uint16_t access; /* class-file access flags (Class.getModifiers approximates from these) */
} jabs_class;

/* Every object starts with this header; the class's own struct embeds it first. */
typedef struct jabs_object {
    const jabs_class *cls;
    /* Lock word, only touched atomically by runtime/jabs_sync.c: 0 unlocked, a thin lock
     * (owner thread id and depth), or an inflated monitor's index. 0 in a fresh object. */
    uint32_t monitor;
    /* Identity hash code, assigned on first request (0 = not yet assigned). Atomic: two threads
     * asking at once must agree on one value. */
    _Atomic uint32_t hash;
} jabs_object;

typedef union jabs_value {
    int32_t i;
    int64_t j;
    float f;
    double d;
    jabs_object *a;
} jabs_value;

/* The exception being thrown (a Throwable object), or NULL. */
extern _Thread_local jabs_object *jabs_pending;
/* The method whose code last raised a VM exception on this thread (a breadcrumb until stack
 * traces exist). */
extern _Thread_local const char *jabs_where;

/* Exceptions the runtime itself raises, by kind. The generated unit defines jabs_vm_throwable,
 * which allocates an instance of the JDK class when it is in the unit. Constructors are not run
 * (Throwable's captures a stack trace natively), so these carry no message or stack trace yet. */
enum {
    JABS_VM_NULL_POINTER,
    JABS_VM_ARITHMETIC,
    JABS_VM_ARRAY_INDEX,
    JABS_VM_NEGATIVE_ARRAY_SIZE,
    JABS_VM_ARRAY_STORE,
    JABS_VM_CLASS_CAST,
    JABS_VM_INCOMPATIBLE_CLASS_CHANGE,
    JABS_VM_ABSTRACT_METHOD,
    JABS_VM_NO_CLASS_DEF_FOUND,
    JABS_VM_INITIALIZER,
    JABS_VM_OUT_OF_MEMORY,
    JABS_VM_ILLEGAL_MONITOR_STATE,
    JABS_VM_CLONE_NOT_SUPPORTED,
    JABS_VM_IO,
    JABS_VM_CLASS_NOT_FOUND,
    JABS_VM_INSTANTIATION,
    JABS_VM_FILE_NOT_FOUND,
    JABS_VM_SYNC_FAILED,
    JABS_VM_INTERRUPTED
};
jabs_object *jabs_vm_throwable(int kind);
/* Whether a thrown object is a java.lang.Error (which class initialization passes through unwrapped). */
int jabs_is_error(const jabs_object *o);
/* Throwable.cause of a thrown object (NULL when unset), and setting it for VM-raised wrappers. */
void jabs_set_cause(jabs_object *o, jabs_object *cause);
jabs_object *jabs_cause_of(jabs_object *o);

/* Allocate a zeroed object: all-zero bytes are every field's Java default (0, 0.0, false, null).
 * There is no garbage collector yet, so objects live until the process exits. */
static inline jabs_object *jabs_new(const jabs_class *cls);
/* The canonical String with the same contents (String.intern; literals use it too). */
jabs_object *jabs_string_intern(jabs_object *s);
/* Bytes of Java objects allocated so far, by every thread. With no garbage collector the heap
 * only grows, so this is Runtime.totalMemory (and freeMemory is 0). */
extern _Atomic int64_t jabs_heap_bytes;

/* Runtime locks (jabs_thread.c) for the tables every thread shares: the Class registry, the
 * String intern table, the sorting of the frame table and the .jab's descriptors. A holder never runs Java code, never
 * raises an exception and never takes another runtime lock, so these cannot deadlock. */
enum { JABS_LOCK_CLASSES, JABS_LOCK_STRINGS, JABS_LOCK_FRAMES, JABS_LOCK_JAB, JABS_LOCK_COUNT };
void jabs_lock(int which);
void jabs_unlock(int which);
/* Class (internal name) that called the @CallerSensitive method now running; see
 * Reflection.getCallerClass. Set at each call site of such a method. */
extern _Thread_local const char *jabs_caller;

/* A class's initialization state (JVMS 5.5), one per class, zero-initialized. `state` is read
 * without a lock only on the fast path (an acquire load that sees JABS_CLASS_INITIALIZED, paired
 * with the release store that ends initialization); every change to it, and `owner` (the thread
 * running the initialization), happens in jabs_thread.c under the class-initialization lock. */
enum { JABS_CLASS_UNINITIALIZED, JABS_CLASS_INITIALIZING, JABS_CLASS_INITIALIZED, JABS_CLASS_ERRONEOUS };
typedef struct { _Atomic int32_t state; const void *owner; } jabs_class_init;
static inline bool jabs_class_initialized(jabs_class_init *c) {
    return atomic_load_explicit(&c->state, memory_order_acquire) == JABS_CLASS_INITIALIZED;
}
/* What the caller of jabs_class_init_begin does next: run the initialization (and then call
 * jabs_class_init_end), return success (initialized, or this thread is already initializing it),
 * or fail (erroneous: NoClassDefFoundError is pending). Another thread's initialization in
 * progress is waited for inside jabs_class_init_begin. */
enum { JABS_INIT_RUN, JABS_INIT_DONE, JABS_INIT_FAILED };
int jabs_class_init_begin(jabs_class_init *c);
void jabs_class_init_end(jabs_class_init *c, bool ok);
/* The same gate for a once-only runtime step that runs Java code but is not a class (the module
 * graph, runtime/jabs_modules.c): begin with jabs_class_init_begin, end with this, which does not
 * count it as an initialized class. */
void jabs_runtime_once_end(jabs_class_init *c, bool ok);

/* Threads (jabs_thread.c). The java.lang.Thread of the calling thread: threads JABS started know
 * theirs from the start; the main thread's is built on first request (jabs_main_thread, generated),
 * which then calls jabs_thread_attach_main. */
jabs_object *jabs_current_thread(void);
void jabs_thread_attach_main(jabs_object *thread);
/* Thread.start0: mark the thread alive and RUNNABLE, then run it on a new OS thread. */
void jabs_thread_start(jabs_object *thread);
/* What the java launcher does once main has returned (its uncaught exception, if any, already
 * reported and cleared by the caller): end the main thread like any other (Thread.exit, then
 * TERMINATED and join() waiters woken), then wait until no non-daemon thread runs
 * (DestroyJavaVM). Daemon threads still running die with the process. */
void jabs_destroy_java_vm(void);
/* Thread.sleepNanos0: sleep, ending early with InterruptedException("sleep interrupted") when
 * the thread is or becomes interrupted (the interrupt clears). 0 yields. */
void jabs_thread_sleep(int64_t nanos);
/* Unsafe.park / Unsafe.unpark: per-thread binary permit using sleep_lock/sleep_wake; unlike
 * sleep, an interrupt wakes park without clearing the interrupt status or throwing. */
void jabs_thread_park(int32_t is_absolute, int64_t time);
void jabs_thread_unpark(jabs_object *thread);
/* Thread.interrupt0: wake the thread if it sleeps (Thread.interrupt already set the flag). */
void jabs_thread_interrupt(jabs_object *thread);
/* Thread.clearInterruptEvent: the caller cleared the Java interrupt status; drop the monitors'
 * pending interrupt mark too, so a later Object.wait does not see a stale interrupt. */
void jabs_thread_clear_interrupt_event(void);
/* Object.wait reported JABS_WAIT_INTERRUPTED: clear the interrupt status and raise
 * InterruptedException, as HotSpot's ObjectMonitor::wait does. */
void jabs_thread_wait_interrupted(void);
void jabs_thread_yield(void);
/* Name the calling OS thread (debuggers, ps); best effort, as in HotSpot. UTF-8. */
void jabs_thread_set_native_name(const char *name);
/* Processors this process may run on (Runtime.availableProcessors). */
int32_t jabs_thread_processor_count(void);
/* java.lang.management's thread counts (VMManagementImpl, ThreadMXBean), kept as HotSpot's
 * ThreadService does: live and daemon threads now, the most live at once since the last reset,
 * and every thread ever started, main included. One consistent snapshot. */
typedef struct { int64_t live, daemon, peak, started; } jabs_thread_counts;
jabs_thread_counts jabs_thread_management_counts(void);
/* ThreadMXBean.resetPeakThreadCount: the peak becomes the live count. */
void jabs_thread_reset_peak(void);
/* Classes whose initialization has completed (jabs_class_init_end with ok). */
int64_t jabs_class_initialized_count(void);
/* Classes and interfaces compiled into the program (generated with the class records). */
extern const int64_t jabs_class_record_count;
/* The VM's start (jabs_vm_startup records it; else the first query does) as wall-clock
 * milliseconds, and milliseconds since then on the monotonic clock (jabs_platform.c). */
void jabs_platform_record_start(void);
int64_t jabs_platform_start_millis(void);
int64_t jabs_platform_uptime_millis(void);
/* The operating system's id of this process. */
int64_t jabs_platform_process_id(void);
/* TimeZone's two natives (jabs_platform.c): the host's zone as a Java zone ID, and its current
 * offset as "GMT+hh:mm"; malloc'd UTF-8, or NULL when the host has no answer. */
char *jabs_platform_time_zone_id(const char *java_home);
char *jabs_platform_gmt_offset_id(void);

/* java.util.zip.CRC32: CRC-32 (IEEE 802.3, reflected polynomial 0xEDB88320) continuing from a
 * previous checksum, exactly as zlib's crc32(), which the JDK's natives call. Bit by bit, with no
 * table to build or share between threads; fast enough for the zone data and class-path checks
 * that use it so far. */
static inline uint32_t jabs_crc32(uint32_t crc, const uint8_t *data, size_t length) {
    crc = ~crc;
    for (size_t i = 0; i < length; i++) {
        crc ^= data[i];
        for (int bit = 0; bit < 8; bit++) crc = (crc >> 1) ^ (0xEDB88320u & (0u - (crc & 1u)));
    }
    return ~crc;
}
/* java.util.zip.Adler32: Adler-32 checksum continuing from a previous value, matching zlib's
 * adler32(). Returns `adler` unchanged when `length == 0` without dereferencing `data`. */
static inline uint32_t jabs_adler32(uint32_t adler, const uint8_t *data, size_t length) {
    const uint32_t MOD_ADLER = 65521u;
    const size_t NMAX = 5552;
    uint32_t s1 = adler & 0xffffu;
    uint32_t s2 = (adler >> 16) & 0xffffu;
    while (length > 0) {
        size_t chunk = length < NMAX ? length : NMAX;
        length -= chunk;
        for (size_t i = 0; i < chunk; i++) {
            s1 += data[i];
            s2 += s1;
        }
        data += chunk;
        s1 %= MOD_ADLER;
        s2 %= MOD_ADLER;
    }
    return (s2 << 16) | s1;
}
/* The host's 1, 5 and 15 minute load averages, the first `count` (1..3) into `out`: how many
 * were stored, or -1 when the host has none (Windows). */
int32_t jabs_platform_load_average(double *out, int32_t count);
/* Generated glue the thread runtime uses (thread_glue_functions in cgen.rs): the main Thread, and
 * where Thread.eetop lives (the JVM's "alive" marker: non-zero exactly while the thread runs). */
jabs_object *jabs_main_thread(void);
int64_t *jabs_thread_eetop_slot(jabs_object *thread);
/* holder.threadStatus (NULL when the Thread has no holder) and holder.daemon. */
int32_t *jabs_thread_status_slot(jabs_object *thread);
bool jabs_thread_is_daemon(jabs_object *thread);
/* Thread.interrupted, the interrupt status Thread.interrupt sets. */
uint8_t *jabs_thread_interrupted_slot(jabs_object *thread);
/* thread.run() (virtual), Thread.dispatchUncaughtException and Thread.exit. The last two return
 * false when the method was not compiled into the program. */
void jabs_thread_run_java(jabs_object *thread);
bool jabs_thread_dispatch_uncaught(jabs_object *thread, jabs_object *uncaught);
bool jabs_thread_exit_java(jabs_object *thread);
/* Raise a VM exception with a message when the unit can build Strings, else without one. */
void jabs_thread_raise(int kind, const char *message);
/* The monitor a static synchronized method of class `name` locks: its Class object, as
 * synchronized (X.class) does; `fallback` when the program has no Class registry (then nothing
 * else can name the class's monitor). NULL with an exception pending if creating it failed. */
jabs_object *jabs_static_monitor(const char *name, jabs_object *fallback);

/* Exception a failed static initializer reports (JVMS 5.5 step 11): Errors pass through,
 * anything else becomes ExceptionInInitializerError. */
static inline jabs_object *jabs_initializer_failure(jabs_object *thrown) {
    if (jabs_is_error(thrown)) return thrown;
    jabs_object *wrapper = jabs_vm_throwable(JABS_VM_INITIALIZER);
    jabs_set_cause(wrapper, thrown);
    return wrapper;
}

/* Reinterpret raw IEEE bits as float/double (exact, including NaN payloads). */
static inline float jabs_fbits(uint32_t bits) { float f; memcpy(&f, &bits, 4); return f; }
static inline double jabs_dbits(uint64_t bits) { double d; memcpy(&d, &bits, 8); return d; }
/* And back: the raw bits of a float/double (floatToRawIntBits: NaN payloads kept). */
static inline uint32_t jabs_float_raw_bits(float f) { uint32_t bits; memcpy(&bits, &f, 4); return bits; }
static inline uint64_t jabs_double_raw_bits(double d) { uint64_t bits; memcpy(&bits, &d, 8); return bits; }

/* int */
static inline int32_t jabs_iadd(int32_t a, int32_t b) { return (int32_t)((uint32_t)a + (uint32_t)b); }
static inline int32_t jabs_isub(int32_t a, int32_t b) { return (int32_t)((uint32_t)a - (uint32_t)b); }
static inline int32_t jabs_imul(int32_t a, int32_t b) { return (int32_t)((uint32_t)a * (uint32_t)b); }
static inline int32_t jabs_ineg(int32_t a) { return (int32_t)(0u - (uint32_t)a); }
/* Java division: INT_MIN / -1 wraps to INT_MIN (UB in C). Callers check b == 0 first (ArithmeticException). */
static inline int32_t jabs_idiv(int32_t a, int32_t b) { return (a == INT32_MIN && b == -1) ? INT32_MIN : a / b; }
static inline int32_t jabs_irem(int32_t a, int32_t b) { return b == -1 ? 0 : a % b; }
/* Shift counts are masked to 0..31 as Java does; shifting by >= 32 is UB in C. */
static inline int32_t jabs_ishl(int32_t a, int32_t n) { return (int32_t)((uint32_t)a << (n & 31)); }
/* Arithmetic right shift built from unsigned ops, so it does not rely on implementation-defined signed >>. */
static inline int32_t jabs_ishr(int32_t a, int32_t n) {
    uint32_t u = (uint32_t)a >> (n & 31);
    if (a < 0 && (n & 31)) u |= ~(UINT32_MAX >> (n & 31));
    return (int32_t)u;
}
static inline int32_t jabs_iushr(int32_t a, int32_t n) { return (int32_t)((uint32_t)a >> (n & 31)); }

/* long */
static inline int64_t jabs_ladd(int64_t a, int64_t b) { return (int64_t)((uint64_t)a + (uint64_t)b); }
static inline int64_t jabs_lsub(int64_t a, int64_t b) { return (int64_t)((uint64_t)a - (uint64_t)b); }
static inline int64_t jabs_lmul(int64_t a, int64_t b) { return (int64_t)((uint64_t)a * (uint64_t)b); }
static inline int64_t jabs_lneg(int64_t a) { return (int64_t)(0u - (uint64_t)a); }
static inline int64_t jabs_ldiv(int64_t a, int64_t b) { return (a == INT64_MIN && b == -1) ? INT64_MIN : a / b; }
static inline int64_t jabs_lrem(int64_t a, int64_t b) { return b == -1 ? 0 : a % b; }
static inline int64_t jabs_lshl(int64_t a, int32_t n) { return (int64_t)((uint64_t)a << (n & 63)); }
static inline int64_t jabs_lshr(int64_t a, int32_t n) {
    uint64_t u = (uint64_t)a >> (n & 63);
    if (a < 0 && (n & 63)) u |= ~(UINT64_MAX >> (n & 63));
    return (int64_t)u;
}
static inline int64_t jabs_lushr(int64_t a, int32_t n) { return (int64_t)((uint64_t)a >> (n & 63)); }

/* Conversions: Java saturates out-of-range floating values and maps NaN to 0. */
static inline int32_t jabs_d2i(double v) {
    if (v != v) return 0;
    if (v >= 2147483647.0) return INT32_MAX;
    if (v <= -2147483648.0) return INT32_MIN;
    return (int32_t)v;
}
static inline int64_t jabs_d2l(double v) {
    if (v != v) return 0;
    if (v >= 9223372036854775807.0) return INT64_MAX; /* 2^63 */
    if (v <= -9223372036854775808.0) return INT64_MIN;
    return (int64_t)v;
}
static inline int32_t jabs_f2i(float v) { return jabs_d2i((double)v); }
static inline int64_t jabs_f2l(float v) { return jabs_d2l((double)v); }
/* Keep the low 32 bits, as Java's l2i does. */
static inline int32_t jabs_l2i(int64_t v) { return (int32_t)(uint32_t)(uint64_t)v; }
static inline int32_t jabs_i2b(int32_t v) { return (int8_t)(uint8_t)(uint32_t)v; }
static inline int32_t jabs_i2c(int32_t v) { return (uint16_t)(uint32_t)v; }
static inline int32_t jabs_i2s(int32_t v) { return (int16_t)(uint16_t)(uint32_t)v; }

/* Comparisons: fcmpl/dcmpl yield -1 on NaN, fcmpg/dcmpg yield 1. */
static inline int32_t jabs_lcmp(int64_t a, int64_t b) { return (a > b) - (a < b); }
static inline int32_t jabs_fcmpl(float a, float b) { return a > b ? 1 : a == b ? 0 : -1; }
static inline int32_t jabs_fcmpg(float a, float b) { return a < b ? -1 : a == b ? 0 : 1; }
static inline int32_t jabs_dcmpl(double a, double b) { return a > b ? 1 : a == b ? 0 : -1; }
static inline int32_t jabs_dcmpg(double a, double b) { return a < b ? -1 : a == b ? 0 : 1; }

/* System properties from the command line, as `java -Dkey=value` gives them: each entry is
 * "key=value" ("key" alone means an empty value). Call before jabs_vm_startup; the strings must
 * outlive the program (argv does). Returns 0, or -1 if an entry has an empty key. Written once
 * before any second thread exists, read-only afterwards. */
int jabs_set_command_line_properties(int count, const char *const *defines);
extern int jabs_define_count;
extern const char *const *jabs_defines;

/* Set by programs whose class registry can name Java frames (jabs_vm_startup does it, before any
 * second thread exists): prints the Java methods on the current stack to stderr before an
 * uncompiled method stops the program. */
extern void (*jabs_on_not_compiled)(void);
void jabs_print_java_stack(void);
/* Throwable.fillInStackTrace(int) and StackTraceElement.initStackTraceElements: the stack as the
 * JVM records it (generated by the class registry). */
jabs_object *jabs_fill_in_stack_trace(jabs_object *throwable);
void jabs_init_stack_trace_elements(jabs_object *elements, jabs_object *backtrace, int32_t depth);

/* Reached a method that has no native code yet: stop with a diagnostic rather than guess.
 * The profile must run on the JVM backend until this method compiles. */
static inline _Noreturn void jabs_not_compiled(const char *method) {
    fprintf(stderr, "jabs: %s is not compiled to native code; run this profile on the JVM backend\n", method);
    if (jabs_on_not_compiled) jabs_on_not_compiled();
    abort();
}

/* Every compiled Java method: not inlined and no tail calls, so each has its own native frame
 * and stack walks (SecurityManager.getClassContext, stack traces) see every Java frame.
 * Define JABS_INLINE_METHODS to trade that exactness for speed. */
#ifndef JABS_METHOD
#if defined(JABS_INLINE_METHODS)
#define JABS_METHOD
#elif defined(__clang__)
#define JABS_METHOD __attribute__((noinline, disable_tail_calls))
#elif defined(__GNUC__)
#define JABS_METHOD __attribute__((noinline, optimize("no-optimize-sibling-calls")))
#else
#define JABS_METHOD
#endif
#endif

/* Function start addresses of the current native stack frames, innermost first (jabs_platform.c). */
int jabs_platform_stack_functions(const void **starts, int max);
jabs_object *jabs_class_new_instance(jabs_object *c);
jabs_object *jabs_unsafe_allocate_instance(jabs_object *c);
int32_t jabs_unsafe_should_be_initialized(jabs_object *c);

/* jabs_not_compiled naming the class involved. */
static inline _Noreturn void jabs_not_compiled_class(const char *what, const char *class_name) {
    fprintf(stderr, "jabs: %s (%s) is not compiled to native code; run this profile on the JVM backend\n", what, class_name);
    if (jabs_on_not_compiled) jabs_on_not_compiled();
    abort();
}

/* Arrays: header, length, then elements at their JVM width. The element class of a reference
 * array is recorded so covariant stores can be checked at runtime (ArrayStoreException). */
enum {
    JABS_COMPONENT_PRIMITIVE,
    JABS_COMPONENT_ANY,       /* Object[]: anything fits */
    JABS_COMPONENT_CLASS,     /* elements must be instances of `component` */
    JABS_COMPONENT_ARRAY,     /* elements must be arrays whose descriptor equals `component_descriptor` */
    JABS_COMPONENT_UNCHECKED  /* element type outside the unit or an interface: stores cannot be checked yet */
};
typedef struct jabs_array {
    jabs_object header;
    int32_t length;
    int32_t component_kind;
    const jabs_class *component;
    const char *descriptor;           /* this array's type, e.g. "[I" */
    const char *component_descriptor; /* element array type for JABS_COMPONENT_ARRAY */
} jabs_array;

/* A file from the input jars embedded in the program (resources.c), for the app class loader. */
typedef struct { const char *name; const unsigned char *data; size_t length; } jabs_resource;
extern const jabs_resource jabs_resources[];

/* Whether the CPU allows unaligned memory access (UnsafeConstants.UNALIGNED_ACCESS). */
#if defined(__x86_64__) || defined(__i386__) || defined(_M_X64) || defined(__aarch64__)
#define JABS_UNALIGNED_ACCESS 1
#else
#define JABS_UNALIGNED_ACCESS 0
#endif

/* Host facts for system properties (jabs_platform.c); UTF-8, NULL when unknown. */
enum {
    JABS_PLATFORM_FILE_SEPARATOR, JABS_PLATFORM_LINE_SEPARATOR, JABS_PLATFORM_PATH_SEPARATOR,
    JABS_PLATFORM_TMPDIR, JABS_PLATFORM_OS_NAME, JABS_PLATFORM_OS_ARCH, JABS_PLATFORM_OS_VERSION,
    JABS_PLATFORM_NATIVE_ENCODING, JABS_PLATFORM_JNU_ENCODING, JABS_PLATFORM_USER_DIR,
    JABS_PLATFORM_USER_HOME, JABS_PLATFORM_USER_NAME, JABS_PLATFORM_LANGUAGE, JABS_PLATFORM_COUNTRY,
    JABS_PLATFORM_DATA_MODEL, JABS_PLATFORM_ENDIAN, JABS_PLATFORM_UNICODE_ENCODING, JABS_PLATFORM_EXE_DIR,
    JABS_PLATFORM_LIBRARY_PREFIX, JABS_PLATFORM_LIBRARY_SUFFIX
};
const char *jabs_platform_value(int which);
const char *jabs_platform_common(int which);
/* Windows HANDLE of stdin/stdout/stderr (fd 0-2), else -1. */
int64_t jabs_platform_std_handle(int32_t fd);
/* Write all bytes to a file descriptor; -1 on failure. */
int64_t jabs_platform_write(int32_t fd, const void *data, size_t length);
/* A VarHandle made by the runtime: what it reaches and the value type ('I', 'J', ..., 'L').
 * shape: 0 static field (address), 1 instance field (offset from the object), 2 array element,
 * 3 byte[] viewed as an array of `kind` (MethodHandles.byteArrayViewVarHandle): the index is a
 * byte offset, `scale` the view type's size and `big_endian` its byte order. */
enum { JABS_VH_STATIC = 0, JABS_VH_FIELD = 1, JABS_VH_ELEMENT = 2, JABS_VH_BYTE_VIEW = 3 };
typedef struct { char kind; uint8_t shape; bool big_endian; int32_t scale; int64_t offset; void *address; } jabs_vh;
/* The handle data of a runtime-made VarHandle; NULL for any other object. */
const jabs_vh *jabs_vh_of(jabs_object *o);
/* The accessed address; NULL with NullPointerException, ClassCastException (a byte view given
 * something other than a byte[]) or ArrayIndexOutOfBoundsException pending. */
void *jabs_vh_address(const jabs_vh *h, jabs_object *o, int32_t index);

/* A byte view's element as raw bits, assembled byte by byte in the handle's order, so neither
 * the host's byte order nor the element's alignment matters (views allow any byte offset). */
static inline uint64_t jabs_vh_view_load(const jabs_vh *h, const void *p) {
    if (h->scale < 1 || h->scale > 8) jabs_not_compiled("byte-array view VarHandle with a bad element size");
    const uint8_t *b = (const uint8_t *)p;
    uint64_t bits = 0;
    for (int32_t i = 0; i < h->scale; i++) {
        int32_t at = h->big_endian ? i : h->scale - 1 - i;
        bits = (bits << 8) | b[at];
    }
    return bits;
}
static inline void jabs_vh_view_store(const jabs_vh *h, void *p, uint64_t bits) {
    if (h->scale < 1 || h->scale > 8) jabs_not_compiled("byte-array view VarHandle with a bad element size");
    uint8_t *b = (uint8_t *)p;
    for (int32_t i = 0; i < h->scale; i++) {
        int32_t at = h->big_endian ? h->scale - 1 - i : i;
        b[at] = (uint8_t)bits;
        bits >>= 8;
    }
}
int32_t jabs_array_scale(jabs_object *array_class);

/* Atomic accesses (Unsafe, VarHandles, volatile fields) use the clang/GCC __atomic builtins:
 * the memory they touch is declared without _Atomic, since the same field is also accessed
 * plainly. VarHandle getAndAdd on a float/double is a compare-and-set loop (no CPU adds floats
 * in memory atomically) and returns the old value. A loop that keeps losing this long means
 * something else is broken, so it stops instead of spinning forever. */
#define JABS_ATOMIC_ADD_RETRIES 100000000u
static inline float jabs_atomic_add_float(float *p, float v, int order) {
    float old;
    __atomic_load(p, &old, __ATOMIC_RELAXED);
    for (uint32_t i = 0; i < JABS_ATOMIC_ADD_RETRIES; i++) {
        float sum = old + v;
        if (__atomic_compare_exchange(p, &old, &sum, 1, order, __ATOMIC_RELAXED)) return old;
    }
    jabs_not_compiled("VarHandle.getAndAdd on a float that never stopped changing");
}
static inline double jabs_atomic_add_double(double *p, double v, int order) {
    double old;
    __atomic_load(p, &old, __ATOMIC_RELAXED);
    for (uint32_t i = 0; i < JABS_ATOMIC_ADD_RETRIES; i++) {
        double sum = old + v;
        if (__atomic_compare_exchange(p, &old, &sum, 1, order, __ATOMIC_RELAXED)) return old;
    }
    jabs_not_compiled("VarHandle.getAndAdd on a double that never stopped changing");
}

/* Generated glue the java.io natives use (see io_accessor_functions in cgen.rs). */
jabs_object *jabs_file_path_of(jabs_object *file);
/* Unsafe.objectFieldOffset1 (class and name) and objectFieldOffset0 (a reflect Field). */
int64_t jabs_field_offset(jabs_object *c, jabs_object *name);
int64_t jabs_unsafe_field_offset(jabs_object *field);
/* ClassLoader.findLoadedClass0 (generated with the class registry). */
jabs_object *jabs_find_loaded_class(jabs_object *loader, jabs_object *binary);
/* VarHandle.withInvokeExactBehavior/withInvokeBehavior of a JABS handle (generated). */
jabs_object *jabs_vh_with_exact(jabs_object *handle, int32_t exact);
jabs_object **jabs_stream_fd_slot(jabs_object *stream);
int32_t *jabs_descriptor_fd_slot(jabs_object *descriptor);
void jabs_descriptor_set_handle(jabs_object *descriptor, int64_t handle);
void jabs_descriptor_set_append(jabs_object *descriptor, int32_t append);
jabs_object *jabs_new_string_array(int32_t length);
void jabs_raise_message(int kind, const char *utf8);
char *jabs_string_to_utf8(jabs_object *s);
jabs_object *jabs_string_from_utf8(const char *text);
/* java.lang.management natives' glue (jabs_management.c): VMManagementImpl.getVmArguments0. */
jabs_object *jabs_management_vm_arguments(void);
/* A MemoryUsage built with its constructor (generated: management::glue in src/cgen/management.rs). */
jabs_object *jabs_management_memory_usage(int64_t init, int64_t used, int64_t committed, int64_t max);
/* java.net and java.nio natives' glue: generated (net::glue in src/cgen/net.rs). */
/* Exceptions jabs_net_raise builds, in the order of RAISED in src/cgen/net.rs. */
enum {
    JABS_NET_UNKNOWN_HOST,
    JABS_NET_SOCKET,
    JABS_NET_BIND,
    JABS_NET_CONNECT,
    JABS_NET_NO_ROUTE,
    JABS_NET_PROTOCOL,
    JABS_NET_CONNECTION_RESET
};
void jabs_net_raise(int32_t kind, const char *utf8);
jabs_object *jabs_net_new_inet4(int32_t address, jabs_object *host);
jabs_object *jabs_net_new_inet6(const uint8_t *bytes, int32_t scope_id, jabs_object *host);
jabs_object *jabs_net_new_inet_array(int32_t length);
int32_t jabs_net_inet_read(jabs_object *address, uint8_t *bytes, int32_t *scope_id);
jabs_object *jabs_net_new_socket_address(jabs_object *address, int32_t port);
int32_t jabs_net_enhanced_host_info(void);
int32_t jabs_net_prefer_ipv4_stack(void);
/* The natives themselves (handwritten: jabs_net.c). */
int32_t jabs_net_ipv4_available(void);
int32_t jabs_net_ipv6_available(void);
jabs_object *jabs_net_local_host_name(void);
jabs_object *jabs_net_lookup(jabs_object *host, int32_t characteristics);
jabs_object *jabs_net_host_by_addr(jabs_object *address);
/* NIO sockets (sun.nio.ch.Net, IOUtil, SocketDispatcher; Linux's UnixDispatcher, NativeThread). */
enum {
    JABS_NET_VALUE_POLLIN,
    JABS_NET_VALUE_POLLOUT,
    JABS_NET_VALUE_POLLERR,
    JABS_NET_VALUE_POLLHUP,
    JABS_NET_VALUE_POLLNVAL,
    JABS_NET_VALUE_POLLCONN,
    JABS_NET_VALUE_REUSE_PORT,
    JABS_NET_VALUE_EXCLUSIVE_BIND,
    JABS_NET_VALUE_SHUTDOWN_WRITE_BEFORE_CLOSE,
    JABS_NET_VALUE_SET_BOTH_IPV4_IPV6_OPTIONS,
    JABS_NET_VALUE_IPV6_JOINS_IPV4_GROUP,
    JABS_NET_VALUE_JOIN6_WITH_IPV4_GROUP,
    JABS_NET_VALUE_IPV6_OPTIONS_WITH_IPV4_LOCAL,
    JABS_NET_VALUE_IOV_MAX,
    JABS_NET_VALUE_WRITEV_MAX
};
int64_t jabs_net_value(int32_t which);
int32_t jabs_net_fd_val(jabs_object *fdo);
void jabs_net_set_fd_val(jabs_object *fdo, int32_t fd);
int32_t jabs_net_socket(int32_t prefer_ipv6, int32_t stream, int32_t reuse, int32_t fast_loopback);
void jabs_net_bind(jabs_object *fdo, int32_t prefer_ipv6, int32_t exclusive, jabs_object *address, int32_t port);
void jabs_net_listen(jabs_object *fdo, int32_t backlog);
int32_t jabs_net_connect(int32_t prefer_ipv6, jabs_object *fdo, jabs_object *address, int32_t port);
int32_t jabs_net_accept(jabs_object *fdo, jabs_object *newfdo, jabs_object *addresses);
int32_t jabs_net_port(jabs_object *fdo, int32_t peer);
jabs_object *jabs_net_address(jabs_object *fdo, int32_t peer);
void jabs_net_shutdown(jabs_object *fdo, int32_t how);
int32_t jabs_net_get_option(jabs_object *fdo, int32_t may_need_conversion, int32_t level, int32_t option);
void jabs_net_set_option(jabs_object *fdo, int32_t may_need_conversion, int32_t level, int32_t option, int32_t value, int32_t is_ipv6);
int32_t jabs_net_poll(jabs_object *fdo, int32_t events, int64_t timeout);
int32_t jabs_net_poll_connect(jabs_object *fdo, int64_t timeout);
int32_t jabs_net_available(jabs_object *fdo);
void jabs_net_configure_blocking(jabs_object *fdo, int32_t blocking);
int32_t jabs_net_read(jabs_object *fdo, int64_t address, int32_t length);
int32_t jabs_net_write(jabs_object *fdo, int64_t address, int32_t length);
void jabs_net_close_socket(int32_t fd);
int32_t jabs_net_drain(int32_t fd);
int32_t jabs_net_write1(int32_t fd, int8_t byte);
void jabs_net_ioutil_init(void);
int32_t jabs_net_fd_limit(void);
int64_t jabs_net_make_pipe(int32_t blocking);
void jabs_net_dispatcher_init(void);
void jabs_net_pre_close(jabs_object *fdo);
void jabs_net_close_fd(int32_t fd);
void jabs_net_native_thread_init(void);
int64_t jabs_net_native_thread_current(void);
void jabs_net_native_thread_signal(int64_t thread);
/* Selectors (WEPoll on Windows; EPoll and EventFD on Linux), Unix domain sockets. */
int32_t jabs_net_epoll_event_size(void);
int32_t jabs_net_epoll_events_offset(void);
int32_t jabs_net_epoll_data_offset(void);
int64_t jabs_net_epoll_create(void);
int32_t jabs_net_epoll_ctl(int64_t handle, int32_t op, int64_t socket_value, int32_t events);
int32_t jabs_net_epoll_wait(int64_t handle, int64_t address, int32_t maxevents, int32_t timeout);
void jabs_net_epoll_close(int64_t handle);
int32_t jabs_net_linux_epoll_create(void);
int32_t jabs_net_linux_epoll_ctl(int32_t epfd, int32_t op, int32_t fd, int32_t events);
int32_t jabs_net_linux_epoll_wait(int32_t epfd, int64_t address, int32_t maxevents, int32_t timeout);
int32_t jabs_net_eventfd(void);
int32_t jabs_net_eventfd_set(int32_t efd);
int32_t jabs_net_uds_init(void);
int32_t jabs_net_uds_socket(void);
void jabs_net_uds_bind(jabs_object *fdo, jabs_object *path);
int32_t jabs_net_uds_connect(jabs_object *fdo, jabs_object *path);
int32_t jabs_net_uds_accept(jabs_object *fdo, jabs_object *newfdo, jabs_object *addresses);
jabs_object *jabs_net_uds_local_address(jabs_object *fdo);
int32_t jabs_net_generate_seed(jabs_object *bytes);
/* jdk.net's WindowsSocketOptions and LinuxSocketOptions: which extended options the OS has. */
enum { JABS_NET_EXTENDED_KEEPALIVE, JABS_NET_EXTENDED_QUICKACK, JABS_NET_EXTENDED_NAPI_ID };
int32_t jabs_net_extended_option_supported(int32_t which);
/* NetworkInterface (jabs_net.c) and the objects it builds (generated, src/cgen/net.rs). */
jabs_object *jabs_net_interfaces(void);
int32_t jabs_net_interface_is_loopback(int32_t index);
jabs_object *jabs_net_interface_mac(int32_t index);
int32_t jabs_net_address_is_bound(jabs_object *address);
jabs_object *jabs_net_new_interface(jabs_object *name, jabs_object *display_name, int32_t index, jabs_object *addresses, jabs_object *bindings);
jabs_object *jabs_net_new_interface_array(int32_t length);
jabs_object *jabs_net_new_interface_address(jabs_object *address, jabs_object *broadcast, int16_t prefix);
jabs_object *jabs_net_new_interface_address_array(int32_t length);
void jabs_net_set_scope_interface(jabs_object *inet6, jabs_object *owner);
/* java.io natives' glue (jabs_io.c). */
/* System.mapLibraryName: "name" -> "name.dll" / "libname.so" / "libname.dylib". */
jabs_object *jabs_map_library_name(jabs_object *name);
int32_t jabs_io_attributes(jabs_object *file, int32_t with_hidden);
int32_t jabs_io_check_access(jabs_object *file, int32_t access);
int64_t jabs_io_last_modified(jabs_object *file);
int64_t jabs_io_length(jabs_object *file);
int32_t jabs_io_set_permission(jabs_object *file, int32_t access, int32_t enable, int32_t owner_only);
int32_t jabs_io_create_exclusively(jabs_object *path);
jabs_object *jabs_io_list(jabs_object *file);
int32_t jabs_io_create_directory(jabs_object *file);
int32_t jabs_io_set_last_modified(jabs_object *file, int64_t ms);
int32_t jabs_io_set_read_only(jabs_object *file);
int32_t jabs_io_delete(jabs_object *file, int32_t allow_read_only);
int32_t jabs_io_rename(jabs_object *from, jabs_object *to);
int64_t jabs_io_space(jabs_object *file, int32_t which);
jabs_object *jabs_io_canonicalize(jabs_object *path);
jabs_object *jabs_io_final_path(jabs_object *path);
jabs_object *jabs_io_drive_directory(int32_t drive);
int32_t jabs_io_raf_flags(int32_t mode);
void jabs_io_open(jabs_object *stream, jabs_object *name, int32_t flags);
int32_t jabs_io_read_byte(jabs_object *stream);
int32_t jabs_io_read_bytes(jabs_object *stream, jabs_object *bytes, int32_t off, int32_t len);
void jabs_io_write_byte(jabs_object *stream, int32_t b);
void jabs_io_write_bytes(jabs_object *stream, jabs_object *bytes, int32_t off, int32_t len);
int64_t jabs_io_position(jabs_object *stream);
void jabs_io_seek(jabs_object *stream, int64_t position);
int64_t jabs_io_skip(jabs_object *stream, int64_t n);
int64_t jabs_io_stream_length(jabs_object *stream);
void jabs_io_set_length(jabs_object *stream, int64_t length);
int32_t jabs_io_available(jabs_object *stream);
int32_t jabs_io_is_regular(jabs_object *fdo);
void jabs_io_close_descriptor(jabs_object *fdo);
void jabs_io_cleanup_close(int32_t fd, int64_t handle);
void jabs_io_sync(jabs_object *fdo);

/* java.nio.file natives (jabs_nio_fs.c; bodies in src/cgen/net.rs) and their generated glue. */
jabs_object *jabs_string_from_utf16(const uint16_t *units, size_t count);
/* A FileDescriptor's handle field (Windows; -1 where the JDK has none). */
int64_t jabs_descriptor_handle(jabs_object *descriptor);
void jabs_nio_windows_exception(int32_t error);
void jabs_nio_unix_exception(int32_t error);
void jabs_nio_set_first_file(jabs_object *first, int64_t handle, jabs_object *name, int32_t attributes);
/* What UnixNativeDispatcher.stat0 stores into a UnixFileAttributes. */
typedef struct {
    int32_t mode, nlink, uid, gid, birthtime_available;
    int64_t ino, dev, rdev, size;
    int64_t atime_sec, atime_nsec, mtime_sec, mtime_nsec, ctime_sec, ctime_nsec, birthtime_sec, birthtime_nsec;
} jabs_nio_stat_result;
void jabs_nio_set_unix_attributes(jabs_object *attributes, const jabs_nio_stat_result *s);
int64_t jabs_nio_create_file(int64_t path, int32_t access, int32_t share, int64_t descriptor, int32_t disposition, int32_t flags);
void jabs_nio_close_handle(int64_t handle);
/* CloseHandle for FileDescriptor.close: 0 when it failed (the caller raises). */
int32_t jabs_nio_close_descriptor_handle(int64_t handle);
/* FileDispatcherImpl.size0 / seek0 (Windows) and UnixFileDispatcherImpl.size0 / seek0. */
/* Perf.createLong: a direct ByteBuffer over a new native counter (src/cgen/nio_fs.rs glue). */
jabs_object *jabs_perf_create_long(jabs_object *name, int64_t value);
int64_t jabs_nio_file_size(jabs_object *fdo);
int64_t jabs_nio_file_seek(jabs_object *fdo, int64_t offset);
int64_t jabs_nio_unix_size(jabs_object *fdo);
int64_t jabs_nio_unix_seek(jabs_object *fdo, int64_t offset);
void jabs_nio_delete_file(int64_t path);
void jabs_nio_find_first_file(int64_t path, jabs_object *first);
jabs_object *jabs_nio_find_next_file(int64_t handle, int64_t data);
void jabs_nio_find_close(int64_t handle);
void jabs_nio_get_file_attributes_ex(int64_t path, int64_t data);
void jabs_nio_get_file_information(int64_t handle, int64_t data);
jabs_object *jabs_nio_get_full_path_name(int64_t path);
void jabs_nio_get_reparse_point(int64_t handle, int64_t buffer, int32_t size);
int32_t jabs_nio_file_read(jabs_object *fdo, int64_t address, int32_t length);
int64_t jabs_nio_allocation_granularity(void);
int32_t jabs_nio_max_direct_transfer_size(void);
int32_t jabs_nio_unix_init(void);
jabs_object *jabs_nio_getcwd(void);
int32_t jabs_nio_open(int64_t path, int32_t flags, int32_t mode);
int32_t jabs_nio_stat(int64_t path, jabs_object *attributes, int32_t follow);
void jabs_nio_unlink(int64_t path);
void jabs_nio_close_int_fd(int32_t fd);
int32_t jabs_nio_unix_read(jabs_object *fdo, int64_t address, int32_t length);
int32_t jabs_nio_access(int64_t path, int32_t mode);
void jabs_nio_close(int32_t fd);
int32_t jabs_nio_dup(int32_t fd);
int64_t jabs_nio_fdopendir(int32_t fd);
void jabs_nio_closedir(int64_t dir);
jabs_object *jabs_nio_readdir(int64_t dir);
jabs_object *jabs_nio_realpath(int64_t path);
/* java.lang.ProcessEnvironment (jabs_nio_fs.c): Windows's environment block, Linux's environ. */
jabs_object *jabs_process_environment_block(void);
jabs_object *jabs_process_environ(void);

/* java.util.zip (jabs_zip.c, glue in src/cgen/zip.rs). */
int64_t jabs_zip_inflate_init(int32_t nowrap);
void jabs_zip_inflate_set_dict(int64_t stream, const uint8_t *dict, int32_t len);
int64_t jabs_zip_inflate(jabs_object *inflater, int64_t stream, const uint8_t *in, int32_t in_len, uint8_t *out, int32_t out_len);
int32_t jabs_zip_inflate_adler(int64_t stream);
void jabs_zip_inflate_reset(int64_t stream);
void jabs_zip_inflate_end(int64_t stream);
int64_t jabs_zip_deflate_init(int32_t level, int32_t strategy, int32_t nowrap);
void jabs_zip_deflate_set_dict(int64_t stream, const uint8_t *dict, int32_t len);
int64_t jabs_zip_deflate(int64_t stream, const uint8_t *in, int32_t in_len, uint8_t *out, int32_t out_len, int32_t flush, int32_t params);
int32_t jabs_zip_deflate_adler(int64_t stream);
void jabs_zip_deflate_reset(int64_t stream);
void jabs_zip_deflate_end(int64_t stream);
void jabs_zip_data_format_exception(const char *message);
void jabs_zip_illegal_argument(const char *message);
void jabs_zip_set_inflater_consumed(jabs_object *inflater, int32_t in_consumed, int32_t out_consumed);

/* Files (jabs_files.c): UTF-8 paths, one implementation per OS family. */
enum { JABS_FS_EXISTS = 0x01, JABS_FS_REGULAR = 0x02, JABS_FS_DIRECTORY = 0x04, JABS_FS_HIDDEN = 0x08 };
enum { JABS_FS_ACCESS_READ = 0x04, JABS_FS_ACCESS_WRITE = 0x02, JABS_FS_ACCESS_EXECUTE = 0x01 };
enum { JABS_FS_SPACE_TOTAL = 0, JABS_FS_SPACE_FREE = 1, JABS_FS_SPACE_USABLE = 2 };
enum {
    JABS_OPEN_READ = 1, JABS_OPEN_WRITE = 2, JABS_OPEN_CREATE = 4, JABS_OPEN_TRUNCATE = 8,
    JABS_OPEN_APPEND = 16, JABS_OPEN_SYNC = 32, JABS_OPEN_DSYNC = 64, JABS_OPEN_DELETE_ON_CLOSE = 128
};
/* Why the last failing call failed (the OS's own message). */
const char *jabs_files_error(void);
/* JABS_FS_* bits without HIDDEN; *hidden set separately (only Windows has the attribute). */
int32_t jabs_fs_attributes(const char *path, int32_t *hidden);
int32_t jabs_fs_access(const char *path, int32_t access);
/* Milliseconds since 1970, 0 when unknown. */
int64_t jabs_fs_mtime(const char *path);
int64_t jabs_fs_length(const char *path);
/* 1 created, 0 already exists, -1 error. */
int32_t jabs_fs_create_exclusive(const char *path);
int32_t jabs_fs_delete(const char *path, int32_t allow_read_only);
int32_t jabs_fs_mkdir(const char *path);
int32_t jabs_fs_rename(const char *from, const char *to);
int32_t jabs_fs_set_mtime(const char *path, int64_t ms);
int32_t jabs_fs_set_read_only(const char *path);
int32_t jabs_fs_set_permission(const char *path, int32_t access, int32_t enable, int32_t owner_only);
/* malloc'd names (each malloc'd), NULL when the path is not a readable directory. */
char **jabs_fs_list(const char *path, int32_t *count);
/* malloc'd results, NULL on error. */
char *jabs_fs_canonicalize(const char *path);
char *jabs_fs_final_path(const char *path);
char *jabs_fs_drive_directory(int32_t drive);
/* Bit i set when drive 'A'+i exists (Windows); 0 elsewhere. */
int32_t jabs_fs_roots(void);
int64_t jabs_fs_space(const char *path, int32_t which);
/* File descriptors: -1 (or 0 for the int32 predicates) on error. jabs_fd_read returns 0 at EOF. */
int32_t jabs_fd_open(const char *path, int32_t flags);
int64_t jabs_fd_read(int32_t fd, void *buffer, size_t length);
int64_t jabs_fd_seek(int32_t fd, int64_t offset, int32_t whence);
int64_t jabs_fd_length(int32_t fd);
int32_t jabs_fd_set_length(int32_t fd, int64_t length);
int32_t jabs_fd_is_regular(int32_t fd);
int64_t jabs_fd_available(int32_t fd);
int32_t jabs_fd_close(int32_t fd);
int32_t jabs_fd_sync(int32_t fd);

/* The program's .jab (jabs_jab.c): its class path and java.home. The generated program says
 * whether it has one: jabs_jab_mode 0 none, 1 a .jab, 2 a .jab and a recording of the java.home
 * files the run touches (java.home is then the build JDK, jabs_jab_record_home). */
extern const int32_t jabs_jab_mode;
extern const char jabs_jab_id[];
extern const char jabs_jab_record_home[];
extern const char jabs_jab_record_log[];
/* Find and check the .jab before any Java code runs; a missing or damaged one stops the program. */
int jabs_jab_start(void);
/* java.class.path ("" without a .jab), java.home (NULL without a .jab), the .jab's path as a
 * String (null without one: jabs.rt.Natives.jabPath). */
const char *jabs_jab_class_path(void);
const char *jabs_jab_java_home(void);
jabs_object *jabs_jab_path_string(void);
/* The virtual tree the file natives consult before the operating system: a path is OUTSIDE (the
 * OS's), a FILE or DIRECTORY of the .jab, or MISSING (inside the .jab's tree, not there). */
enum { JABS_VFS_OUTSIDE, JABS_VFS_FILE, JABS_VFS_DIRECTORY, JABS_VFS_MISSING };
typedef struct {
    int32_t kind;
    int32_t parent_exists; /* MISSING: whether its directory is there */
    uint32_t index;
    uint64_t start, size, id; /* id: unique per node, for inode numbers and file indexes */
} jabs_vfs_node;
int32_t jabs_vfs_find(const char *path, jabs_vfs_node *node);
#ifdef _WIN32
int32_t jabs_vfs_find_wide(const uint16_t *path, jabs_vfs_node *node);
#endif
/* Every virtual node's last-modified time (the .jab's own), and the directory the .jab is in. */
int64_t jabs_vfs_mtime_ms(void);
const char *jabs_vfs_directory(void);
/* A recording build notes a directory listed under java.home. */
void jabs_vfs_note_listing(const char *path);
/* malloc'd names directly inside a DIRECTORY node. */
char **jabs_vfs_list(const jabs_vfs_node *directory, int32_t *count);
/* Virtual descriptors: [BASE, BASE + CAP), never an OS descriptor or HANDLE. listing: 0 a file's
 * contents, 1 a directory's names, 2 the same after "." and ".." (Windows FindFirstFile). */
#define JABS_VFS_DESCRIPTOR_BASE 0x6A610000
#define JABS_VFS_DESCRIPTOR_CAP 1024
int jabs_vfs_is_descriptor(int64_t d);
int64_t jabs_vfs_open(const jabs_vfs_node *node, int32_t listing);
/* Bytes read (0 at the end), or -1 for a bad descriptor or a failed read of the .jab. */
int64_t jabs_vfs_read(int64_t d, void *buffer, size_t length);
int64_t jabs_vfs_seek(int64_t d, int64_t offset, int32_t whence);
int64_t jabs_vfs_size(int64_t d);
int64_t jabs_vfs_position(int64_t d);
int32_t jabs_vfs_descriptor_kind(int64_t d, uint64_t *id);
int32_t jabs_vfs_next_name(int64_t d, char **name, int32_t *is_directory, uint64_t *size);
int32_t jabs_vfs_close(int64_t d);
/* java.util.zip.Inflater's natives without zlib (jabs_jab.c): a stream that never inflates. */
int64_t jabs_inflater_init(int32_t nowrap);
void jabs_inflater_reset(int64_t address);
void jabs_inflater_end(int64_t address);
int32_t jabs_inflater_adler(int64_t address);

/* SetErrorMode on Windows (returns the previous mode); 0 elsewhere. */
int64_t jabs_platform_set_error_mode(int64_t mode);
/* Sleep for at least this many nanoseconds. */
void jabs_sleep_nanos(int64_t nanos);
/* VM.getNanoTimeAdjustment: wall-clock nanoseconds since `offset_seconds`, or -1 if too far. */
int64_t jabs_nano_time_adjustment(int64_t offset_seconds);

/* Elements start right after the header, aligned for any element type. */
static inline void *jabs_array_data(jabs_array *a) {
    return (char *)a + ((sizeof(jabs_array) + 7) & ~(size_t)7);
}

/* Shared record for every array object: arrays are instances of no compiled class. */
extern const jabs_class jabs_array_class_record;
static inline const jabs_class *jabs_array_class(void) { return &jabs_array_class_record; }

static inline int32_t jabs_is_instance(const jabs_object *o, const jabs_class *target);
static inline int32_t jabs_is_array_instance(const jabs_object *o, const char *target, const jabs_class *component);

/* aastore check: may `value` (non-null) be stored in this array? */
static inline int jabs_array_store_ok(jabs_array *a, jabs_object *value) {
    switch (a->component_kind) {
    case JABS_COMPONENT_ANY: return 1;
    case JABS_COMPONENT_CLASS: return jabs_is_instance(value, a->component);
    case JABS_COMPONENT_ARRAY: {
        if (value->cls != jabs_array_class()) return 0;
        const char *row = a->component_descriptor;
        if (!row || row[0] != '[') jabs_not_compiled("aastore into an array of arrays with no row type recorded");
        /* Rows of a primitive type fit only exactly; rows of references are covariant, so a row
         * fits exactly when it is an instance of the row type (a String[] in a CharSequence[][]). */
        if (row[1] != 'L' && row[1] != '[') return strcmp(((jabs_array *)value)->descriptor, row) == 0;
        if (strcmp(((jabs_array *)value)->descriptor, row) == 0) return 1;
        /* A covariant row needs the row element's record; without it the answer would be a
         * wrong ArrayStoreException, so stop instead. */
        if (row[1] == 'L' && !a->component && strcmp(row, "[Ljava/lang/Object;") != 0)
            jabs_not_compiled("aastore of a row whose element class is outside the unit");
        return jabs_is_array_instance(value, row, a->component);
    }
    default: jabs_not_compiled("aastore into an array whose element type is outside the unit");
    }
    return 0;
}

static inline jabs_object *jabs_new_array(int32_t length, size_t element_size, int32_t kind, const jabs_class *component,
                                          const char *descriptor, const char *component_descriptor);

/* multianewarray: an array of type `descriptor` with its first `dimensions` levels allocated
 * from `counts` (all already >= 0), each level checked as anewarray/newarray would make it.
 * `element` is the record of the innermost element class, or NULL (Object, primitives, or a
 * class outside the unit). Descriptors of the levels are suffixes of `descriptor`, which must
 * outlive the program (a string literal). NULL with jabs_pending set when allocation fails.
 * Recursion depth is at most `dimensions` (JVMS: at most 255). */
static inline jabs_object *jabs_multi_new_array(const char *descriptor, int dimensions, const int32_t *counts,
                                                const jabs_class *element) {
    if (dimensions < 1 || dimensions > 255 || descriptor[0] != '[') jabs_not_compiled("multianewarray with a malformed type");
    const char *component = descriptor + 1;
    int32_t length = counts[0];
    if (dimensions == 1) {
        static const char codes[] = "ZBCSIJFD";
        static const size_t sizes[] = { 1, 1, 2, 2, 4, 8, 4, 8 };
        const char *code = component[0] ? strchr(codes, component[0]) : NULL;
        if (code) return jabs_new_array(length, sizes[code - codes], JABS_COMPONENT_PRIMITIVE, NULL, descriptor, NULL);
        if (component[0] == '[') {
            /* Rows: a one-level reference row keeps its element's record (as anewarray does). */
            const jabs_class *row_element = component[1] == 'L' ? element : NULL;
            return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_ARRAY, row_element, descriptor, component);
        }
        if (strcmp(component, "Ljava/lang/Object;") == 0)
            return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_ANY, NULL, descriptor, NULL);
        return jabs_new_array(length, sizeof(jabs_object *), element ? JABS_COMPONENT_CLASS : JABS_COMPONENT_UNCHECKED,
                              element, descriptor, NULL);
    }
    if (component[0] != '[') jabs_not_compiled("multianewarray with more dimensions than its type");
    const jabs_class *row_element = component[1] == 'L' ? element : NULL;
    jabs_object *outer = jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_ARRAY, row_element, descriptor, component);
    if (!outer) return NULL;
    for (int32_t i = 0; i < length; i++) {
        jabs_object *row = jabs_multi_new_array(component, dimensions - 1, counts + 1, element);
        if (!row) return NULL;
        ((jabs_object **)jabs_array_data((jabs_array *)outer))[i] = row;
    }
    return outer;
}

/* Allocate a zeroed array (zero bytes are every element's default); length is already >= 0. */
static inline jabs_object *jabs_new_array(int32_t length, size_t element_size, int32_t kind, const jabs_class *component,
                                          const char *descriptor, const char *component_descriptor) {
    size_t header = (sizeof(jabs_array) + 7) & ~(size_t)7;
    if ((size_t)length > (SIZE_MAX - header) / element_size) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }
    jabs_array *a = (jabs_array *)calloc(1, header + (size_t)length * element_size);
    if (a) atomic_fetch_add_explicit(&jabs_heap_bytes, (int64_t)(header + (size_t)length * element_size), memory_order_relaxed);
    if (!a) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }
    a->header.cls = jabs_array_class();
    a->length = length;
    a->component_kind = kind;
    a->component = component;
    a->descriptor = descriptor;
    a->component_descriptor = component_descriptor;
    return &a->header;
}

/* Whether class record `c` is `target` or a subtype of it (superclass chain or interface list). */
static inline int32_t jabs_class_assignable(const jabs_class *c, const jabs_class *target) {
    if (target->is_interface) {
        for (uint32_t i = 0; i < c->interfaces_len; i++) {
            if (c->interfaces[i] == target) return 1;
        }
        return 0;
    }
    for (; c; c = c->super) {
        if (c == target) return 1;
    }
    return 0;
}

/* instanceof for a class or interface target. Arrays implement exactly Cloneable and Serializable. */
static inline int32_t jabs_is_instance(const jabs_object *o, const jabs_class *target) {
    if (o->cls == jabs_array_class()) {
        return target->is_interface &&
               (strcmp(target->name, "java/lang/Cloneable") == 0 || strcmp(target->name, "java/io/Serializable") == 0);
    }
    return jabs_class_assignable(o->cls, target);
}

/* instanceof for an array type `target` (a descriptor such as "[I" or "[LShape;"). `component`
 * is the record of a class element type when the unit has one. Reference arrays are covariant. */
static inline int32_t jabs_is_array_instance(const jabs_object *o, const char *target, const jabs_class *component) {
    if (o->cls != jabs_array_class()) return 0;
    const jabs_array *a = (const jabs_array *)o;
    if (strcmp(a->descriptor, target) == 0) return 1;
    int reference = a->descriptor[1] == 'L' || a->descriptor[1] == '[';
    /* Primitive arrays are instances only of their exact type (checked above). */
    if (!reference) return 0;
    if (strcmp(target, "[Ljava/lang/Object;") == 0) return 1;
    if (target[1] == 'L') {
        /* Elements that are arrays are Object, Cloneable and Serializable, nothing else. */
        if (a->descriptor[1] == '[') {
            return strcmp(target, "[Ljava/lang/Cloneable;") == 0 || strcmp(target, "[Ljava/io/Serializable;") == 0;
        }
        switch (a->component_kind) {
        case JABS_COMPONENT_CLASS: return component && jabs_class_assignable(a->component, component);
        case JABS_COMPONENT_ANY: return 0; /* an Object[] is only an Object[] */
        default: break;
        }
    } else if (target[1] != '[' || a->descriptor[1] != '[') {
        return 0; /* never a primitive array; a one-dimensional reference array is never an array of arrays */
    }
    jabs_not_compiled("array type test whose element type is outside the unit or multi-dimensional");
}

/* Object monitors (runtime/jabs_sync.c). The owner is the calling OS thread, never a Java Thread
 * object. Callers raise NullPointerException for a null object first; these stop the program
 * on NULL rather than guess.
 * monitorenter: blocks until this thread owns the monitor (re-entering adds a level). */
void jabs_monitor_enter(jabs_object *o);
/* monitorexit: 1 = one level released; 0 = this thread does not own the monitor (the caller
 * raises IllegalMonitorStateException). */
int jabs_monitor_exit(jabs_object *o);
/* Whether this thread owns the monitor (Thread.holdsLock). */
int jabs_monitor_holds(jabs_object *o);
/* notify / notifyAll: 1 = done (also when nobody waits; a notification is never remembered);
 * 0 = this thread does not own the monitor (IllegalMonitorStateException). notify wakes the
 * longest-waiting thread. */
int jabs_monitor_notify(jabs_object *o);
int jabs_monitor_notify_all(jabs_object *o);
/* Object.wait: release the monitor completely, sleep until notified, interrupted or `millis`
 * passed (0 = no limit; negative stops the program, Object.wait rejects it first), then own the
 * monitor again at the same recursion depth. Never returns spuriously. Results: */
enum {
    JABS_WAIT_NOT_OWNER = 0,   /* nothing happened: IllegalMonitorStateException */
    JABS_WAIT_NOTIFIED = 1,    /* woken by notify/notifyAll */
    JABS_WAIT_TIMED_OUT = 2,   /* the time passed */
    JABS_WAIT_INTERRUPTED = 3  /* an interrupt was pending or arrived (and is now consumed):
                                  InterruptedException. A thread notified and interrupted at once
                                  returns NOTIFIED and keeps the interrupt pending (JLS 17.2.4). */
};
int jabs_monitor_wait(jabs_object *o, int64_t millis);
/* The calling OS thread as the monitors know it: created on first use, never freed, so the
 * pointer stays valid after the thread ends. */
typedef struct jabs_sync_thread jabs_sync_thread;
jabs_sync_thread *jabs_sync_current(void);
/* The interrupt hook for the threads code. jabs_sync_interrupt marks the thread interrupted and
 * wakes it if it is in jabs_monitor_wait (which then returns JABS_WAIT_INTERRUPTED); if it is
 * not waiting, its next wait returns that at once. The mark is consumed by that wait or by
 * jabs_sync_clear_interrupt, which returns whether it was set. Thread.interrupt should call the
 * first after setting the Java `interrupted` field, and clearing that field the second (as
 * Thread.clearInterruptEvent). Any thread may call either for any record. */
void jabs_sync_interrupt(jabs_sync_thread *t);
bool jabs_sync_clear_interrupt(jabs_sync_thread *t);
/* Optional, for the threads code: the calling thread is ending and holds no monitor level (it
 * stops the program otherwise). Its small id, which lock words use to name owners, may then go
 * to a new thread; its record stays allocated. Without this call ids are never reused and at
 * most 2^20 - 1 threads can ever touch monitors. */
void jabs_sync_thread_end(void);
/* For tests and diagnostics: monitors inflated so far (thin locks have none). */
uint32_t jabs_sync_inflated_monitors(void);

/* invokeinterface: binary search the receiver's interface table; NULL if the class does not
 * implement a method with this selector (IncompatibleClassChangeError). */
static inline void (*jabs_itable_find(const jabs_object *o, uint64_t selector))(void) {
    const jabs_itable_entry *t = o->cls->itable;
    uint32_t lo = 0, hi = o->cls->itable_len;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (t[mid].selector < selector) lo = mid + 1;
        else hi = mid;
    }
    return (lo < o->cls->itable_len && t[lo].selector == selector) ? t[lo].fn : NULL;
}

static inline jabs_object *jabs_new(const jabs_class *cls) {
    jabs_object *o = (jabs_object *)calloc(1, cls->size);
    if (o) atomic_fetch_add_explicit(&jabs_heap_bytes, (int64_t)cls->size, memory_order_relaxed);
    if (!o) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }
    o->cls = cls;
    return o;
}

/* ---- Native methods implemented by the runtime (JVM intrinsics) ---- */

/* Identity hash: stable per object, never 0, distinct for consecutive requests on one thread.
 * Each thread has its own xorshift state (as HotSpot's default hashCode mode does), so threads
 * never contend on it; the first value stored in the object wins if two threads race. */
extern _Thread_local uint32_t jabs_hash_next;
static inline int32_t jabs_identity_hash(jabs_object *o) {
    uint32_t hash = atomic_load_explicit(&o->hash, memory_order_relaxed);
    if (hash != 0) return (int32_t)hash;
    jabs_hash_next ^= jabs_hash_next << 13;
    jabs_hash_next ^= jabs_hash_next >> 17;
    jabs_hash_next ^= jabs_hash_next << 5;
    uint32_t mine = jabs_hash_next ? (jabs_hash_next & 0x7fffffffu) | 1u : 1u;
    uint32_t expected = 0;
    if (atomic_compare_exchange_strong_explicit(&o->hash, &expected, mine, memory_order_relaxed, memory_order_relaxed)) return (int32_t)mine;
    return (int32_t)expected;
}

/* Size in bytes of one element of an array, from its descriptor. */
static inline size_t jabs_element_size(const jabs_array *a) {
    switch (a->descriptor[1]) {
    case 'Z': case 'B': return 1;
    case 'C': case 'S': return 2;
    case 'I': case 'F': return 4;
    case 'J': case 'D': return 8;
    default: return sizeof(jabs_object *);
    }
}

/* System.arraycopy with HotSpot's checks in its order: null (NullPointerException), array types
 * (ArrayStoreException), bounds (ArrayIndexOutOfBoundsException), then per-element store checks
 * for reference arrays whose element types differ, which copy up to the first bad element. */
static inline void jabs_arraycopy(jabs_object *src, int32_t src_pos, jabs_object *dst, int32_t dst_pos, int32_t length) {
    if (!src || !dst) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    if (src->cls != jabs_array_class() || dst->cls != jabs_array_class()) {
        jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_STORE);
        return;
    }
    jabs_array *s = (jabs_array *)src, *d = (jabs_array *)dst;
    int s_ref = s->descriptor[1] == 'L' || s->descriptor[1] == '[';
    int d_ref = d->descriptor[1] == 'L' || d->descriptor[1] == '[';
    if (s_ref != d_ref || (!s_ref && s->descriptor[1] != d->descriptor[1])) {
        jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_STORE);
        return;
    }
    if (src_pos < 0 || dst_pos < 0 || length < 0 || (int64_t)src_pos + length > s->length ||
        (int64_t)dst_pos + length > d->length) {
        jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX);
        return;
    }
    size_t size = jabs_element_size(s);
    char *from = (char *)jabs_array_data(s) + (size_t)src_pos * size;
    char *to = (char *)jabs_array_data(d) + (size_t)dst_pos * size;
    if (!s_ref || strcmp(s->descriptor, d->descriptor) == 0 || d->component_kind == JABS_COMPONENT_ANY) {
        memmove(to, from, (size_t)length * size);
        return;
    }
    /* Different reference element types: check each element (source and destination differ,
     * since their descriptors do, so copying forward is safe). */
    jabs_object **in = (jabs_object **)from, **out = (jabs_object **)to;
    for (int32_t i = 0; i < length; i++) {
        if (in[i] && !jabs_array_store_ok(d, in[i])) {
            jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_STORE);
            return;
        }
        out[i] = in[i];
    }
}

/* Object.clone: arrays and Cloneable objects get a shallow copy with a fresh monitor and hash. */
static inline jabs_object *jabs_clone(jabs_object *o) {
    size_t size;
    if (o->cls == jabs_array_class()) {
        jabs_array *a = (jabs_array *)o;
        size = ((sizeof(jabs_array) + 7) & ~(size_t)7) + (size_t)a->length * jabs_element_size(a);
    } else {
        int cloneable = 0;
        for (uint32_t i = 0; i < o->cls->interfaces_len; i++) {
            if (strcmp(o->cls->interfaces[i]->name, "java/lang/Cloneable") == 0) cloneable = 1;
        }
        if (!cloneable) { jabs_pending = jabs_vm_throwable(JABS_VM_CLONE_NOT_SUPPORTED); return NULL; }
        size = o->cls->size;
    }
    jabs_object *copy = (jabs_object *)malloc(size);
    if (copy) atomic_fetch_add_explicit(&jabs_heap_bytes, (int64_t)size, memory_order_relaxed);
    if (!copy) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    memcpy(copy, o, size);
    copy->monitor = 0;
    atomic_store_explicit(&copy->hash, 0, memory_order_relaxed);
    return copy;
}

/* Wall time for currentTimeMillis; nanoTime uses the host's monotonic clock. */
static inline int64_t jabs_current_millis(void) {
    struct timespec t;
    timespec_get(&t, TIME_UTC);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
int64_t jabs_nano_time(void);

#endif
