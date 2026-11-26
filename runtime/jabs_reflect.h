/* Reflection: the metadata java.lang.reflect needs, and the runtime's entry points for it.
 *
 * The tables are generated per program (reflection_metadata in src/cgen.rs) for every class
 * that has a runtime record, and are read-only: nothing here is built or changed at run time,
 * so any thread may read them. runtime/jabs_reflect.c holds the logic; the generated
 * functions below give it what depends on the program (object layouts, records, calls into
 * compiled Java methods). Programs without a class registry have none of this and do not
 * define JABS_HAVE_REFLECTION.
 *
 * Encoding (kept compact: the Minecraft server has tens of thousands of members):
 *  - strings are byte offsets into jabs_rstrings (UTF-8, NUL-terminated); 0 means absent and
 *    1 is the empty string;
 *  - raw attribute bytes are offsets into jabs_rbytes, where a 4-byte big-endian length
 *    precedes the bytes; 0 means the attribute is absent;
 *  - lists are offsets into jabs_rlists: a count, then that many string offsets; 0 is empty.
 */
#ifndef JABS_REFLECT_H
#define JABS_REFLECT_H

/* A declared method or constructor, in class-file order (its index is Method.slot). */
typedef struct jabs_rmethod {
    uint32_t name, descriptor, signature;
    uint32_t exceptions; /* list of internal class names (the Exceptions attribute) */
    uint32_t annotations, parameter_annotations, annotation_default;
    uint16_t access;
    uint16_t caller;     /* index into jabs_rcallers, the C shape of the call */
    int32_t vtable_slot; /* >= 0: Method.invoke dispatches through the receiver's vtable */
    uint64_t selector;   /* != 0: dispatches through the receiver's interface table */
    void (*code)(void);  /* the compiled body for a direct call; NULL if not in this program */
} jabs_rmethod;

/* A declared field, in class-file order (its index is Field.slot). */
typedef struct jabs_rfield {
    uint32_t name, descriptor, signature, annotations;
    uint16_t access;
    size_t offset; /* instance fields: byte offset in the object */
    void *address; /* static fields: storage, NULL if the class's statics are not in this program */
} jabs_rfield;

/* An InnerClasses entry (strings: internal names, the simple name; 0 where the class file has 0). */
typedef struct jabs_rinner {
    uint32_t inner, outer, simple_name;
    uint16_t access;
} jabs_rinner;

/* A constant-pool entry that an annotation attribute names (only those are kept). `tag` is the
 * JVMS tag (1 Utf8: value is a string offset; 3 Integer, 4 Float: 32 raw bits; 5 Long, 6
 * Double: 64 raw bits), or 0 for an entry the binary cannot represent. */
typedef struct jabs_rconst {
    uint16_t index;
    uint8_t tag;
    uint64_t value;
} jabs_rconst;

typedef struct jabs_rclass {
    uint32_t name; /* internal name; the table is sorted by it */
    uint32_t methods, method_count; /* range of jabs_rmethods */
    uint32_t fields, field_count;   /* range of jabs_rfields */
    uint32_t interfaces;            /* list: direct superinterfaces, in class-file order */
    uint32_t signature, annotations;
    uint32_t inner, inner_count;    /* range of jabs_rinners */
    uint32_t enclosing_class, enclosing_name, enclosing_descriptor; /* EnclosingMethod */
    uint32_t nest_host;             /* 0: the class is its own nest host */
    uint32_t pool, pool_count;      /* range of jabs_rconsts, sorted by index */
    uint16_t pool_size;             /* constant_pool_count, for ConstantPool.getSize */
    uint16_t access;                /* class-file access flags */
    uint16_t modifiers;             /* Class.getModifiers, as HotSpot computes it */
    uint8_t is_record;
} jabs_rclass;

/* A class's NestHost attribute (both internal names), for every class in the unit that has one. */
typedef struct jabs_rnest {
    uint32_t class_name, host;
} jabs_rnest;

/* A proxy class generated at build time (src/proxy.rs): its internal name and its interface
 * list (internal names, in the order Proxy.newProxyInstance is given them). */
typedef struct jabs_rproxy {
    uint32_t name, interfaces;
} jabs_rproxy;

/* Calls a compiled method of one C shape: `fn` with the receiver (ignored for static shapes)
 * and the arguments, storing the result. */
typedef void (*jabs_rcaller)(void (*fn)(void), jabs_object *self, const jabs_value *args, jabs_value *result);

