/* JABS runtime prelude: Java primitive semantics in portable C11.
 *
 * Requirements: two's-complement integers, IEEE-754 binary32/binary64 with
 * FLT_EVAL_METHOD == 0 (SSE2 on x86-64, any AArch64), and no -ffast-math.
 * Signed results are produced through unsigned arithmetic so overflow wraps
 * as Java requires instead of being undefined behaviour in C.
 */
#ifndef JABS_RT_H
#define JABS_RT_H

#include <float.h>
#include <math.h>
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
    /* Monitor entry count. The runtime has a single thread, so the owner is always that thread. */
    uint32_t monitor;
    /* Identity hash code, assigned on first request (0 = not yet assigned). */
    uint32_t hash;
} jabs_object;

typedef union jabs_value {
    int32_t i;
    int64_t j;
    float f;
    double d;
    jabs_object *a;
} jabs_value;

/* Internal class name of the pending exception, or NULL. */
/* The exception being thrown (a Throwable object), or NULL. */
extern _Thread_local jabs_object *jabs_pending;
/* The method whose code last raised a VM exception (a breadcrumb until stack traces exist). */
extern const char *jabs_where;

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
    JABS_VM_SYNC_FAILED
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
/* Bytes of Java objects allocated so far. With no garbage collector the heap only grows, so this
 * is Runtime.totalMemory (and freeMemory is 0). */
extern int64_t jabs_heap_bytes;
/* Class (internal name) that called the @CallerSensitive method now running; see
 * Reflection.getCallerClass. Set at each call site of such a method. */
extern _Thread_local const char *jabs_caller;

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

/* Reached a method that has no native code yet: stop with a diagnostic rather than guess.
 * The profile must run on the JVM backend until this method compiles. */
static inline _Noreturn void jabs_not_compiled(const char *method) {
    fprintf(stderr, "jabs: %s is not compiled to native code; run this profile on the JVM backend\n", method);
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

/* jabs_not_compiled naming the class involved. */
static inline _Noreturn void jabs_not_compiled_class(const char *what, const char *class_name) {
    fprintf(stderr, "jabs: %s (%s) is not compiled to native code; run this profile on the JVM backend\n", what, class_name);
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
 * shape: 0 static field (address), 1 instance field (offset from the object), 2 array element. */
typedef struct { char kind; uint8_t shape; int32_t scale; int64_t offset; void *address; } jabs_vh;
/* The handle data of a runtime-made VarHandle; NULL for any other object. */
const jabs_vh *jabs_vh_of(jabs_object *o);
/* The accessed address; NULL with NullPointerException/ArrayIndexOutOfBounds pending. */
void *jabs_vh_address(const jabs_vh *h, jabs_object *o, int32_t index);
int32_t jabs_array_scale(jabs_object *array_class);

/* Generated glue the java.io natives use (see io_accessor_functions in cgen.rs). */
jabs_object *jabs_file_path_of(jabs_object *file);
jabs_object **jabs_stream_fd_slot(jabs_object *stream);
int32_t *jabs_descriptor_fd_slot(jabs_object *descriptor);
void jabs_descriptor_set_handle(jabs_object *descriptor, int64_t handle);
void jabs_descriptor_set_append(jabs_object *descriptor, int32_t append);
jabs_object *jabs_new_string_array(int32_t length);
void jabs_raise_message(int kind, const char *utf8);
char *jabs_string_to_utf8(jabs_object *s);
jabs_object *jabs_string_from_utf8(const char *text);
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
void jabs_io_cleanup_close(int32_t fd);
void jabs_io_sync(jabs_object *fdo);

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

/* Shared record for every array object: arrays are instances of no compiled class.
 * A function-local static avoids unused-variable warnings in programs without arrays. */
extern const jabs_class jabs_array_class_record;
static inline const jabs_class *jabs_array_class(void) { return &jabs_array_class_record; }

static inline int32_t jabs_is_instance(const jabs_object *o, const jabs_class *target);

/* aastore check: may `value` (non-null) be stored in this array? */
static inline int jabs_array_store_ok(jabs_array *a, jabs_object *value) {
    switch (a->component_kind) {
    case JABS_COMPONENT_ANY: return 1;
    case JABS_COMPONENT_CLASS: return jabs_is_instance(value, a->component);
    case JABS_COMPONENT_ARRAY:
        return value->cls == jabs_array_class() &&
               strcmp(((jabs_array *)value)->descriptor, a->component_descriptor) == 0;
    default: jabs_not_compiled("aastore into an array whose element type is outside the unit or an interface");
    }
    return 0;
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
    if (a) jabs_heap_bytes += (int64_t)(header + (size_t)length * element_size);
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

/* monitorenter: with a single thread the monitor is always free or already owned. */
static inline void jabs_monitor_enter(jabs_object *o) { o->monitor++; }

/* monitorexit: 0 means the thread does not own the monitor (IllegalMonitorStateException). */
static inline int jabs_monitor_exit(jabs_object *o) {
    if (o->monitor == 0) return 0;
    o->monitor--;
    return 1;
}

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
    if (o) jabs_heap_bytes += (int64_t)cls->size;
    if (!o) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }
    o->cls = cls;
    return o;
}

/* ---- Native methods implemented by the runtime (JVM intrinsics) ---- */

/* Identity hash: stable per object, never 0, distinct for consecutive requests. */
extern uint32_t jabs_hash_next;
static inline int32_t jabs_identity_hash(jabs_object *o) {
    if (o->hash == 0) {
        jabs_hash_next ^= jabs_hash_next << 13;
        jabs_hash_next ^= jabs_hash_next >> 17;
        jabs_hash_next ^= jabs_hash_next << 5;
        o->hash = jabs_hash_next ? (jabs_hash_next & 0x7fffffffu) | 1u : 1u;
    }
    return (int32_t)o->hash;
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
    if (copy) jabs_heap_bytes += (int64_t)size;
    if (!copy) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    memcpy(copy, o, size);
    copy->monitor = 0;
    copy->hash = 0;
    return copy;
}

/* System.currentTimeMillis and System.nanoTime from the C11 clock (nanoTime is only meaningful
 * as a difference, as in Java). */
static inline int64_t jabs_current_millis(void) {
    struct timespec t;
    timespec_get(&t, TIME_UTC);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static inline int64_t jabs_nano_time(void) {
    struct timespec t;
    timespec_get(&t, TIME_UTC);
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

#endif
