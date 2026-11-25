/* Reflection the JVM provides: member listings (getDeclaredMethods0 and friends), the class
 * attributes java.lang.Class asks the VM for, constant pools for annotation parsing, and
 * calling or accessing members by reflection. Handwritten; it reads the generated, read-only
 * tables and helpers declared in runtime/jabs_reflect.h, so nothing here depends on one JDK's
 * object layout. Behavior follows JDK 25 on HotSpot as the reference JVM runs it; where that
 * differs between HotSpot's native reflection and the JDK's method-handle accessors, the
 * comment says which one is followed. */
#include "jabs_unit.h"

#ifdef JABS_HAVE_REFLECTION

/* HotSpot's JVM_RECOGNIZED_METHOD_MODIFIERS and JVM_RECOGNIZED_FIELD_MODIFIERS: what
 * Method/Constructor/Field.getModifiers report of the class-file access flags. */
#define JABS_METHOD_MODIFIERS 0x1DFF
#define JABS_FIELD_MODIFIERS 0x50DF
/* Class modifiers of a class without metadata: recognized class flags minus ACC_SUPER. */
#define JABS_CLASS_MODIFIERS 0x7611
#define JABS_ACC_PUBLIC 0x0001
#define JABS_ACC_STATIC 0x0008
#define JABS_ACC_FINAL 0x0010
#define JABS_ACC_VOLATILE 0x0040
#define JABS_ACC_INTERFACE 0x0200
#define JABS_ACC_ABSTRACT 0x0400
/* JVMS 4.3.3: a method descriptor has at most 255 parameter slots. */
#define JABS_MAX_PARAMETERS 255
/* Longest class name or descriptor reflection builds a Class from (JVMS allows 65535 bytes;
 * nothing real comes close, and a longer one stops rather than truncate). */
#define JABS_MAX_NAME 4096

static const char *rstring(uint32_t at) { return at ? jabs_rstrings + at : NULL; }

static int compare_class(const void *key, const void *entry) {
    return strcmp((const char *)key, jabs_rstrings + ((const jabs_rclass *)entry)->name);
}

const jabs_rclass *jabs_reflect_info_named(const char *name) {
    if (!name || jabs_rclass_count == 0) return NULL;
    return (const jabs_rclass *)bsearch(name, jabs_rclasses, jabs_rclass_count, sizeof *jabs_rclasses, compare_class);
}

int32_t jabs_reflect_class_modifiers(const char *name, uint16_t access) {
    const jabs_rclass *k = jabs_reflect_info_named(name);
    return k ? k->modifiers : (int32_t)(access & JABS_CLASS_MODIFIERS);
}

static int is_primitive_name(const char *n) {
    static const char *const names[] = { "boolean", "byte", "char", "short", "int", "long", "float", "double", "void" };
    for (size_t i = 0; i < sizeof names / sizeof names[0]; i++) {
        if (strcmp(n, names[i]) == 0) return 1;
    }
    return 0;
}

/* The internal name of a Class object, stopping on one the registry did not make. */
static const char *name_of(jabs_object *c, const char *what) {
    const char *name = jabs_reflect_class_name(c);
    if (!name) jabs_not_compiled(what);
    return name;
}

/* Metadata of a class that must have it: every class with a record has metadata, and only
 * such classes can be asked about their members. */
static const jabs_rclass *info_of(const char *name, const char *what) {
    const jabs_rclass *k = jabs_reflect_info_named(name);
    if (!k) jabs_not_compiled_class(what, name);
    return k;
}

static uint32_t read_be32(const unsigned char *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | (uint32_t)p[3];
}

/* ---- Building Java values ---- */

static jabs_object *new_string(uint32_t at) { return at ? jabs_string_from_utf8(jabs_rstrings + at) : NULL; }

/* HotSpot interns the names of Method and Field objects. */
static jabs_object *interned_string(uint32_t at) {
    jabs_object *s = new_string(at);
    return s ? jabs_string_intern(s) : NULL;
}

/* A fresh byte[] with an attribute's raw bytes (HotSpot copies them for every object), or
 * NULL when the attribute is absent. `*failed` is set when allocation failed. */
static jabs_object *blob_array(uint32_t at, int *failed) {
    if (!at) return NULL;
    uint32_t length = read_be32(jabs_rbytes + at);
    if (length > INT32_MAX) jabs_not_compiled("reflection attribute longer than a Java array");
    jabs_object *a = jabs_new_array((int32_t)length, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!a) { *failed = 1; return NULL; }
    memcpy(jabs_array_data((jabs_array *)a), jabs_rbytes + at + 4, length);
    return a;
}

static jabs_object **elements(jabs_object *array) { return (jabs_object **)jabs_array_data((jabs_array *)array); }

/* A Class[] of `length` (its record is the record every Class object has). */
static jabs_object *class_array(jabs_object *any_class, int32_t length) {
    return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_CLASS, any_class->cls, "[Ljava/lang/Class;", NULL);
}

static jabs_object *record_array(int record, const char *descriptor, int32_t length) {
    const jabs_class *r = jabs_rrecord(record);
    if (!r) jabs_not_compiled_class("reflection array of a class without a record", descriptor);
    return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_CLASS, r, descriptor, NULL);
}

/* Length of the field descriptor at `d` (one element of a method descriptor), or 0. */
static size_t descriptor_element(const char *d) {
    size_t i = 0;
    while (d[i] == '[' && i < 255) i++; /* JVMS 4.4.1: at most 255 dimensions */
    if (d[i] == 'L') {
        const char *semi = strchr(d + i, ';');
        return semi ? (size_t)(semi - d) + 1 : 0;
    }
    if (d[i] != 0 && strchr("ZBCSIJFD", d[i])) return i + 1;
    return 0;
}

/* A method descriptor split into its parameters and return type. */
typedef struct {
    int count;
    const char *at[JABS_MAX_PARAMETERS];
    size_t length[JABS_MAX_PARAMETERS];
    const char *ret;
    size_t ret_length;
} parsed_descriptor;

static void parse_descriptor(const char *d, parsed_descriptor *p) {
    if (!d || d[0] != '(') jabs_not_compiled("reflection: malformed method descriptor");
    const char *at = d + 1;
    p->count = 0;
    while (*at != ')') {
        size_t n = descriptor_element(at);
        if (n == 0 || p->count == JABS_MAX_PARAMETERS) jabs_not_compiled("reflection: malformed method descriptor");
        p->at[p->count] = at;
        p->length[p->count] = n;
        p->count++;
        at += n;
    }
    p->ret = at + 1;
    p->ret_length = strcmp(p->ret, "V") == 0 ? 1 : descriptor_element(p->ret);
    if (p->ret_length == 0 || p->ret[p->ret_length] != 0) jabs_not_compiled("reflection: malformed method descriptor");
}