extern const char jabs_rstrings[];
extern const unsigned char jabs_rbytes[];
extern const uint32_t jabs_rlists[];
extern const jabs_rclass jabs_rclasses[];
extern const uint32_t jabs_rclass_count;
extern const jabs_rmethod jabs_rmethods[];
extern const jabs_rfield jabs_rfields[];
extern const jabs_rinner jabs_rinners[];
extern const jabs_rconst jabs_rconsts[];
extern const jabs_rnest jabs_rnests[];
extern const uint32_t jabs_rnest_count;
extern const jabs_rcaller jabs_rcallers[];
extern const jabs_rproxy jabs_rproxies[];
extern const uint32_t jabs_rproxy_count;
/* One per jabs_rproxies row: the proxy's Constructor, made on first use. The only table written
 * at run time: each slot changes once, from NULL, by compare-and-swap. */
extern jabs_object *jabs_rproxy_constructors[];
extern const uint32_t jabs_rcaller_count;

/* Value kinds in descriptor order of the boxes and accessors below: Z B C S I J F D, then L. */
enum { JABS_RK_Z, JABS_RK_B, JABS_RK_C, JABS_RK_S, JABS_RK_I, JABS_RK_J, JABS_RK_F, JABS_RK_D, JABS_RK_L, JABS_RK_COUNT };

/* Classes the runtime allocates instances of. */
enum {
    JABS_RR_METHOD, JABS_RR_FIELD, JABS_RR_CONSTRUCTOR, JABS_RR_CONSTANT_POOL,
    JABS_RR_IAE,                         /* java.lang.IllegalArgumentException */
    JABS_RR_ITE,                         /* java.lang.reflect.InvocationTargetException */
    JABS_RR_BOX,                         /* + JABS_RK_Z..JABS_RK_D: Boolean .. Double */
    JABS_RR_FIELD_ACCESSOR = JABS_RR_BOX + 8, /* + JABS_RK_*: MethodHandle<T>FieldAccessorImpl */
    JABS_RR_COUNT = JABS_RR_FIELD_ACCESSOR + JABS_RK_COUNT
};

/* Fields the runtime reads or writes. */
enum {
    JABS_RF_METHOD_CLAZZ, JABS_RF_METHOD_SLOT, JABS_RF_METHOD_NAME, JABS_RF_METHOD_RETURN_TYPE,
    JABS_RF_METHOD_PARAMETER_TYPES, JABS_RF_METHOD_EXCEPTION_TYPES, JABS_RF_METHOD_MODIFIERS,
    JABS_RF_METHOD_SIGNATURE, JABS_RF_METHOD_ANNOTATIONS, JABS_RF_METHOD_PARAMETER_ANNOTATIONS,
    JABS_RF_METHOD_ANNOTATION_DEFAULT,
    JABS_RF_CTOR_CLAZZ, JABS_RF_CTOR_SLOT, JABS_RF_CTOR_PARAMETER_TYPES, JABS_RF_CTOR_EXCEPTION_TYPES,
    JABS_RF_CTOR_MODIFIERS, JABS_RF_CTOR_SIGNATURE, JABS_RF_CTOR_ANNOTATIONS,
    JABS_RF_CTOR_PARAMETER_ANNOTATIONS,
    JABS_RF_FIELD_CLAZZ, JABS_RF_FIELD_SLOT, JABS_RF_FIELD_NAME, JABS_RF_FIELD_TYPE,
    JABS_RF_FIELD_MODIFIERS, JABS_RF_FIELD_TRUSTED_FINAL, JABS_RF_FIELD_SIGNATURE,
    JABS_RF_FIELD_ANNOTATIONS,
    JABS_RF_CONSTANT_POOL_OOP,
    JABS_RF_ACCESSOR_FIELD,              /* FieldAccessorImpl.field */
    JABS_RF_OVERRIDE,                    /* AccessibleObject.override (a boolean) */
    JABS_RF_BOX_VALUE,                   /* + JABS_RK_Z..JABS_RK_D: Boolean.value .. Double.value */
    JABS_RF_COUNT = JABS_RF_BOX_VALUE + 8
};

/* Compiled Java methods the runtime calls. */
enum {
    JABS_RJ_VALUE_OF,                    /* + JABS_RK_Z..JABS_RK_D: Boolean.valueOf .. Double.valueOf */
    JABS_RJ_IAE_MESSAGE = JABS_RJ_VALUE_OF + 8, /* IllegalArgumentException(String) on args[0] */
    JABS_RJ_IAE_CAUSE,                   /* IllegalArgumentException(Throwable) */
    JABS_RJ_ITE,                         /* InvocationTargetException(Throwable) */
    JABS_RJ_ACCESSOR_INIT,               /* + JABS_RK_*: <T>FieldAccessorImpl(Field, MH, MH, boolean, boolean) */
    JABS_RJ_ENSURE_OBJ = JABS_RJ_ACCESSOR_INIT + JABS_RK_COUNT, /* MethodHandleFieldAccessorImpl.ensureObj */
    JABS_RJ_IS_READ_ONLY,
    JABS_RJ_NEW_GET_IAE,                 /* newGetIllegalArgumentException(Object) */
    JABS_RJ_NEW_SET_IAE,                 /* newSetIllegalArgumentException(Object) */
    JABS_RJ_THROW_SET_IAE,               /* throwSetIllegalArgumentException(Object) */
    JABS_RJ_THROW_FINAL,                 /* + JABS_RK_*: throwFinalFieldIllegalAccessException(T) */
    JABS_RJ_COUNT = JABS_RJ_THROW_FINAL + JABS_RK_COUNT
};

