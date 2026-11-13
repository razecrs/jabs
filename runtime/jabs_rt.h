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
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if FLT_EVAL_METHOD != 0
#error "JABS requires FLT_EVAL_METHOD == 0 for Java floating-point semantics"
#endif

/* A class's runtime record: name, instance size, vtable (one slot per overridable method)
 * and superclass record (NULL directly below java/lang/Object). */
typedef struct jabs_class {
    const char *name;
    size_t size;
    void (*const *vtable)(void);
    const struct jabs_class *super;
} jabs_class;

/* Every object starts with this header; the class's own struct embeds it first. */
typedef struct jabs_object {
    const jabs_class *cls;
} jabs_object;

typedef union jabs_value {
    int32_t i;
    int64_t j;
    float f;
    double d;
    jabs_object *a;
} jabs_value;

/* Internal class name of the pending exception, or NULL. */
static _Thread_local const char *jabs_pending;

/* Allocate a zeroed object: all-zero bytes are every field's Java default (0, 0.0, false, null).
 * There is no garbage collector yet, so objects live until the process exits. */
static inline jabs_object *jabs_new(const jabs_class *cls);

/* Exception a failed static initializer reports (JVMS 5.5 step 11): Errors pass through,
 * anything else becomes ExceptionInInitializerError. Until the class hierarchy is modeled,
 * the Error subclasses listed are the ones this runtime itself can raise. */
static inline const char *jabs_initializer_failure(const char *thrown) {
    static const char *const errors[] = {
        "java/lang/ExceptionInInitializerError", "java/lang/NoClassDefFoundError",
        "java/lang/StackOverflowError", "java/lang/OutOfMemoryError",
    };
    for (size_t i = 0; i < sizeof errors / sizeof errors[0]; i++) {
        if (strcmp(thrown, errors[i]) == 0) return thrown;
    }
    return "java/lang/ExceptionInInitializerError";
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
static inline void jabs_not_compiled(const char *method) {
    fprintf(stderr, "jabs: %s is not compiled to native code; run this profile on the JVM backend\n", method);
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

/* Elements start right after the header, aligned for any element type. */
static inline void *jabs_array_data(jabs_array *a) {
    return (char *)a + ((sizeof(jabs_array) + 7) & ~(size_t)7);
}

/* Shared record for every array object: arrays are instances of no compiled class.
 * A function-local static avoids unused-variable warnings in programs without arrays. */
static inline const jabs_class *jabs_array_class(void) {
    static const jabs_class record = { "[", 0, NULL, NULL };
    return &record;
}

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
        jabs_pending = "java/lang/OutOfMemoryError";
        return NULL;
    }
    jabs_array *a = (jabs_array *)calloc(1, header + (size_t)length * element_size);
    if (!a) {
        jabs_pending = "java/lang/OutOfMemoryError";
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

/* instanceof for a class target: walk the object's superclass chain. */
static inline int32_t jabs_is_instance(const jabs_object *o, const jabs_class *target) {
    for (const jabs_class *c = o->cls; c; c = c->super) {
        if (c == target) return 1;
    }
    return 0;
}

static inline jabs_object *jabs_new(const jabs_class *cls) {
    jabs_object *o = (jabs_object *)calloc(1, cls->size);
    if (!o) {
        jabs_pending = "java/lang/OutOfMemoryError";
        return NULL;
    }
    o->cls = cls;
    return o;
}

#endif