/* The Class of a field descriptor ("I" -> int, "Lx/Y;" -> x/Y, "[I" -> [I; "V" -> void). */
static jabs_object *descriptor_class(const char *d, size_t length) {
    static const char codes[] = "ZBCSIJFDV";
    static const char *const names[] = { "boolean", "byte", "char", "short", "int", "long", "float", "double", "void" };
    char name[JABS_MAX_NAME];
    if (length == 1) {
        const char *at = d[0] ? strchr(codes, d[0]) : NULL;
        if (!at) jabs_not_compiled("reflection: unknown primitive descriptor");
        return jabs_class_object(names[at - codes]);
    }
    if (d[0] == 'L') {
        if (length < 3 || d[length - 1] != ';' || length - 2 >= sizeof name) jabs_not_compiled("reflection: class descriptor too long");
        memcpy(name, d + 1, length - 2);
        name[length - 2] = 0;
    } else if (d[0] == '[') {
        if (length >= sizeof name) jabs_not_compiled("reflection: array descriptor too long");
        memcpy(name, d, length);
        name[length] = 0;
    } else {
        jabs_not_compiled("reflection: malformed field descriptor");
    }
    return jabs_class_object(name);
}

/* Class[] of a descriptor's parameter types. */
static jabs_object *parameter_classes(jabs_object *any_class, const parsed_descriptor *p) {
    jabs_object *array = class_array(any_class, p->count);
    if (!array) return NULL;
    for (int i = 0; i < p->count; i++) {
        jabs_object *c = descriptor_class(p->at[i], p->length[i]);
        if (!c) return NULL;
        elements(array)[i] = c;
    }
    return array;
}

/* Class[] of a list of internal class names (declared exceptions, interfaces). */
static jabs_object *named_classes(jabs_object *any_class, uint32_t list) {
    uint32_t count = list ? jabs_rlists[list] : 0;
    if (count > INT32_MAX) jabs_not_compiled("reflection: class list too long");
    jabs_object *array = class_array(any_class, (int32_t)count);
    if (!array) return NULL;
    for (uint32_t i = 0; i < count; i++) {
        jabs_object *c = jabs_class_object(jabs_rstrings + jabs_rlists[list + 1 + i]);
        if (!c) return NULL;
        elements(array)[i] = c;
    }
    return array;
}

static void set_ref(jabs_object *o, int field, jabs_object *value) { *(jabs_object **)jabs_rf(o, field) = value; }
static void set_int(jabs_object *o, int field, int32_t value) { *(int32_t *)jabs_rf(o, field) = value; }

/* ---- Exceptions reflection raises (constructed by their compiled constructors) ---- */

static void raise_iae(const char *message) {
    jabs_object *text = jabs_string_from_utf8(message);
    if (!text) return;
    jabs_object *e = jabs_rnew(JABS_RR_IAE);
    if (!e) return;
    jabs_value args[1], result;
    args[0].a = text;
    if (jabs_rcall(JABS_RJ_IAE_MESSAGE, e, args, &result) == 0) jabs_pending = e;
}

/* ---- Member objects, made as HotSpot's Reflection::new_method/new_constructor/new_field ---- */

static jabs_object *new_method(jabs_object *holder, const jabs_rclass *k, uint32_t slot) {
    const jabs_rmethod *m = &jabs_rmethods[k->methods + slot];
    parsed_descriptor p;
    parse_descriptor(rstring(m->descriptor), &p);
    int failed = 0;
    jabs_object *parameters = parameter_classes(holder, &p);
    jabs_object *ret = parameters ? descriptor_class(p.ret, p.ret_length) : NULL;
    jabs_object *exceptions = ret ? named_classes(holder, m->exceptions) : NULL;
    jabs_object *name = exceptions ? interned_string(m->name) : NULL;
    if (!name) return NULL;
    jabs_object *signature = new_string(m->signature);
    if (m->signature && !signature) return NULL;
    jabs_object *annotations = blob_array(m->annotations, &failed);
    jabs_object *parameter_annotations = blob_array(m->parameter_annotations, &failed);
    jabs_object *annotation_default = blob_array(m->annotation_default, &failed);
    if (failed) return NULL;
    jabs_object *o = jabs_rnew(JABS_RR_METHOD);
    if (!o) return NULL;
    set_ref(o, JABS_RF_METHOD_CLAZZ, holder);
    set_int(o, JABS_RF_METHOD_SLOT, (int32_t)slot);
    set_ref(o, JABS_RF_METHOD_NAME, name);
    set_ref(o, JABS_RF_METHOD_RETURN_TYPE, ret);
    set_ref(o, JABS_RF_METHOD_PARAMETER_TYPES, parameters);
    set_ref(o, JABS_RF_METHOD_EXCEPTION_TYPES, exceptions);
    set_int(o, JABS_RF_METHOD_MODIFIERS, m->access & JABS_METHOD_MODIFIERS);
    set_ref(o, JABS_RF_METHOD_SIGNATURE, signature);
    set_ref(o, JABS_RF_METHOD_ANNOTATIONS, annotations);
    set_ref(o, JABS_RF_METHOD_PARAMETER_ANNOTATIONS, parameter_annotations);
    set_ref(o, JABS_RF_METHOD_ANNOTATION_DEFAULT, annotation_default);
    return o;
}

static jabs_object *new_constructor(jabs_object *holder, const jabs_rclass *k, uint32_t slot) {
    const jabs_rmethod *m = &jabs_rmethods[k->methods + slot];
    parsed_descriptor p;
    parse_descriptor(rstring(m->descriptor), &p);
    int failed = 0;
    jabs_object *parameters = parameter_classes(holder, &p);
    jabs_object *exceptions = parameters ? named_classes(holder, m->exceptions) : NULL;
    if (!exceptions) return NULL;
    jabs_object *signature = new_string(m->signature);
    if (m->signature && !signature) return NULL;
    jabs_object *annotations = blob_array(m->annotations, &failed);
    jabs_object *parameter_annotations = blob_array(m->parameter_annotations, &failed);
    if (failed) return NULL;
    jabs_object *o = jabs_rnew(JABS_RR_CONSTRUCTOR);
    if (!o) return NULL;
    set_ref(o, JABS_RF_CTOR_CLAZZ, holder);
    set_int(o, JABS_RF_CTOR_SLOT, (int32_t)slot);
    set_ref(o, JABS_RF_CTOR_PARAMETER_TYPES, parameters);
    set_ref(o, JABS_RF_CTOR_EXCEPTION_TYPES, exceptions);
    set_int(o, JABS_RF_CTOR_MODIFIERS, m->access & JABS_METHOD_MODIFIERS);
    set_ref(o, JABS_RF_CTOR_SIGNATURE, signature);
    set_ref(o, JABS_RF_CTOR_ANNOTATIONS, annotations);
    set_ref(o, JABS_RF_CTOR_PARAMETER_ANNOTATIONS, parameter_annotations);
    return o;
}