/* ---- Generated per program (reflection_support in src/cgen.rs) ---- */
/* The record of a JABS_RR_* class, or NULL when the program has none. */
const jabs_class *jabs_rrecord(int which);
/* A new zeroed instance of a JABS_RR_* class (its initialization run first); NULL with
 * jabs_pending set. Stops if the class is not in the program. */
jabs_object *jabs_rnew(int which);
/* Address of a JABS_RF_* field in `o`; stops if this JDK's layout has no such field. */
void *jabs_rf(jabs_object *o, int which);
/* Call a JABS_RJ_* method (self is ignored for static ones); 0, or -1 with jabs_pending set.
 * Stops if the method is not compiled into the program. */
int jabs_rcall(int which, jabs_object *self, const jabs_value *args, jabs_value *result);
/* Internal name ("a/B"), array descriptor or primitive name of a Class object; NULL if it is
 * not one the registry made. */
const char *jabs_reflect_class_name(jabs_object *class_object);
/* The runtime record of a Class object's class, or NULL (primitives, arrays, no record). */
const jabs_class *jabs_reflect_class_record(jabs_object *class_object);

/* ---- runtime/jabs_reflect.c ---- */
const jabs_rclass *jabs_reflect_info_named(const char *internal_name);
int32_t jabs_reflect_class_modifiers(const char *internal_name, uint16_t access);
jabs_object *jabs_reflect_declared_methods(jabs_object *c, int32_t public_only);
jabs_object *jabs_reflect_declared_constructors(jabs_object *c, int32_t public_only);
jabs_object *jabs_reflect_declared_fields(jabs_object *c, int32_t public_only);
jabs_object *jabs_reflect_interfaces(jabs_object *c);
jabs_object *jabs_reflect_declared_classes(jabs_object *c);
jabs_object *jabs_reflect_declaring_class(jabs_object *c);
jabs_object *jabs_reflect_enclosing_method(jabs_object *c);
jabs_object *jabs_reflect_simple_binary_name(jabs_object *c);
jabs_object *jabs_reflect_generic_signature(jabs_object *c);
jabs_object *jabs_reflect_raw_annotations(jabs_object *c);
int32_t jabs_reflect_is_record(jabs_object *c);
jabs_object *jabs_reflect_nest_host(jabs_object *c);
int32_t jabs_reflect_nest_mates(jabs_object *a, jabs_object *b);
jabs_object *jabs_reflect_constant_pool(jabs_object *c);
int32_t jabs_reflect_cp_size(jabs_object *oop);
int32_t jabs_reflect_cp_int(jabs_object *oop, int32_t index);
int64_t jabs_reflect_cp_long(jabs_object *oop, int32_t index);
float jabs_reflect_cp_float(jabs_object *oop, int32_t index);
double jabs_reflect_cp_double(jabs_object *oop, int32_t index);
jabs_object *jabs_reflect_cp_utf8(jabs_object *oop, int32_t index);
jabs_object *jabs_reflect_invoke(jabs_object *method, jabs_object *receiver, jabs_object *args);
jabs_object *jabs_reflect_new_instance(jabs_object *constructor, jabs_object *args);
jabs_object *jabs_reflect_new_field_accessor(jabs_object *field, int32_t read_only);
/* Proxy.getProxyConstructor and ProxyBuilder.isProxyClass, answered from jabs_rproxies. */
jabs_object *jabs_reflect_proxy_constructor(jabs_object *loader, jabs_object *interfaces);
int32_t jabs_reflect_is_proxy_class(jabs_object *c);
/* Field accessor leaves (the JDK's MethodHandle<T>FieldAccessorImpl get/set of its own type). */
void *jabs_reflect_field_slot(jabs_object *accessor, jabs_object *o, int32_t for_set, int32_t *is_volatile);
jabs_object *jabs_reflect_static_field_base(jabs_object *field);
int64_t jabs_reflect_static_field_offset(jabs_object *field);
int32_t jabs_reflect_field_refuse_set(jabs_object *accessor, jabs_object *o, int kind, jabs_value value);
void jabs_reflect_field_set_object(jabs_object *accessor, jabs_object *o, jabs_object *value);

#endif