static jabs_object *new_field(jabs_object *holder, const jabs_rclass *k, uint32_t slot) {
    const jabs_rfield *f = &jabs_rfields[k->fields + slot];
    const char *descriptor = rstring(f->descriptor);
    size_t length = descriptor ? descriptor_element(descriptor) : 0;
    if (length == 0 || descriptor[length] != 0) jabs_not_compiled("reflection: malformed field descriptor");
    int failed = 0;
    jabs_object *type = descriptor_class(descriptor, length);
    jabs_object *name = type ? interned_string(f->name) : NULL;
    if (!name) return NULL;
    jabs_object *signature = new_string(f->signature);
    if (f->signature && !signature) return NULL;
    jabs_object *annotations = blob_array(f->annotations, &failed);
    if (failed) return NULL;
    jabs_object *o = jabs_rnew(JABS_RR_FIELD);
    if (!o) return NULL;
    set_ref(o, JABS_RF_FIELD_CLAZZ, holder);
    set_int(o, JABS_RF_FIELD_SLOT, (int32_t)slot);
    set_ref(o, JABS_RF_FIELD_NAME, name);
    set_ref(o, JABS_RF_FIELD_TYPE, type);
    set_int(o, JABS_RF_FIELD_MODIFIERS, f->access & JABS_FIELD_MODIFIERS);
    /* fieldDescriptor::is_trusted_final: final, and static or in a record (or a hidden class;
     * reflection cannot reach JABS's lambda classes, the only hidden ones). */
    *(uint8_t *)jabs_rf(o, JABS_RF_FIELD_TRUSTED_FINAL) =
        (uint8_t)((f->access & JABS_ACC_FINAL) && ((f->access & JABS_ACC_STATIC) || k->is_record));
    set_ref(o, JABS_RF_FIELD_SIGNATURE, signature);
    set_ref(o, JABS_RF_FIELD_ANNOTATIONS, annotations);
    return o;
}

/* ---- Member listings (JVM_GetClassDeclaredMethods/Constructors/Fields) ---- */

/* Whether method `m` belongs in a methods (constructors = 0) or constructors listing. */
static int listed(const jabs_rmethod *m, int constructors, int32_t public_only) {
    const char *member = jabs_rstrings + m->name;
    int is_init = strcmp(member, "<init>") == 0;
    if (constructors ? !is_init : (is_init || strcmp(member, "<clinit>") == 0)) return 0;
    return !public_only || (m->access & JABS_ACC_PUBLIC);
}

/* Order: class-file order. HotSpot's is its method array order, which the JVM specification
 * leaves unspecified (HotSpot sorts by symbol address), so programs cannot rely on either. */
static jabs_object *declared_executables(jabs_object *c, int32_t public_only, int constructors) {
    const char *name = name_of(c, "Class.getDeclaredMethods0/Constructors0 on a Class the registry did not make");
    int record = constructors ? JABS_RR_CONSTRUCTOR : JABS_RR_METHOD;
    const char *descriptor = constructors ? "[Ljava/lang/reflect/Constructor;" : "[Ljava/lang/reflect/Method;";
    /* Primitives and arrays declare nothing. */
    if (name[0] == '[' || is_primitive_name(name)) return record_array(record, descriptor, 0);
    const jabs_rclass *k = info_of(name, "Class.getDeclaredMethods0/Constructors0 for a class without reflection metadata");
    int32_t count = 0;
    for (uint32_t i = 0; i < k->method_count; i++) {
        if (listed(&jabs_rmethods[k->methods + i], constructors, public_only)) count++;
    }
    jabs_object *array = record_array(record, descriptor, count);
    if (!array) return NULL;
    int32_t at = 0;
    for (uint32_t i = 0; i < k->method_count && at < count; i++) {
        if (!listed(&jabs_rmethods[k->methods + i], constructors, public_only)) continue;
        jabs_object *o = constructors ? new_constructor(c, k, i) : new_method(c, k, i);
        if (!o) return NULL;
        elements(array)[at++] = o;
    }
    return array;
}

jabs_object *jabs_reflect_declared_methods(jabs_object *c, int32_t public_only) {
    return declared_executables(c, public_only, 0);
}

jabs_object *jabs_reflect_declared_constructors(jabs_object *c, int32_t public_only) {
    return declared_executables(c, public_only, 1);
}

jabs_object *jabs_reflect_declared_fields(jabs_object *c, int32_t public_only) {
    const char *name = name_of(c, "Class.getDeclaredFields0 on a Class the registry did not make");
    const char *descriptor = "[Ljava/lang/reflect/Field;";
    if (name[0] == '[' || is_primitive_name(name)) return record_array(JABS_RR_FIELD, descriptor, 0);
    const jabs_rclass *k = info_of(name, "Class.getDeclaredFields0 for a class without reflection metadata");
    int32_t count = 0;
    for (uint32_t i = 0; i < k->field_count; i++) {
        if (!public_only || (jabs_rfields[k->fields + i].access & JABS_ACC_PUBLIC)) count++;
    }
    jabs_object *array = record_array(JABS_RR_FIELD, descriptor, count);
    if (!array) return NULL;
    int32_t at = 0;
    for (uint32_t i = 0; i < k->field_count && at < count; i++) {
        if (public_only && !(jabs_rfields[k->fields + i].access & JABS_ACC_PUBLIC)) continue;
        jabs_object *o = new_field(c, k, i);
        if (!o) return NULL;
        elements(array)[at++] = o;
    }
    return array;
}

/* ---- Class attributes ---- */

/* Class.getInterfaces0: direct superinterfaces in class-file order; arrays implement exactly
 * Cloneable and Serializable; primitives none. */
jabs_object *jabs_reflect_interfaces(jabs_object *c) {
    const char *name = name_of(c, "Class.getInterfaces0 on a Class the registry did not make");
    if (is_primitive_name(name)) return class_array(c, 0);
    if (name[0] == '[') {
        jabs_object *cloneable = jabs_class_object("java/lang/Cloneable");
        jabs_object *serializable = cloneable ? jabs_class_object("java/io/Serializable") : NULL;
        jabs_object *array = serializable ? class_array(c, 2) : NULL;
        if (!array) return NULL;
        elements(array)[0] = cloneable;
        elements(array)[1] = serializable;
        return array;
    }
    const jabs_rclass *k = info_of(name, "Class.getInterfaces0 for a class without reflection metadata");
    return named_classes(c, k->interfaces);
}

/* The InnerClasses entry a class has for itself, or NULL. Strings are stored once, so equal
 * offsets mean equal names. */
static const jabs_rinner *own_inner_entry(const jabs_rclass *k) {
    for (uint32_t i = 0; i < k->inner_count; i++) {
        const jabs_rinner *e = &jabs_rinners[k->inner + i];
        if (e->inner == k->name) return e;
    }
    return NULL;
}

/* Metadata of a class, or NULL for primitives and arrays (which have no attributes). */
static const jabs_rclass *class_info_or_null(jabs_object *c, const char *what) {
    const char *name = name_of(c, what);
    if (name[0] == '[' || is_primitive_name(name)) return NULL;
    return info_of(name, what);
}

/* Class.getDeclaredClasses0: member classes named by InnerClasses, in attribute order. */
jabs_object *jabs_reflect_declared_classes(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getDeclaredClasses0");
    if (!k) return class_array(c, 0);
    int32_t count = 0;
    for (uint32_t i = 0; i < k->inner_count; i++) {
        const jabs_rinner *e = &jabs_rinners[k->inner + i];
        if (e->outer == k->name && e->inner != k->name) count++;
    }
    jabs_object *array = class_array(c, count);
    if (!array) return NULL;
    int32_t at = 0;
    for (uint32_t i = 0; i < k->inner_count && at < count; i++) {
        const jabs_rinner *e = &jabs_rinners[k->inner + i];
        if (e->outer != k->name || e->inner == k->name) continue;
        jabs_object *member = jabs_class_object(jabs_rstrings + e->inner);
        if (!member) return NULL;
        elements(array)[at++] = member;
    }
    return array;
}

/* Class.getDeclaringClass0: the outer class of a member class; null for top-level, local and
 * anonymous classes. */
jabs_object *jabs_reflect_declaring_class(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getDeclaringClass0");
    const jabs_rinner *e = k ? own_inner_entry(k) : NULL;
    if (!e || !e->outer) return NULL;
    return jabs_class_object(jabs_rstrings + e->outer);
}

/* Class.getEnclosingMethod0: { enclosing class, method name, method descriptor } from the
 * EnclosingMethod attribute (name and descriptor null when enclosed by an initializer), or
 * null without one. */
jabs_object *jabs_reflect_enclosing_method(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getEnclosingMethod0");
    if (!k || !k->enclosing_class) return NULL;
    jabs_object *enclosing = jabs_class_object(jabs_rstrings + k->enclosing_class);
    if (!enclosing) return NULL;
    jabs_object *name = new_string(k->enclosing_name);
    if (k->enclosing_name && !name) return NULL;
    jabs_object *descriptor = new_string(k->enclosing_descriptor);
    if (k->enclosing_descriptor && !descriptor) return NULL;
    jabs_object *array = jabs_new_array(3, sizeof(jabs_object *), JABS_COMPONENT_ANY, NULL, "[Ljava/lang/Object;", NULL);
    if (!array) return NULL;
    elements(array)[0] = enclosing;
    elements(array)[1] = name;
    elements(array)[2] = descriptor;
    return array;
}

/* Class.getSimpleBinaryName0: the simple name in the class's own InnerClasses entry (null for
 * anonymous classes and classes without an entry). */
jabs_object *jabs_reflect_simple_binary_name(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getSimpleBinaryName0");
    const jabs_rinner *e = k ? own_inner_entry(k) : NULL;
    return e ? new_string(e->simple_name) : NULL;
}

jabs_object *jabs_reflect_generic_signature(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getGenericSignature0");
    return k ? new_string(k->signature) : NULL;
}

jabs_object *jabs_reflect_raw_annotations(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.getRawAnnotations");
    int failed = 0;
    return k ? blob_array(k->annotations, &failed) : NULL;
}

int32_t jabs_reflect_is_record(jabs_object *c) {
    const jabs_rclass *k = class_info_or_null(c, "Class.isRecord0");
    return k ? k->is_record : 0;
}

static int compare_nest(const void *key, const void *entry) {
    return strcmp((const char *)key, jabs_rstrings + ((const jabs_rnest *)entry)->class_name);
}

/* The nest host's name: the NestHost attribute, else the class itself (also for primitives
 * and arrays). The attribute is looked up in jabs_rnests, which covers every class in the unit,
 * not only those with metadata: the caller of a reflective access may have no record. HotSpot
 * also checks that the host lists the class in NestMembers; javac's class files always agree,
 * and pre-11 classes have neither attribute. */
static const char *nest_host_name(jabs_object *c) {
    const char *name = name_of(c, "Class.getNestHost0 on a Class the registry did not make");
    if (name[0] == '[' || is_primitive_name(name)) return name;
    const jabs_rclass *k = jabs_reflect_info_named(name);
    if (k) return k->nest_host ? jabs_rstrings + k->nest_host : name;
    const jabs_rnest *e = jabs_rnest_count
        ? (const jabs_rnest *)bsearch(name, jabs_rnests, jabs_rnest_count, sizeof *jabs_rnests, compare_nest)
        : NULL;
    return e ? jabs_rstrings + e->host : name;
}

jabs_object *jabs_reflect_nest_host(jabs_object *c) { return jabs_class_object(nest_host_name(c)); }

int32_t jabs_reflect_nest_mates(jabs_object *a, jabs_object *b) {
    if (a == b) return 1;
    return strcmp(nest_host_name(a), nest_host_name(b)) == 0;
}

/* ---- Constant pools (only the entries annotation attributes name are kept) ---- */

/* Class.getConstantPool: a new ConstantPool whose constantPoolOop is the class itself; its
 * natives find the class's kept constants from there. */
jabs_object *jabs_reflect_constant_pool(jabs_object *c) {
    jabs_object *pool = jabs_rnew(JABS_RR_CONSTANT_POOL);
    if (!pool) return NULL;
    set_ref(pool, JABS_RF_CONSTANT_POOL_OOP, c);
    return pool;
}

static const jabs_rclass *pool_class(jabs_object *oop) {
    if (!oop) jabs_not_compiled("ConstantPool without a class");
    const char *name = name_of(oop, "ConstantPool of an object that is not a Class");
    if (name[0] == '[' || is_primitive_name(name)) jabs_not_compiled_class("ConstantPool of a class without a constant pool", name);
    return info_of(name, "ConstantPool of a class without reflection metadata");
}

int32_t jabs_reflect_cp_size(jabs_object *oop) { return pool_class(oop)->pool_size; }

/* The kept entry at `index` with JVMS tag `tag`, or NULL with IllegalArgumentException
 * pending (HotSpot's messages). */
static const jabs_rconst *pool_entry(jabs_object *oop, int32_t index, uint8_t tag) {
    const jabs_rclass *k = pool_class(oop);
    if (index <= 0 || index >= (int32_t)k->pool_size) {
        raise_iae("Constant pool index out of bounds");
        return NULL;
    }
    uint32_t lo = 0, hi = k->pool_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (jabs_rconsts[k->pool + mid].index < (uint32_t)index) lo = mid + 1;
        else hi = mid;
    }
    const jabs_rconst *e = lo < k->pool_count ? &jabs_rconsts[k->pool + lo] : NULL;
    if (!e || e->index != (uint32_t)index) {
        jabs_not_compiled_class("ConstantPool entry that no annotation names (the binary keeps only those)", jabs_rstrings + k->name);
    }
    if (e->tag == 0) jabs_not_compiled_class("ConstantPool entry the binary cannot represent (a Utf8 with NUL)", jabs_rstrings + k->name);
    if (e->tag != tag) {
        raise_iae("Wrong type at constant pool index");
        return NULL;
    }
    return e;
}

int32_t jabs_reflect_cp_int(jabs_object *oop, int32_t index) {
    const jabs_rconst *e = pool_entry(oop, index, 3);
    return e ? (int32_t)(uint32_t)e->value : 0;
}

int64_t jabs_reflect_cp_long(jabs_object *oop, int32_t index) {
    const jabs_rconst *e = pool_entry(oop, index, 5);
    return e ? (int64_t)e->value : 0;
}

float jabs_reflect_cp_float(jabs_object *oop, int32_t index) {
    const jabs_rconst *e = pool_entry(oop, index, 4);
    return e ? jabs_fbits((uint32_t)e->value) : 0.0f;
}

double jabs_reflect_cp_double(jabs_object *oop, int32_t index) {
    const jabs_rconst *e = pool_entry(oop, index, 6);
    return e ? jabs_dbits(e->value) : 0.0;
}

jabs_object *jabs_reflect_cp_utf8(jabs_object *oop, int32_t index) {
    const jabs_rconst *e = pool_entry(oop, index, 1);
    if (!e) return NULL;
    if (e->value > UINT32_MAX) jabs_not_compiled("ConstantPool Utf8 entry with a bad string offset");
    return new_string((uint32_t)e->value);
}

/* ---- Calling methods and constructors (DirectMethodHandleAccessor$NativeAccessor.invoke0,
 * DirectConstructorHandleAccessor$NativeAccessor.newInstance0) ----
 *
 * JABS has no method handles, so the JDK's accessor factory always picks these native
 * accessors (the replaced MethodHandleAccessorFactory.useNativeAccessor). What a program sees
 * follows the JDK 25 accessors the reference JVM uses (DirectMethodHandleAccessor), which agree
 * with HotSpot's native reflection except where noted: checks in the order receiver, argument
 * count, then each argument from the first; the same messages; boxing of results through
 * valueOf (so small Integers are the cached ones); and exceptions from the call wrapped in
 * InvocationTargetException. */

static jabs_object *get_ref(jabs_object *o, int field) { return *(jabs_object **)jabs_rf(o, field); }
static int32_t get_int(jabs_object *o, int field) { return *(int32_t *)jabs_rf(o, field); }

/* IllegalArgumentException(cause): how the JDK's accessors report a NullPointerException
 * from argument conversion (a null for a primitive parameter). */
static void raise_iae_caused_by(jabs_object *cause) {
    jabs_object *e = jabs_rnew(JABS_RR_IAE);
    if (!e) return;
    jabs_value args[1], result;
    args[0].a = cause;
    if (jabs_rcall(JABS_RJ_IAE_CAUSE, e, args, &result) == 0) jabs_pending = e;
}

/* Replace the exception the invoked code threw with an InvocationTargetException wrapping it. */
static void wrap_pending_in_ite(void) {
    jabs_object *target = jabs_pending;
    jabs_pending = NULL;
    jabs_object *e = jabs_rnew(JABS_RR_ITE);
    if (!e) return;
    jabs_value args[1], result;
    args[0].a = target;
    if (jabs_rcall(JABS_RJ_ITE, e, args, &result) == 0) jabs_pending = e;
}

/* JABS_RK_* of a primitive descriptor character, or JABS_RK_L. */
static int kind_of_code(char code) {
    static const char codes[] = "ZBCSIJFD";
    const char *at = code ? strchr(codes, code) : NULL;
    return at ? (int)(at - codes) : JABS_RK_L;
}

/* The JABS_RK_* of a box object (Boolean .. Double), or -1. */
static int box_kind(jabs_object *o) {
    for (int k = JABS_RK_Z; k <= JABS_RK_D; k++) {
        const jabs_class *r = jabs_rrecord(JABS_RR_BOX + k);
        if (r && o->cls == r) return k;
    }
    return -1;
}

/* A box's value in the jabs_value member its kind uses (int kinds in .i). */
static jabs_value box_value(jabs_object *o, int kind) {
    jabs_value v;
    void *p = jabs_rf(o, JABS_RF_BOX_VALUE + kind);
    switch (kind) {
    case JABS_RK_Z: v.i = *(uint8_t *)p; break;
    case JABS_RK_B: v.i = *(int8_t *)p; break;
    case JABS_RK_C: v.i = *(uint16_t *)p; break;
    case JABS_RK_S: v.i = *(int16_t *)p; break;
    case JABS_RK_I: v.i = *(int32_t *)p; break;
    case JABS_RK_J: v.j = *(int64_t *)p; break;
    case JABS_RK_F: v.f = *(float *)p; break;
    case JABS_RK_D: v.d = *(double *)p; break;
    default: jabs_not_compiled("reflection: box of an unknown kind");
    }
    return v;
}

/* JLS 5.1.2 widening from kind `from` to kind `to` (identity included); 0 if not allowed. */
static int widen(int from, int to, jabs_value *v) {
    if (from == to) return 1;
    if (from == JABS_RK_Z || to == JABS_RK_Z) return 0;
    /* What each source widens to: byte -> short, int, long, float, double; short and char ->
     * int, long, float, double; int -> long, float, double; long -> float, double; float -> double. */
    int ok;
    switch (from) {
    case JABS_RK_B: ok = to == JABS_RK_S || to >= JABS_RK_I; break;
    case JABS_RK_S: case JABS_RK_C: ok = to >= JABS_RK_I; break;
    case JABS_RK_I: ok = to >= JABS_RK_J; break;
    case JABS_RK_J: ok = to >= JABS_RK_F; break;
    case JABS_RK_F: ok = to == JABS_RK_D; break;
    default: ok = 0; break;
    }
    if (!ok) return 0;
    switch (to) {
    case JABS_RK_S: case JABS_RK_I: return 1; /* stays in .i */
    case JABS_RK_J: v->j = (int64_t)v->i; return 1; /* every source of a widening to long is an int kind */
    case JABS_RK_F: v->f = from == JABS_RK_J ? (float)v->j : (float)v->i; return 1;
    case JABS_RK_D:
        v->d = from == JABS_RK_J ? (double)v->j : from == JABS_RK_F ? (double)v->f : (double)v->i;
        return 1;
    default: return 0;
    }
}

/* Whether `o` (not null) is an instance of the class `c`. A class without a record has no
 * instances: every object's class and all its supertypes have records (add_records). */
static int instance_of(jabs_object *c, jabs_object *o) {
    const char *name = name_of(c, "reflection type check against a Class the registry did not make");
    if (strcmp(name, "java/lang/Object") == 0) return 1;
    if (is_primitive_name(name)) return 0;
    if (name[0] != '[' && !jabs_reflect_class_record(c)) return 0;
    return jabs_class_is_instance(c, o);
}

enum { ARGUMENT_OK, ARGUMENT_MISMATCH, ARGUMENT_NULL };

/* Convert one argument for a parameter of descriptor `d` (class `type`), as the JDK's
 * accessors do: unboxing then widening for primitives, an instance check for references. */
static int convert_argument(const char *d, jabs_object *type, jabs_object *arg, jabs_value *out) {
    int to = d[0] == 'L' || d[0] == '[' ? JABS_RK_L : kind_of_code(d[0]);
    if (to == JABS_RK_L) {
        if (arg && !instance_of(type, arg)) return ARGUMENT_MISMATCH;
        out->a = arg;
        return ARGUMENT_OK;
    }
    if (!arg) return ARGUMENT_NULL;
    int from = box_kind(arg);
    if (from < 0) return ARGUMENT_MISMATCH;
    *out = box_value(arg, from);
    return widen(from, to, out) ? ARGUMENT_OK : ARGUMENT_MISMATCH;
}

/* Check the argument count and convert every argument; 0, or -1 with
 * IllegalArgumentException pending. */
static int convert_arguments(const parsed_descriptor *p, jabs_object *types, jabs_object *args, jabs_value *values) {
    int32_t given = args ? ((jabs_array *)args)->length : 0;
    if (given != p->count) {
        char message[96];
        snprintf(message, sizeof message, "wrong number of arguments: %d expected: %d", (int)given, p->count);
        raise_iae(message);
        return -1;
    }
    if (!types || ((jabs_array *)types)->length != p->count) jabs_not_compiled("reflection: parameter types do not match the descriptor");
    for (int i = 0; i < p->count; i++) {
        int r = convert_argument(p->at[i], elements(types)[i], elements(args)[i], &values[i]);
        if (r == ARGUMENT_OK) continue;
        if (r == ARGUMENT_MISMATCH) raise_iae("argument type mismatch");
        else raise_iae_caused_by(jabs_vm_throwable(JABS_VM_NULL_POINTER));
        return -1;
    }
    return 0;
}

/* The metadata row of a Method/Constructor object, from its clazz and slot. */
static const jabs_rmethod *executable_row(jabs_object *holder, int32_t slot, const jabs_rclass **k_out) {
    const char *name = name_of(holder, "reflective call on a member of a Class the registry did not make");
    const jabs_rclass *k = info_of(name, "reflective call on a member of a class without reflection metadata");
    if (slot < 0 || (uint32_t)slot >= k->method_count) jabs_not_compiled_class("reflective call with a slot outside the class", name);
    *k_out = k;
    return &jabs_rmethods[k->methods + (uint32_t)slot];
}

static jabs_rcaller caller_of(const jabs_rmethod *m) {
    if (m->caller >= jabs_rcaller_count) jabs_not_compiled("reflective call of a method whose descriptor JABS cannot call");
    return jabs_rcallers[m->caller];
}

/* Box a result of descriptor `d` (Integer.valueOf ...); void gives null. */
static jabs_object *box_result(const char *d, jabs_value result) {
    if (d[0] == 'V') return NULL;
    if (d[0] == 'L' || d[0] == '[') return result.a;
    int kind = kind_of_code(d[0]);
    jabs_value boxed;
    jabs_value args[1];
    args[0] = result;
    if (jabs_rcall(JABS_RJ_VALUE_OF + kind, NULL, args, &boxed) != 0) return NULL;
    return boxed.a;
}

/* Diagnostic naming the member a reflective call cannot reach. */
static _Noreturn void not_compiled_member(const char *what, const jabs_rclass *k, const jabs_rmethod *m) {
    static char message[1024];
    snprintf(message, sizeof message, "%s %s.%s%s (it is not compiled into this program)", what, jabs_rstrings + k->name,
             jabs_rstrings + m->name, jabs_rstrings + m->descriptor);
    jabs_not_compiled(message);
}

/* DirectMethodHandleAccessor.checkReceiver's message (HotSpot's native path says only "object
 * is not an instance of declaring class"; the JDK 25 accessors are what programs see). */
static void raise_not_an_instance(jabs_object *receiver, jabs_object *holder) {
    char message[2 * JABS_MAX_NAME + 64];
    const char *of = name_of(jabs_class_of(receiver), "receiver of a reflective call");
    const char *declaring = name_of(holder, "declaring class of a reflective call");
    int n = snprintf(message, sizeof message, "object of type %s is not an instance of %s", of, declaring);
    if (n < 0 || (size_t)n >= sizeof message) jabs_not_compiled("reflection: class name too long for a message");
    for (char *p = message; *p; p++) if (*p == '/') *p = '.'; /* Class.getName spells packages with dots */
    raise_iae(message);
}

jabs_object *jabs_reflect_invoke(jabs_object *method, jabs_object *receiver, jabs_object *args) {
    if (!method) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    jabs_object *holder = get_ref(method, JABS_RF_METHOD_CLAZZ);
    const jabs_rclass *k;
    const jabs_rmethod *m = executable_row(holder, get_int(method, JABS_RF_METHOD_SLOT), &k);
    int is_static = (m->access & JABS_ACC_STATIC) != 0;
    if (is_static) {
        /* The JDK initializes the class when it makes the accessor; HotSpot's native path does
         * it on every call. Either way it happens before anything else. */
        jabs_ensure_initialized(holder);
        if (jabs_pending) return NULL;
    } else {
        if (!receiver) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
        if (!instance_of(holder, receiver)) {
            raise_not_an_instance(receiver, holder);
            return NULL;
        }
    }
    parsed_descriptor p;
    parse_descriptor(rstring(m->descriptor), &p);
    jabs_value values[JABS_MAX_PARAMETERS];
    if (convert_arguments(&p, get_ref(method, JABS_RF_METHOD_PARAMETER_TYPES), args, values) != 0) return NULL;
    /* Dispatch as Reflection::invoke: through the receiver's interface table or vtable for an
     * overridable method, else the method itself. */
    void (*fn)(void) = m->code;
    if (!is_static && (m->selector || m->vtable_slot >= 0)) {
        if (receiver->cls == jabs_array_class()) not_compiled_member("Method.invoke on an array receiver of", k, m);
        fn = m->selector ? jabs_itable_find(receiver, m->selector) : receiver->cls->vtable[m->vtable_slot];
    }
    if (!fn) not_compiled_member("Method.invoke of", k, m);
    jabs_value result;
    result.j = 0;
    caller_of(m)(fn, receiver, values, &result);
    if (jabs_pending) { wrap_pending_in_ite(); return NULL; }
    return box_result(p.ret, result);
}

jabs_object *jabs_reflect_new_instance(jabs_object *constructor, jabs_object *args) {
    if (!constructor) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    jabs_object *holder = get_ref(constructor, JABS_RF_CTOR_CLAZZ);
    const jabs_rclass *k;
    const jabs_rmethod *m = executable_row(holder, get_int(constructor, JABS_RF_CTOR_SLOT), &k);
    /* ReflectionFactory hands abstract classes an accessor that throws before reaching here;
     * this is the JVM's own check behind it. */
    if (k->access & (JABS_ACC_ABSTRACT | JABS_ACC_INTERFACE)) { jabs_pending = jabs_vm_throwable(JABS_VM_INSTANTIATION); return NULL; }
    const jabs_class *record = jabs_reflect_class_record(holder);
    if (!record || !m->code) not_compiled_member("Constructor.newInstance of", k, m);
    jabs_ensure_initialized(holder);
    if (jabs_pending) return NULL;
    parsed_descriptor p;
    parse_descriptor(rstring(m->descriptor), &p);
    jabs_value values[JABS_MAX_PARAMETERS];
    if (convert_arguments(&p, get_ref(constructor, JABS_RF_CTOR_PARAMETER_TYPES), args, values) != 0) return NULL;
    jabs_object *o = jabs_new(record);
    if (!o) return NULL;
    jabs_value ignored;
    caller_of(m)(m->code, o, values, &ignored);
    if (jabs_pending) { wrap_pending_in_ite(); return NULL; }
    return o;
}

/* ---- Field access (the JDK's MethodHandle<T>FieldAccessorImpl, with JABS leaves) ---- */

/* The metadata row of a Field object. */
static const jabs_rfield *field_row(jabs_object *field, jabs_object **holder_out) {
    jabs_object *holder = get_ref(field, JABS_RF_FIELD_CLAZZ);
    const char *name = name_of(holder, "reflective field access on a Class the registry did not make");
    const jabs_rclass *k = info_of(name, "reflective field access on a class without reflection metadata");
    int32_t slot = get_int(field, JABS_RF_FIELD_SLOT);
    if (slot < 0 || (uint32_t)slot >= k->field_count) jabs_not_compiled_class("reflective field access with a slot outside the class", name);
    *holder_out = holder;
    return &jabs_rfields[k->fields + (uint32_t)slot];
}

/* MethodHandleAccessorFactory.newFieldAccessor(field, isReadOnly): the JDK's accessor for
 * the field's type, made by its own constructor, with no method handles (the leaves that
 * would call them are replaced). The class is initialized first, as the JDK does. */
jabs_object *jabs_reflect_new_field_accessor(jabs_object *field, int32_t read_only) {
    if (!field) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    jabs_object *holder;
    const jabs_rfield *f = field_row(field, &holder);
    jabs_ensure_initialized(holder);
    if (jabs_pending) return NULL;
    int kind = kind_of_code(jabs_rstrings[f->descriptor]);
    jabs_object *accessor = jabs_rnew(JABS_RR_FIELD_ACCESSOR + kind);
    if (!accessor) return NULL;
    jabs_value args[5], ignored;
    args[0].a = field;
    args[1].a = NULL;
    args[2].a = NULL;
    args[3].i = read_only != 0;
    args[4].i = (f->access & JABS_ACC_STATIC) != 0;
    if (jabs_rcall(JABS_RJ_ACCESSOR_INIT + kind, accessor, args, &ignored) != 0) return NULL;
    return accessor;
}

/* The storage an accessor reads or writes for `o`, or NULL with an exception pending: the
 * static field's storage; else NullPointerException for null, and for an object of the wrong
 * class the IllegalArgumentException the JDK's leaf makes (newGet/newSetIllegalArgumentException). */
void *jabs_reflect_field_slot(jabs_object *accessor, jabs_object *o, int32_t for_set, int32_t *is_volatile) {
    jabs_object *field = get_ref(accessor, JABS_RF_ACCESSOR_FIELD);
    jabs_object *holder;
    const jabs_rfield *f = field_row(field, &holder);
    *is_volatile = (f->access & JABS_ACC_VOLATILE) != 0;
    if (f->access & JABS_ACC_STATIC) {
        if (!f->address) {
            static char message[1024];
            snprintf(message, sizeof message, "reflective access to static field %s.%s (its class's statics are not compiled into this program)",
                     name_of(holder, "static field holder"), jabs_rstrings + f->name);
            jabs_not_compiled(message);
        }
        return f->address;
    }
    if (!o) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    if (!instance_of(holder, o)) {
        jabs_value args[1], made;
        args[0].a = o;
        if (jabs_rcall(for_set ? JABS_RJ_NEW_SET_IAE : JABS_RJ_NEW_GET_IAE, accessor, args, &made) == 0) jabs_pending = made.a;
        return NULL;
    }
    return (char *)o + f->offset;
}

/* The primitive set leaves' final-field rule: a read-only accessor checks the receiver
 * (ensureObj) and throws throwFinalFieldIllegalAccessException(value). 1 when the set must not
 * happen (an exception is pending), 0 to go on. */
int32_t jabs_reflect_field_refuse_set(jabs_object *accessor, jabs_object *o, int kind, jabs_value value) {
    jabs_value args[1], result;
    if (jabs_rcall(JABS_RJ_IS_READ_ONLY, accessor, args, &result) != 0) return 1;
    if (!result.i) return 0;
    args[0].a = o;
    if (jabs_rcall(JABS_RJ_ENSURE_OBJ, accessor, args, &result) != 0) return 1;
    args[0] = value;
    jabs_rcall(JABS_RJ_THROW_FINAL + kind, accessor, args, &result);
    if (!jabs_pending) jabs_not_compiled("throwFinalFieldIllegalAccessException returned without throwing");
    return 1;
}

/* MethodHandleObjectFieldAccessorImpl.set: ensureObj, the final-field rule, then the store,
 * whose type check (the setter handle's cast) fails with throwSetIllegalArgumentException(value). */
void jabs_reflect_field_set_object(jabs_object *accessor, jabs_object *o, jabs_object *value) {
    jabs_value args[1], result;
    args[0].a = o;
    if (jabs_rcall(JABS_RJ_ENSURE_OBJ, accessor, args, &result) != 0) return;
    if (jabs_rcall(JABS_RJ_IS_READ_ONLY, accessor, args, &result) != 0) return;
    if (result.i) {
        args[0].a = value;
        jabs_rcall(JABS_RJ_THROW_FINAL + JABS_RK_L, accessor, args, &result);
        if (!jabs_pending) jabs_not_compiled("throwFinalFieldIllegalAccessException returned without throwing");
        return;
    }
    int32_t is_volatile = 0;
    jabs_object **p = (jabs_object **)jabs_reflect_field_slot(accessor, o, 1, &is_volatile);
    if (!p) return;
    if (value) {
        jabs_object *type = get_ref(get_ref(accessor, JABS_RF_ACCESSOR_FIELD), JABS_RF_FIELD_TYPE);
        if (!instance_of(type, value)) {
            args[0].a = value;
            jabs_rcall(JABS_RJ_THROW_SET_IAE, accessor, args, &result);
            if (!jabs_pending) jabs_not_compiled("throwSetIllegalArgumentException returned without throwing");
            return;
        }
    }
    if (is_volatile) __atomic_store(p, &value, __ATOMIC_SEQ_CST);
    else *p = value;
}

/* ---- Proxies (classes generated at build time by src/proxy.rs) ----
 *
 * The JVM spins a proxy class per (class loader, interface list) at run time. JABS generated
 * one per interface list the build could foresee; jabs_rproxies lists those compiled in. The
 * class loader does not take part: a JABS program has one definition of every interface. */

/* Whether row `p` is the proxy of exactly these interfaces, in this order. */
static int proxy_row_matches(const jabs_rproxy *p, jabs_object *interfaces, int32_t count) {
    uint32_t listed = p->interfaces ? jabs_rlists[p->interfaces] : 0;
    if (count < 0 || listed != (uint32_t)count) return 0;
    for (int32_t i = 0; i < count; i++) {
        const char *name = name_of(elements(interfaces)[i], "Proxy interface list with a Class the registry did not make");
        if (strcmp(name, jabs_rstrings + jabs_rlists[p->interfaces + 1 + (uint32_t)i]) != 0) return 0;
    }
    return 1;
}

/* Stop: the program asked for a proxy of a list the build did not generate. The message names
 * the interfaces as Class.getName spells them. */
static _Noreturn void proxy_not_generated(jabs_object *interfaces, int32_t count) {
    static char message[2048];
    size_t at = 0;
    int n = snprintf(message, sizeof message, "java.lang.reflect.Proxy for interfaces [");
    if (n > 0) at = (size_t)n;
    for (int32_t i = 0; i < count && at < sizeof message; i++) {
        const char *name = name_of(elements(interfaces)[i], "Proxy interface list with a Class the registry did not make");
        n = snprintf(message + at, sizeof message - at, "%s%s", i ? ", " : "", name);
        if (n < 0) break;
        at += (size_t)n;
    }
    if (at < sizeof message) {
        snprintf(message + at, sizeof message - at,
                 "] (no proxy class was generated for this interface list at build time; src/proxy.rs decides the lists)");
    }
    for (char *p = message; *p; p++) if (*p == '/') *p = '.';
    jabs_not_compiled(message);
}

/* Proxy.getProxyConstructor(loader, interfaces): the generated class's (InvocationHandler)
 * constructor, made accessible as ProxyBuilder.build makes it. The same Constructor is
 * returned every time, as the JDK's cache does. */
jabs_object *jabs_reflect_proxy_constructor(jabs_object *loader, jabs_object *interfaces) {
    (void)loader;
    if (!interfaces) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    int32_t count = ((jabs_array *)interfaces)->length;
    /* ProxyBuilder reads every interface's name first: a null element is a NullPointerException. */
    for (int32_t i = 0; i < count; i++) {
        if (!elements(interfaces)[i]) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    }
    for (uint32_t r = 0; r < jabs_rproxy_count; r++) {
        const jabs_rproxy *p = &jabs_rproxies[r];
        if (!proxy_row_matches(p, interfaces, count)) continue;
        jabs_object *cached = __atomic_load_n(&jabs_rproxy_constructors[r], __ATOMIC_ACQUIRE);
        if (cached) return cached;
        jabs_object *c = jabs_class_object(jabs_rstrings + p->name);
        jabs_object *constructors = c ? jabs_reflect_declared_constructors(c, 0) : NULL;
        if (!constructors) return NULL;
        if (((jabs_array *)constructors)->length != 1) {
            jabs_not_compiled_class("a generated proxy class without exactly one constructor", jabs_rstrings + p->name);
        }
        jabs_object *constructor = elements(constructors)[0];
        *(uint8_t *)jabs_rf(constructor, JABS_RF_OVERRIDE) = 1;
        /* Two threads may build one at once: the first stored is the one everybody gets. */
        jabs_object *expected = NULL;
        if (!__atomic_compare_exchange_n(&jabs_rproxy_constructors[r], &expected, constructor, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            return expected;
        }
        return constructor;
    }
    proxy_not_generated(interfaces, count);
}

/* ProxyBuilder.isProxyClass(c): whether c is one of the generated proxy classes. */
int32_t jabs_reflect_is_proxy_class(jabs_object *c) {
    if (!c) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }
    const char *name = name_of(c, "ProxyBuilder.isProxyClass on a Class the registry did not make");
    for (uint32_t r = 0; r < jabs_rproxy_count; r++) {
        if (strcmp(name, jabs_rstrings + jabs_rproxies[r].name) == 0) return 1;
    }
    return 0;
}

#endif /* JABS_HAVE_REFLECTION */

/* Keeps this file a valid translation unit in programs without reflection. */
typedef int jabs_reflect_translation_unit;
