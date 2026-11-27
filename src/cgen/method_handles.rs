//! `java.lang.invoke.MethodHandleNatives` and `MemberName` VM glue for JDK 25.
//!
//! Backs `registerNatives`, `getNamedCon`, `init`, `expand`, `resolve`, `objectFieldOffset`,
//! `staticFieldOffset`, `staticFieldBase`, and `getMemberVMInfo` using JABS's reflection
//! metadata tables (`jabs_rclasses`, `jabs_rmethods`, `jabs_rfields` in `runtime/jabs_reflect.h`)
//! and `LambdaForm.compileToBytecode` (kept interpreted since JABS has no runtime class
//! definition).

use super::*;

const METHOD_HANDLE_NATIVES: &str = "java/lang/invoke/MethodHandleNatives";
const MEMBER_NAME: &str = "java/lang/invoke/MemberName";
const RESOLVED_METHOD_NAME: &str = "java/lang/invoke/ResolvedMethodName";
const METHOD_HANDLE: &str = "java/lang/invoke/MethodHandle";
const VAR_HANDLE: &str = "java/lang/invoke/VarHandle";
const METHOD_TYPE: &str = "java/lang/invoke/MethodType";
const LAMBDA_FORM: &str = "java/lang/invoke/LambdaForm";
const REFLECT_METHOD: &str = "java/lang/reflect/Method";
const REFLECT_CONSTRUCTOR: &str = "java/lang/reflect/Constructor";
const REFLECT_FIELD: &str = "java/lang/reflect/Field";
const LONG: &str = "java/lang/Long";

const RESOLVE_DESC: &str =
    "(Ljava/lang/invoke/MemberName;Ljava/lang/Class;IZ)Ljava/lang/invoke/MemberName;";
const INIT_DESC: &str = "(Ljava/lang/invoke/MemberName;Ljava/lang/Object;)V";
const EXPAND_DESC: &str = "(Ljava/lang/invoke/MemberName;)V";
const FIELD_OFFSET_DESC: &str = "(Ljava/lang/invoke/MemberName;)J";
const STATIC_BASE_DESC: &str = "(Ljava/lang/invoke/MemberName;)Ljava/lang/Object;";
const VM_INFO_DESC: &str = "(Ljava/lang/invoke/MemberName;)Ljava/lang/Object;";
const NAMED_CON_DESC: &str = "(I[Ljava/lang/Object;)I";

const MN_CALLER_SENSITIVE: u32 = 0x0010_0000;
const MN_HIDDEN_MEMBER: u32 = 0x0040_0000;

/// Constants defined in `java.lang.invoke.MethodHandleNatives$Constants` (JDK 25).
const NAMED_CONSTANTS: &[(&str, i32)] = &[
    ("MN_IS_METHOD", 0x0001_0000),
    ("MN_IS_CONSTRUCTOR", 0x0002_0000),
    ("MN_IS_FIELD", 0x0004_0000),
    ("MN_IS_TYPE", 0x0008_0000),
    ("MN_CALLER_SENSITIVE", 0x0010_0000),
    ("MN_TRUSTED_FINAL", 0x0020_0000),
    ("MN_HIDDEN_MEMBER", 0x0040_0000),
    ("MN_REFERENCE_KIND_SHIFT", 24),
    ("MN_REFERENCE_KIND_MASK", 15),
    ("REF_NONE", 0),
    ("REF_getField", 1),
    ("REF_getStatic", 2),
    ("REF_putField", 3),
    ("REF_putStatic", 4),
    ("REF_invokeVirtual", 5),
    ("REF_invokeStatic", 6),
    ("REF_invokeSpecial", 7),
    ("REF_newInvokeSpecial", 8),
    ("REF_invokeInterface", 9),
    ("REF_LIMIT", 10),
    ("NESTMATE_CLASS", 1),
    ("HIDDEN_CLASS", 2),
    ("STRONG_LOADER_LINK", 4),
    ("ACCESS_VM_ANNOTATIONS", 8),
    ("LM_MODULE", 16),
    ("LM_UNCONDITIONAL", 32),
    ("LM_TRUSTED", -1),
];

/// VM errors/exceptions raised by `MethodHandleNatives` natives, in `JABS_MH_ERR_*` order.
const RAISED: &[(&str, &str)] = &[
    ("java/lang/NoSuchMethodError", "JABS_MH_ERR_NO_SUCH_METHOD"),
    ("java/lang/NoSuchFieldError", "JABS_MH_ERR_NO_SUCH_FIELD"),
    ("java/lang/LinkageError", "JABS_MH_ERR_LINKAGE"),
    ("java/lang/IncompatibleClassChangeError", "JABS_MH_ERR_ICCE"),
    ("java/lang/IllegalAccessError", "JABS_MH_ERR_ILLEGAL_ACCESS"),
    (
        "java/lang/IllegalArgumentException",
        "JABS_MH_ERR_ILLEGAL_ARGUMENT",
    ),
    (
        "java/lang/ClassNotFoundException",
        "JABS_MH_ERR_CLASS_NOT_FOUND",
    ),
    ("java/lang/InternalError", "JABS_MH_ERR_INTERNAL"),
];

fn is_mh_member_native(key: &MethodKey) -> bool {
    key.0 == METHOD_HANDLE_NATIVES
        && matches!(
            (key.1.as_str(), key.2.as_str()),
            ("init", INIT_DESC)
                | ("expand", EXPAND_DESC)
                | ("resolve", RESOLVE_DESC)
                | ("objectFieldOffset", FIELD_OFFSET_DESC)
                | ("staticFieldOffset", FIELD_OFFSET_DESC)
                | ("staticFieldBase", STATIC_BASE_DESC)
                | ("getMemberVMInfo", VM_INFO_DESC)
        )
}

/// Java methods invoked from C by `MethodHandleNatives` natives so RTA keeps them compiled.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if !is_mh_member_native(key) {
        return Vec::new();
    }
    let mut calls = vec![(RESOLVED_METHOD_NAME, "<init>", "()V")];
    if key.1 == "getMemberVMInfo" {
        calls.push((LONG, "valueOf", "(J)Ljava/lang/Long;"));
    }
    if matches!(
        key.1.as_str(),
        "staticFieldOffset" | "staticFieldBase" | "getMemberVMInfo"
    ) {
        // Ensure static-field globals exist even if no Java instruction initialized the holder.
        calls.push((UNSAFE, "ensureClassInitialized", "(Ljava/lang/Class;)V"));
    }
    for (exc, _) in RAISED {
        calls.push((exc, "<init>", "(Ljava/lang/String;)V"));
    }
    calls
}

/// Ensure the reflection/member classes read or allocated by `MethodHandleNatives` have records.
pub(super) fn reach(
    unit: &Unit,
    records: &BTreeSet<String>,
    methods: &BTreeSet<MethodKey>,
) -> Vec<String> {
    if !methods.iter().any(is_mh_member_native) {
        return Vec::new();
    }
    let needed = [
        OBJECT,
        STRING,
        LONG,
        MEMBER_NAME,
        RESOLVED_METHOD_NAME,
        METHOD_TYPE,
        REFLECT_METHOD,
        REFLECT_CONSTRUCTOR,
        REFLECT_FIELD,
    ];
    let mut missing = Vec::new();
    for name in needed {
        if !records.contains(name) && unit.get(name).is_some() && unit.chain(name).is_ok() {
            missing.push(name.to_string());
        }
    }
    missing
}

/// Java methods replaced for `java.lang.invoke`: JABS has no runtime bytecode class definition
/// (`ClassLoader.defineClass0`), so `LambdaForm.prepare` and `LambdaForm.compileToBytecode` stay
/// no-ops (leaving `LambdaForm` uncompiled).
pub(super) fn replacement(key: &MethodKey) -> Option<(String, bool)> {
    if key.0 == LAMBDA_FORM
        && matches!(key.1.as_str(), "prepare" | "compileToBytecode")
        && key.2 == "()V"
    {
        return Some((
            "(void)self; /* JABS cannot dynamically define hidden classes; leave LambdaForm uncompiled */"
                .into(),
            false,
        ));
    }
    None
}

/// Intrinsic C body for `MethodHandleNatives` natives: `(C body, needs_classes)`.
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if key.0 != METHOD_HANDLE_NATIVES {
        return None;
    }
    match (key.1.as_str(), key.2.as_str()) {
        ("registerNatives", "()V") => Some(("/* JABS binds natives statically */".into(), false)),
        ("getNamedCon", NAMED_CON_DESC) => {
            classes_available.then(|| ("return jabs_mh_get_named_con(a0, a1);".into(), true))
        }
        ("init", INIT_DESC) => classes_available.then(|| ("jabs_mh_init(a0, a1);".into(), true)),
        ("expand", EXPAND_DESC) => classes_available.then(|| ("jabs_mh_expand(a0);".into(), true)),
        ("resolve", RESOLVE_DESC) => {
            classes_available.then(|| ("return jabs_mh_resolve(a0, a1, a2, a3);".into(), true))
        }
        ("objectFieldOffset", FIELD_OFFSET_DESC) => {
            classes_available.then(|| ("return jabs_mh_object_field_offset(a0);".into(), true))
        }
        ("staticFieldOffset", FIELD_OFFSET_DESC) => {
            classes_available.then(|| ("return jabs_mh_static_field_offset(a0);".into(), true))
        }
        ("staticFieldBase", STATIC_BASE_DESC) => {
            classes_available.then(|| ("return jabs_mh_static_field_base(a0);".into(), true))
        }
        ("getMemberVMInfo", VM_INFO_DESC) => {
            classes_available.then(|| ("return jabs_mh_get_member_vm_info(a0);".into(), true))
        }
        _ => None,
    }
}

/// C glue emitted into `unit_0.c` when the class registry is present.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let mut out =
        String::from("\n/* ---- MethodHandleNatives glue (src/cgen/method_handles.rs) ---- */\n");

    // `getNamedCon` only depends on arrays and Strings, so emit it unconditionally with the registry.
    out.push_str("typedef struct { const char *name; int32_t value; } jabs_mh_con_entry;\n");
    out.push_str("static const jabs_mh_con_entry jabs_mh_named_cons[] = {\n");
    for (name, value) in NAMED_CONSTANTS {
        let _ = writeln!(out, "    {{ \"{name}\", {value} }},");
    }
    out.push_str("    { NULL, 0 }\n};\n");
    let _ = write!(
        out,
        r#"int32_t jabs_mh_get_named_con(int32_t which, jabs_object *box_obj) {{
    if (which < 0 || (size_t)which >= {con_count}) return 0;
    if (box_obj) {{
        if (box_obj->cls != jabs_array_class()) jabs_not_compiled("MethodHandleNatives.getNamedCon box is not an array");
        jabs_array *arr = (jabs_array *)box_obj;
        if (!jabs_array_is_ref(arr)) jabs_not_compiled("MethodHandleNatives.getNamedCon box is a primitive array");
        if (arr->length > 0) {{
            jabs_object *s = jabs_string_from_utf8(jabs_mh_named_cons[which].name);
            if (!s) return 0;
            if (!jabs_array_store_ok(arr, s)) {{
                jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_STORE);
                return 0;
            }}
            ((jabs_object **)jabs_array_data(arr))[0] = s;
        }}
    }}
    return jabs_mh_named_cons[which].value;
}}
"#,
        con_count = NAMED_CONSTANTS.len(),
    );

    let has_field = |class: &str, name: &str, descriptor: &str| {
        unit.get(class).is_some_and(|c| {
            c.access & ACC_INTERFACE == 0
                && unit.chain(class).is_ok()
                && c.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
        })
    };
    let has_method = |class: &str, name: &str, descriptor: &str| {
        methods.contains(&(class.to_string(), name.to_string(), descriptor.to_string()))
    };

    let mh_ok = records.contains(OBJECT)
        && records.contains(STRING)
        && records.contains(MEMBER_NAME)
        && records.contains(RESOLVED_METHOD_NAME)
        && inits.contains(RESOLVED_METHOD_NAME)
        && records.contains(METHOD_TYPE)
        && records.contains(REFLECT_METHOD)
        && records.contains(REFLECT_CONSTRUCTOR)
        && records.contains(REFLECT_FIELD)
        && has_method(RESOLVED_METHOD_NAME, "<init>", "()V")
        && has_field(MEMBER_NAME, "clazz", "Ljava/lang/Class;")
        && has_field(MEMBER_NAME, "name", "Ljava/lang/String;")
        && has_field(MEMBER_NAME, "type", "Ljava/lang/Object;")
        && has_field(MEMBER_NAME, "flags", "I")
        && has_field(
            MEMBER_NAME,
            "method",
            "Ljava/lang/invoke/ResolvedMethodName;",
        )
        && has_field(MEMBER_NAME, "resolution", "Ljava/lang/Object;")
        && has_field(RESOLVED_METHOD_NAME, "vmholder", "Ljava/lang/Class;")
        && has_field(METHOD_TYPE, "rtype", "Ljava/lang/Class;")
        && has_field(METHOD_TYPE, "ptypes", "[Ljava/lang/Class;")
        && has_field(REFLECT_METHOD, "clazz", "Ljava/lang/Class;")
        && has_field(REFLECT_METHOD, "slot", "I")
        && has_field(REFLECT_METHOD, "name", "Ljava/lang/String;")
        && has_field(REFLECT_CONSTRUCTOR, "clazz", "Ljava/lang/Class;")
        && has_field(REFLECT_CONSTRUCTOR, "slot", "I")
        && has_field(REFLECT_FIELD, "clazz", "Ljava/lang/Class;")
        && has_field(REFLECT_FIELD, "slot", "I");

    if !mh_ok {
        out.push_str(
            r#"void jabs_mh_init(jabs_object *mn, jabs_object *target) { (void)mn; (void)target; jabs_not_compiled("MethodHandleNatives.init (MemberName layout unavailable)"); }
void jabs_mh_expand(jabs_object *mn) { (void)mn; jabs_not_compiled("MethodHandleNatives.expand (MemberName layout unavailable)"); }
jabs_object *jabs_mh_resolve(jabs_object *mn, jabs_object *caller, int32_t mode, int32_t spec) { (void)mn; (void)caller; (void)mode; (void)spec; jabs_not_compiled("MethodHandleNatives.resolve (MemberName layout unavailable)"); }
int64_t jabs_mh_object_field_offset(jabs_object *mn) { (void)mn; jabs_not_compiled("MethodHandleNatives.objectFieldOffset (MemberName layout unavailable)"); }
int64_t jabs_mh_static_field_offset(jabs_object *mn) { (void)mn; jabs_not_compiled("MethodHandleNatives.staticFieldOffset (MemberName layout unavailable)"); }
jabs_object *jabs_mh_static_field_base(jabs_object *mn) { (void)mn; jabs_not_compiled("MethodHandleNatives.staticFieldBase (MemberName layout unavailable)"); }
jabs_object *jabs_mh_get_member_vm_info(jabs_object *mn) { (void)mn; jabs_not_compiled("MethodHandleNatives.getMemberVMInfo (MemberName layout unavailable)"); }
"#,
        );
        return out;
    }

    // Emit exception helper `jabs_mh_raise` (using compiled `(String)` constructors when present).
    out.push_str("enum {\n");
    for (_, tag) in RAISED {
        let _ = writeln!(out, "    {tag},");
    }
    out.push_str("    JABS_MH_ERR_COUNT\n};\n");
    out.push_str("static void jabs_mh_raise(int kind, const char *message) {\n");
    out.push_str("    jabs_object *text = message ? jabs_string_from_utf8(message) : NULL;\n");
    out.push_str("    if (message && !text) return;\n");
    out.push_str("    switch (kind) {\n");
    for (class, tag) in RAISED {
        let ctor_ok = records.contains(*class)
            && inits.contains(*class)
            && has_method(class, "<init>", "(Ljava/lang/String;)V");
        if ctor_ok {
            let _ = writeln!(
                out,
                "    case {tag}: {{ if ({init}() != 0) return; jabs_object *e = jabs_new(&{record}); if (!e) return; {ctor}(e, text); if (!jabs_pending) jabs_pending = e; return; }}",
                init = init_symbol(class),
                record = class_record(class),
                ctor = symbol(class, "<init>", "(Ljava/lang/String;)V"),
            );
        } else {
            let _ = writeln!(
                out,
                "    case {tag}: (void)text; jabs_not_compiled(\"MethodHandleNatives error {class} is not compiled\");",
            );
        }
    }
    out.push_str(
        "    default: jabs_not_compiled(\"MethodHandleNatives unknown error kind\");\n    }\n}\n",
    );

    // Emit `jabs_mh_box_long` for `getMemberVMInfo`.
    if records.contains(LONG)
        && inits.contains(LONG)
        && has_method(LONG, "valueOf", "(J)Ljava/lang/Long;")
    {
        let _ = writeln!(
            out,
            "static jabs_object *jabs_mh_box_long(int64_t v) {{ if ({}() != 0) return NULL; return {}(v); }}",
            init_symbol(LONG),
            symbol(LONG, "valueOf", "(J)Ljava/lang/Long;"),
        );
    } else {
        out.push_str("static jabs_object *jabs_mh_box_long(int64_t v) { (void)v; jabs_not_compiled(\"MethodHandleNatives.getMemberVMInfo (Long.valueOf not compiled)\"); }\n");
    }

    // Collect CallerSensitive / Hidden flags for recorded classes and their superinterfaces.
    let mut flag_classes: BTreeSet<&str> = records.iter().map(String::as_str).collect();
    for name in records {
        if let Ok(ifaces) = unit.superinterfaces(name) {
            for iface in ifaces {
                flag_classes.insert(&iface.name);
            }
        }
    }
    let mut extra_rows: Vec<(&str, &str, &str, u32)> = Vec::new();
    for class_name in flag_classes {
        let Some(cf) = unit.get(class_name) else {
            continue;
        };
        for m in &cf.methods {
            let mut flags = 0u32;
            if m.annotations
                .iter()
                .any(|a| a == "Ljdk/internal/reflect/CallerSensitive;")
            {
                flags |= MN_CALLER_SENSITIVE;
            }
            if m.annotations
                .iter()
                .any(|a| a == "Ljdk/internal/vm/annotation/Hidden;")
            {
                flags |= MN_HIDDEN_MEMBER;
            }
            if flags != 0 {
                extra_rows.push((class_name, m.name.as_str(), m.descriptor.as_str(), flags));
            }
        }
    }
    extra_rows.sort_unstable();
    out.push_str(
        "typedef struct { const char *cls, *name, *desc; uint32_t flags; } jabs_mh_extra_entry;\n",
    );
    out.push_str("static const jabs_mh_extra_entry jabs_mh_extra_table[] = {\n");
    for (cls, name, desc, flags) in &extra_rows {
        let _ = writeln!(
            out,
            "    {{ \"{}\", \"{}\", \"{}\", 0x{flags:08x}u }},",
            c_escape(cls),
            c_escape(name),
            c_escape(desc),
        );
    }
    out.push_str("    { NULL, NULL, NULL, 0u }\n};\n");

    let _ = write!(
        out,
        r#"#define JABS_MH_MN_IS_METHOD        0x00010000u
#define JABS_MH_MN_IS_CONSTRUCTOR   0x00020000u
#define JABS_MH_MN_IS_FIELD         0x00040000u
#define JABS_MH_MN_IS_TYPE          0x00080000u
#define JABS_MH_MN_CALLER_SENSITIVE 0x00100000u
#define JABS_MH_MN_TRUSTED_FINAL    0x00200000u
#define JABS_MH_LM_PUBLIC           0x0001
#define JABS_MH_LM_PRIVATE          0x0002
#define JABS_MH_LM_PROTECTED        0x0004
#define JABS_MH_LM_PACKAGE          0x0008
#define JABS_MH_LM_UNCONDITIONAL    0x0020
#define JABS_MH_REF_KIND_SHIFT      24u
#define JABS_MH_REF_KIND_MASK       0x0Fu
#define JABS_MH_REF_GET_FIELD       1u
#define JABS_MH_REF_GET_STATIC      2u
#define JABS_MH_REF_PUT_FIELD       3u
#define JABS_MH_REF_PUT_STATIC      4u
#define JABS_MH_REF_INVOKE_VIRTUAL  5u
#define JABS_MH_REF_INVOKE_STATIC   6u
#define JABS_MH_REF_INVOKE_SPECIAL  7u
#define JABS_MH_REF_NEW_SPECIAL     8u
#define JABS_MH_REF_INVOKE_IFACE    9u
#define JABS_MH_METHOD_MODS         0x1DFFu
#define JABS_MH_FIELD_MODS          0x50DFu
#define JABS_MH_ACC_PUBLIC          0x0001u
#define JABS_MH_ACC_PRIVATE         0x0002u
#define JABS_MH_ACC_PROTECTED       0x0004u
#define JABS_MH_ACC_STATIC          0x0008u
#define JABS_MH_ACC_FINAL           0x0010u
#define JABS_MH_ACC_VARARGS         0x0080u
#define JABS_MH_ACC_NATIVE          0x0100u
#define JABS_MH_ACC_INTERFACE       0x0200u
#define JABS_MH_ACC_ABSTRACT        0x0400u
#define JABS_MH_ACC_SYNTHETIC       0x1000u

static int jabs_mh_compare_extra(const void *k, const void *e) {{
    const jabs_mh_extra_entry *a = (const jabs_mh_extra_entry *)k;
    const jabs_mh_extra_entry *b = (const jabs_mh_extra_entry *)e;
    int c = strcmp(a->cls, b->cls);
    if (c != 0) return c;
    c = strcmp(a->name, b->name);
    if (c != 0) return c;
    return strcmp(a->desc, b->desc);
}}

static uint32_t jabs_mh_extra_flags(const char *cls, const char *name, const char *desc) {{
    if (!{extra_count}) return 0u;
    jabs_mh_extra_entry key = {{ cls, name, desc, 0u }};
    const jabs_mh_extra_entry *found = (const jabs_mh_extra_entry *)bsearch(
        &key, jabs_mh_extra_table, {extra_count}, sizeof *found, jabs_mh_compare_extra);
    return found ? found->flags : 0u;
}}

typedef struct {{
    jabs_object *rmn;
    const jabs_rclass *rc;
    const jabs_rmethod *rm;
}} jabs_mh_rmn_entry;

static atomic_flag jabs_mh_rmn_lock = ATOMIC_FLAG_INIT;
static jabs_mh_rmn_entry *jabs_mh_rmn_entries = NULL;
static size_t jabs_mh_rmn_count = 0, jabs_mh_rmn_cap = 0;

static void jabs_mh_rmn_acquire(void) {{
    while (atomic_flag_test_and_set_explicit(&jabs_mh_rmn_lock, memory_order_acquire)) {{}}
}}
static void jabs_mh_rmn_release(void) {{
    atomic_flag_clear_explicit(&jabs_mh_rmn_lock, memory_order_release);
}}

static int jabs_mh_rmn_record(jabs_object *rmn, const jabs_rclass *rc, const jabs_rmethod *rm) {{
    jabs_mh_rmn_acquire();
    if (jabs_mh_rmn_count == jabs_mh_rmn_cap) {{
        size_t grown = jabs_mh_rmn_cap ? jabs_mh_rmn_cap * 2 : 64;
        jabs_mh_rmn_entry *next = (jabs_mh_rmn_entry *)realloc(jabs_mh_rmn_entries, grown * sizeof *next);
        if (!next) {{
            jabs_mh_rmn_release();
            jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
            return -1;
        }}
        jabs_mh_rmn_entries = next;
        jabs_mh_rmn_cap = grown;
    }}
    jabs_mh_rmn_entries[jabs_mh_rmn_count++] = (jabs_mh_rmn_entry){{ rmn, rc, rm }};
    jabs_mh_rmn_release();
    return 0;
}}

static int jabs_mh_rmn_lookup(jabs_object *rmn, const jabs_rclass **out_rc, const jabs_rmethod **out_rm) {{
    if (!rmn) return 0;
    jabs_mh_rmn_acquire();
    for (size_t i = jabs_mh_rmn_count; i > 0; i--) {{
        if (jabs_mh_rmn_entries[i - 1].rmn == rmn) {{
            *out_rc = jabs_mh_rmn_entries[i - 1].rc;
            *out_rm = jabs_mh_rmn_entries[i - 1].rm;
            jabs_mh_rmn_release();
            return 1;
        }}
    }}
    jabs_mh_rmn_release();
    return 0;
}}

void jabs_gc_sweep_mh_roots(jabs_gc_is_marked_fn is_marked) {{
    size_t write_idx = 0;
    for (size_t i = 0; i < jabs_mh_rmn_count; i++) {{
        if (is_marked(jabs_mh_rmn_entries[i].rmn)) {{
            if (write_idx != i) jabs_mh_rmn_entries[write_idx] = jabs_mh_rmn_entries[i];
            write_idx++;
        }}
    }}
    jabs_mh_rmn_count = write_idx;
}}

static jabs_object *jabs_mh_make_rmn(jabs_object *holder, const jabs_rclass *rc, const jabs_rmethod *rm) {{
    if ({rmn_init}() != 0) return NULL;
    jabs_object *rmn = jabs_new(&{rmn_record});
    if (!rmn) return NULL;
    {rmn_ctor}(rmn);
    if (jabs_pending) return NULL;
    (({rmn_struct} *)rmn)->{f_vmholder} = holder;
    if (jabs_mh_rmn_record(rmn, rc, rm) != 0) return NULL;
    return rmn;
}}

static jabs_object *jabs_mh_intern_utf8(const char *s) {{
    jabs_object *str = jabs_string_from_utf8(s);
    return str ? jabs_string_intern(str) : NULL;
}}

static char *jabs_mh_str_utf8(jabs_object *s) {{
    char *out = jabs_string_to_utf8(s);
    if (!out && !jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
    return out;
}}

typedef struct {{
    char *data;
    size_t len, cap;
}} jabs_mh_buf;

static int jabs_mh_buf_append(jabs_mh_buf *b, const char *s, size_t n) {{
    if (b->len + n + 1 > b->cap) {{
        size_t grown = b->cap ? b->cap * 2 : 128;
        while (grown < b->len + n + 1) grown *= 2;
        char *next = (char *)realloc(b->data, grown);
        if (!next) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return -1; }}
        b->data = next;
        b->cap = grown;
    }}
    memcpy(b->data + b->len, s, n);
    b->len += n;
    b->data[b->len] = 0;
    return 0;
}}

static int jabs_mh_append_class_desc(jabs_mh_buf *b, jabs_object *cls_obj) {{
    static const char *const prims[] = {{
        "boolean", "Z", "byte", "B", "char", "C", "short", "S",
        "int", "I", "long", "J", "float", "F", "double", "D", "void", "V"
    }};
    if (!cls_obj) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "null Class in MemberName.type"); return -1; }}
    const char *name = jabs_reflect_class_name(cls_obj);
    if (!name) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "unknown Class in MemberName.type"); return -1; }}
    for (size_t i = 0; i < sizeof prims / sizeof prims[0]; i += 2) {{
        if (strcmp(name, prims[i]) == 0) return jabs_mh_buf_append(b, prims[i + 1], 1);
    }}
    if (name[0] == '[') return jabs_mh_buf_append(b, name, strlen(name));
    if (jabs_mh_buf_append(b, "L", 1) != 0) return -1;
    if (jabs_mh_buf_append(b, name, strlen(name)) != 0) return -1;
    return jabs_mh_buf_append(b, ";", 1);
}}

static int jabs_mh_append_method_desc(jabs_mh_buf *b, jabs_object *rtype, jabs_object *ptypes_obj) {{
    if (!ptypes_obj || ptypes_obj->cls != jabs_array_class()) {{
        jabs_mh_raise(JABS_MH_ERR_INTERNAL, "MemberName parameter types is not an array");
        return -1;
    }}
    jabs_array *ptypes = (jabs_array *)ptypes_obj;
    if (!jabs_array_is_ref(ptypes)) {{
        jabs_mh_raise(JABS_MH_ERR_INTERNAL, "MemberName parameter types is primitive array");
        return -1;
    }}
    if (jabs_mh_buf_append(b, "(", 1) != 0) return -1;
    jabs_object **params = (jabs_object **)jabs_array_data(ptypes);
    for (int32_t i = 0; i < ptypes->length; i++) {{
        if (jabs_mh_append_class_desc(b, params[i]) != 0) return -1;
    }}
    if (jabs_mh_buf_append(b, ")", 1) != 0) return -1;
    return jabs_mh_append_class_desc(b, rtype);
}}

static char *jabs_mh_type_to_desc(jabs_object *type_obj) {{
    if (!type_obj) return NULL;
    if (jabs_class_assignable(type_obj->cls, &{str_record})) {{
        return jabs_mh_str_utf8(type_obj);
    }}
    jabs_mh_buf b = {{ NULL, 0, 0 }};
    if (jabs_class_assignable(type_obj->cls, &{cls_record})) {{
        if (jabs_mh_append_class_desc(&b, type_obj) != 0) {{ free(b.data); return NULL; }}
        return b.data;
    }}
    if (jabs_class_assignable(type_obj->cls, &{mt_record})) {{
        {mt_struct} *mt = ({mt_struct} *)type_obj;
        if (jabs_mh_append_method_desc(&b, mt->{f_rtype}, mt->{f_ptypes}) != 0) {{ free(b.data); return NULL; }}
        return b.data;
    }}
    if (type_obj->cls == jabs_array_class()) {{
        jabs_array *arr = (jabs_array *)type_obj;
        if (jabs_array_is_ref(arr) && arr->length >= 2) {{
            jabs_object **slots = (jabs_object **)jabs_array_data(arr);
            if (jabs_mh_append_method_desc(&b, slots[0], slots[1]) != 0) {{ free(b.data); return NULL; }}
            return b.data;
        }}
    }}
    free(b.data);
    jabs_mh_raise(JABS_MH_ERR_INTERNAL, "unsupported MemberName.type");
    return NULL;
}}

static const jabs_rfield *jabs_mh_find_field_in(const jabs_rclass *rc, const char *name, const char *desc, const jabs_rclass **out_rc) {{
    if (!rc) return NULL;
    for (uint32_t i = 0; i < rc->field_count; i++) {{
        const jabs_rfield *rf = &jabs_rfields[rc->fields + i];
        if (strcmp(jabs_rstrings + rf->name, name) == 0
            && (!desc || strcmp(jabs_rstrings + rf->descriptor, desc) == 0)) {{
            *out_rc = rc;
            return rf;
        }}
    }}
    uint32_t if_list = rc->interfaces;
    uint32_t if_count = if_list ? jabs_rlists[if_list] : 0;
    for (uint32_t i = 0; i < if_count; i++) {{
        const jabs_rclass *ic = jabs_reflect_info_named(jabs_rstrings + jabs_rlists[if_list + 1 + i]);
        const jabs_rfield *rf = jabs_mh_find_field_in(ic, name, desc, out_rc);
        if (rf) return rf;
    }}
    const jabs_class *rec = jabs_reflect_class_record(jabs_class_object(jabs_rstrings + rc->name));
    if (rec && rec->super) {{
        return jabs_mh_find_field_in(jabs_reflect_info_named(rec->super->name), name, desc, out_rc);
    }}
    return NULL;
}}

static const jabs_rmethod *jabs_mh_declared_method(const jabs_rclass *rc, const char *name, const char *desc) {{
    if (!rc) return NULL;
    for (uint32_t i = 0; i < rc->method_count; i++) {{
        const jabs_rmethod *rm = &jabs_rmethods[rc->methods + i];
        if (strcmp(jabs_rstrings + rm->name, name) == 0
            && strcmp(jabs_rstrings + rm->descriptor, desc) == 0) {{
            return rm;
        }}
    }}
    return NULL;
}}

/* Recursively search superinterfaces; prefers a concrete (non-abstract) default method over an abstract one. */
static const jabs_rmethod *jabs_mh_find_iface_method(const jabs_rclass *rc, const char *name, const char *desc, const jabs_rclass **out_rc) {{
    if (!rc) return NULL;
    const jabs_rmethod *best = NULL;
    const jabs_rclass *best_rc = NULL;
    if (rc->access & JABS_MH_ACC_INTERFACE) {{
        const jabs_rmethod *rm = jabs_mh_declared_method(rc, name, desc);
        if (rm && !(rm->access & JABS_MH_ACC_STATIC) && !(rm->access & JABS_MH_ACC_PRIVATE)) {{
            if (!(rm->access & JABS_MH_ACC_ABSTRACT)) {{
                *out_rc = rc;
                return rm;
            }}
            best = rm;
            best_rc = rc;
        }}
    }}
    uint32_t if_list = rc->interfaces;
    uint32_t if_count = if_list ? jabs_rlists[if_list] : 0;
    for (uint32_t i = 0; i < if_count; i++) {{
        const jabs_rclass *ic = jabs_reflect_info_named(jabs_rstrings + jabs_rlists[if_list + 1 + i]);
        const jabs_rclass *sub_rc = NULL;
        const jabs_rmethod *sub_rm = jabs_mh_find_iface_method(ic, name, desc, &sub_rc);
        if (sub_rm) {{
            if (!(sub_rm->access & JABS_MH_ACC_ABSTRACT)) {{
                *out_rc = sub_rc;
                return sub_rm;
            }}
            if (!best) {{ best = sub_rm; best_rc = sub_rc; }}
        }}
    }}
    if (best) *out_rc = best_rc;
    return best;
}}

static const jabs_rmethod *jabs_mh_find_class_method(const jabs_rclass *rc, const char *name, const char *desc, uint32_t ref_kind, const jabs_rclass **out_rc) {{
    const jabs_rclass *cur = rc;
    while (cur) {{
        const jabs_rmethod *rm = jabs_mh_declared_method(cur, name, desc);
        if (rm) {{
            *out_rc = cur;
            return rm;
        }}
        const jabs_class *rec = jabs_reflect_class_record(jabs_class_object(jabs_rstrings + cur->name));
        cur = (rec && rec->super) ? jabs_reflect_info_named(rec->super->name) : NULL;
    }}
    /* emit_class_record omits super (sets NULL) on direct subclasses of java/lang/Object. */
    if (strcmp(name, "<init>") != 0 && strcmp(jabs_rstrings + rc->name, "{OBJECT}") != 0) {{
        const jabs_rclass *obj_rc = jabs_reflect_info_named("{OBJECT}");
        const jabs_rmethod *obj_rm = jabs_mh_declared_method(obj_rc, name, desc);
        if (obj_rm) {{
            *out_rc = obj_rc;
            return obj_rm;
        }}
    }}
    if (ref_kind == JABS_MH_REF_INVOKE_STATIC) return NULL;
    cur = rc;
    const jabs_rmethod *best = NULL;
    const jabs_rclass *best_rc = NULL;
    while (cur) {{
        const jabs_rclass *sub_rc = NULL;
        const jabs_rmethod *sub_rm = jabs_mh_find_iface_method(cur, name, desc, &sub_rc);
        if (sub_rm) {{
            if (!(sub_rm->access & JABS_MH_ACC_ABSTRACT)) {{
                *out_rc = sub_rc;
                return sub_rm;
            }}
            if (!best) {{ best = sub_rm; best_rc = sub_rc; }}
        }}
        const jabs_class *rec = jabs_reflect_class_record(jabs_class_object(jabs_rstrings + cur->name));
        cur = (rec && rec->super) ? jabs_reflect_info_named(rec->super->name) : NULL;
    }}
    if (best) *out_rc = best_rc;
    return best;
}}

static int jabs_mh_same_package(const char *a, const char *b) {{
    const char *sa = strrchr(a, '/');
    const char *sb = strrchr(b, '/');
    size_t la = sa ? (size_t)(sa - a) : 0;
    size_t lb = sb ? (size_t)(sb - b) : 0;
    if (la != lb) return 0;
    return la == 0 || memcmp(a, b, la) == 0;
}}

static int jabs_mh_class_accessible(jabs_object *caller_cls, const char *caller_name, int32_t lookup_mode, const jabs_rclass *rc) {{
    if (!rc) return 0;
    if (rc->access & JABS_MH_ACC_PUBLIC) return 1;
    const char *cname = jabs_rstrings + rc->name;
    if (strcmp(caller_name, cname) == 0 && (lookup_mode & (JABS_MH_LM_PRIVATE | JABS_MH_LM_PACKAGE | JABS_MH_LM_PROTECTED | JABS_MH_LM_PUBLIC))) return 1;
    if ((lookup_mode & JABS_MH_LM_UNCONDITIONAL) && !(lookup_mode & (JABS_MH_LM_PACKAGE | JABS_MH_LM_PRIVATE))) return 0;
    jabs_object *cls = jabs_class_object(cname);
    if (!cls) return -1;
    int32_t same_nest = jabs_reflect_nest_mates(caller_cls, cls);
    if (jabs_pending) return -1;
    if (same_nest && (lookup_mode & JABS_MH_LM_PRIVATE)) return 1;
    if (jabs_mh_same_package(caller_name, cname) && (lookup_mode & JABS_MH_LM_PACKAGE)) return 1;
    return 0;
}}

static int jabs_mh_check_access(jabs_object *caller_cls, int32_t lookup_mode, const jabs_rclass *sym_rc, const jabs_rclass *def_rc, uint16_t member_access, int32_t speculative) {{
    if (!caller_cls) return 0;
    const char *caller_name = jabs_reflect_class_name(caller_cls);
    const char *def_name = jabs_rstrings + def_rc->name;
    if (!caller_name) return 0;
    if ((lookup_mode & (JABS_MH_LM_PUBLIC | JABS_MH_LM_PRIVATE | JABS_MH_LM_PROTECTED | JABS_MH_LM_PACKAGE | JABS_MH_LM_UNCONDITIONAL)) == 0) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_ILLEGAL_ACCESS, def_name);
        return -1;
    }}
    int ok = 1;
    if (sym_rc) {{
        int sym_ok = jabs_mh_class_accessible(caller_cls, caller_name, lookup_mode, sym_rc);
        if (sym_ok < 0) return -1;
        if (!sym_ok) ok = 0;
    }}
    if (ok && (!(member_access & JABS_MH_ACC_PUBLIC) || !sym_rc)) {{
        int def_ok = jabs_mh_class_accessible(caller_cls, caller_name, lookup_mode, def_rc);
        if (def_ok < 0) return -1;
        if (!def_ok) ok = 0;
    }}
    if (ok) {{
        jabs_object *def_cls = jabs_class_object(def_name);
        if (!def_cls) return -1;
        int32_t same_nest = jabs_reflect_nest_mates(caller_cls, def_cls);
        if (jabs_pending) return -1;
        int same_pkg = jabs_mh_same_package(caller_name, def_name);
        int pkg_ok = same_pkg && ((lookup_mode & JABS_MH_LM_PACKAGE) != 0);
        if (strcmp(caller_name, def_name) == 0 && (lookup_mode & JABS_MH_LM_PRIVATE)) {{
            ok = 1;
        }} else if (member_access & JABS_MH_ACC_PUBLIC) {{
            ok = (lookup_mode & (JABS_MH_LM_PUBLIC | JABS_MH_LM_UNCONDITIONAL | JABS_MH_LM_PACKAGE | JABS_MH_LM_PRIVATE)) != 0;
        }} else if (member_access & JABS_MH_ACC_PRIVATE) {{
            ok = (lookup_mode & JABS_MH_LM_PRIVATE) != 0 && same_nest;
        }} else if (member_access & JABS_MH_ACC_PROTECTED) {{
            if (pkg_ok) {{
                ok = 1;
            }} else if ((lookup_mode & JABS_MH_LM_PROTECTED) != 0 && jabs_class_assignable_from(def_cls, caller_cls)) {{
                if (member_access & JABS_MH_ACC_STATIC) {{
                    ok = 1;
                }} else if (sym_rc) {{
                    jabs_object *sym_cls = jabs_class_object(jabs_rstrings + sym_rc->name);
                    if (!sym_cls) return -1;
                    ok = jabs_class_assignable_from(caller_cls, sym_cls) || jabs_class_assignable_from(sym_cls, caller_cls);
                }} else {{
                    ok = 0;
                }}
            }} else {{
                ok = 0;
            }}
        }} else {{
            ok = pkg_ok;
        }}
    }}
    if (!ok) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_ILLEGAL_ACCESS, def_name);
        return -1;
    }}
    return 0;
}}

static uint32_t jabs_mh_method_ref_kind(const jabs_rclass *rc, const jabs_rmethod *rm) {{
    if (rm->access & JABS_MH_ACC_STATIC) return JABS_MH_REF_INVOKE_STATIC;
    if ((rm->access & (JABS_MH_ACC_PRIVATE | JABS_MH_ACC_FINAL)) || (rc->access & JABS_MH_ACC_FINAL)) {{
        return JABS_MH_REF_INVOKE_SPECIAL;
    }}
    if (rc->access & JABS_MH_ACC_INTERFACE) return JABS_MH_REF_INVOKE_IFACE;
    return JABS_MH_REF_INVOKE_VIRTUAL;
}}

void jabs_mh_init(jabs_object *mn_obj, jabs_object *target) {{
    if (!mn_obj || !target) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (!jabs_class_assignable(mn_obj->cls, &{mn_record})) jabs_not_compiled("MethodHandleNatives.init receiver is not a MemberName");
    {mn_struct} *mn = ({mn_struct} *)mn_obj;
    if (jabs_class_assignable(target->cls, &{rm_record})) {{
        {rm_struct} *m = ({rm_struct} *)target;
        jabs_object *holder = m->{f_rm_clazz};
        if (!holder) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "Method.clazz is null"); return; }}
        const char *cls_name = jabs_reflect_class_name(holder);
        if (!cls_name) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "unknown Method.clazz"); return; }}
        const jabs_rclass *rc = jabs_reflect_info_named(cls_name);
        int32_t slot = m->{f_rm_slot};
        if (!rc || slot < 0 || (uint32_t)slot >= rc->method_count) {{
            jabs_mh_raise(JABS_MH_ERR_INTERNAL, "invalid Method slot");
            return;
        }}
        const jabs_rmethod *rm = &jabs_rmethods[rc->methods + (uint32_t)slot];
        const char *mname = jabs_rstrings + rm->name;
        if ((strcmp(cls_name, "{METHOD_HANDLE}") == 0 && (strcmp(mname, "invoke") == 0 || strcmp(mname, "invokeExact") == 0))
            || (strcmp(cls_name, "{VAR_HANDLE}") == 0 && (rm->access & (JABS_MH_ACC_NATIVE | JABS_MH_ACC_VARARGS)) == (JABS_MH_ACC_NATIVE | JABS_MH_ACC_VARARGS))) {{
            return;
        }}
        uint32_t ref_kind = jabs_mh_method_ref_kind(rc, rm);
        uint32_t extra = jabs_mh_extra_flags(cls_name, mname, jabs_rstrings + rm->descriptor);
        uint32_t flags = (ref_kind << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_METHOD
            | ((uint32_t)rm->access & JABS_MH_METHOD_MODS) | extra;
        jabs_object *rmn = jabs_mh_make_rmn(holder, rc, rm);
        if (!rmn) return;
        mn->{f_mn_clazz} = holder;
        mn->{f_mn_flags} = (int32_t)flags;
        mn->{f_mn_method} = rmn;
        return;
    }}
    if (jabs_class_assignable(target->cls, &{rc_record})) {{
        {rc_struct} *c = ({rc_struct} *)target;
        jabs_object *holder = c->{f_rc_clazz};
        if (!holder) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "Constructor.clazz is null"); return; }}
        const char *cls_name = jabs_reflect_class_name(holder);
        const jabs_rclass *rc = cls_name ? jabs_reflect_info_named(cls_name) : NULL;
        int32_t slot = c->{f_rc_slot};
        if (!rc || slot < 0 || (uint32_t)slot >= rc->method_count) {{
            jabs_mh_raise(JABS_MH_ERR_INTERNAL, "invalid Constructor slot");
            return;
        }}
        const jabs_rmethod *rm = &jabs_rmethods[rc->methods + (uint32_t)slot];
        uint32_t extra = jabs_mh_extra_flags(cls_name, jabs_rstrings + rm->name, jabs_rstrings + rm->descriptor);
        uint32_t flags = (JABS_MH_REF_INVOKE_SPECIAL << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_CONSTRUCTOR
            | ((uint32_t)rm->access & JABS_MH_METHOD_MODS) | extra;
        jabs_object *rmn = jabs_mh_make_rmn(holder, rc, rm);
        if (!rmn) return;
        mn->{f_mn_clazz} = holder;
        mn->{f_mn_flags} = (int32_t)flags;
        mn->{f_mn_method} = rmn;
        return;
    }}
    if (jabs_class_assignable(target->cls, &{rf_record})) {{
        {rf_struct} *f = ({rf_struct} *)target;
        jabs_object *holder = f->{f_rf_clazz};
        if (!holder) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "Field.clazz is null"); return; }}
        const char *cls_name = jabs_reflect_class_name(holder);
        const jabs_rclass *rc = cls_name ? jabs_reflect_info_named(cls_name) : NULL;
        int32_t slot = f->{f_rf_slot};
        if (!rc || slot < 0 || (uint32_t)slot >= rc->field_count) {{
            jabs_mh_raise(JABS_MH_ERR_INTERNAL, "invalid Field slot");
            return;
        }}
        const jabs_rfield *rf = &jabs_rfields[rc->fields + (uint32_t)slot];
        uint32_t ref_kind = (rf->access & JABS_MH_ACC_STATIC) ? JABS_MH_REF_GET_STATIC : JABS_MH_REF_GET_FIELD;
        uint32_t trusted = ((rf->access & JABS_MH_ACC_FINAL) && ((rf->access & JABS_MH_ACC_STATIC) || rc->is_record))
            ? JABS_MH_MN_TRUSTED_FINAL : 0u;
        uint32_t flags = (ref_kind << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_FIELD
            | ((uint32_t)rf->access & JABS_MH_FIELD_MODS) | trusted;
        mn->{f_mn_clazz} = holder;
        mn->{f_mn_flags} = (int32_t)flags;
        mn->{f_mn_method} = NULL;
        return;
    }}
    jabs_mh_raise(JABS_MH_ERR_INTERNAL, "MethodHandleNatives.init: unsupported target");
}}

void jabs_mh_expand(jabs_object *mn_obj) {{
    if (!mn_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (!jabs_class_assignable(mn_obj->cls, &{mn_record})) jabs_not_compiled("MethodHandleNatives.expand receiver is not a MemberName");
    {mn_struct} *mn = ({mn_struct} *)mn_obj;
    const jabs_rclass *rc = NULL;
    const jabs_rmethod *rm = NULL;
    if (jabs_mh_rmn_lookup(mn->{f_mn_method}, &rc, &rm)) {{
        if (!mn->{f_mn_clazz}) {{
            jabs_object *holder = (({rmn_struct} *)mn->{f_mn_method})->{f_vmholder};
            mn->{f_mn_clazz} = holder ? holder : jabs_class_object(jabs_rstrings + rc->name);
            if (!mn->{f_mn_clazz}) return;
        }}
        if (!mn->{f_mn_name}) {{
            mn->{f_mn_name} = jabs_mh_intern_utf8(jabs_rstrings + rm->name);
            if (!mn->{f_mn_name}) return;
        }}
        if (!mn->{f_mn_type}) {{
            mn->{f_mn_type} = jabs_string_from_utf8(jabs_rstrings + rm->descriptor);
        }}
        return;
    }}
    if (((uint32_t)mn->{f_mn_flags} & JABS_MH_MN_IS_FIELD) && mn->{f_mn_clazz} && mn->{f_mn_name}) {{
        if (!mn->{f_mn_type}) {{
            const char *cname = jabs_reflect_class_name(mn->{f_mn_clazz});
            char *fname = jabs_mh_str_utf8(mn->{f_mn_name});
            if (!fname) return;
            const jabs_rclass *frc = cname ? jabs_reflect_info_named(cname) : NULL;
            const jabs_rclass *found_rc = NULL;
            const jabs_rfield *rf = jabs_mh_find_field_in(frc, fname, NULL, &found_rc);
            free(fname);
            if (!rf) {{ jabs_mh_raise(JABS_MH_ERR_INTERNAL, "MemberName.expand field not found"); return; }}
            mn->{f_mn_type} = jabs_string_from_utf8(jabs_rstrings + rf->descriptor);
        }}
        return;
    }}
    jabs_mh_raise(JABS_MH_ERR_INTERNAL, "MemberName.expand: missing VM target");
}}

static const jabs_rfield *jabs_mh_resolve_field_row(jabs_object *mn_obj, int want_static, const char *what) {{
    if (!mn_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (!jabs_class_assignable(mn_obj->cls, &{mn_record})) jabs_not_compiled(what);
    {mn_struct} *mn = ({mn_struct} *)mn_obj;
    uint32_t flags = (uint32_t)mn->{f_mn_flags};
    if (!(flags & JABS_MH_MN_IS_FIELD) || mn->{f_mn_resolution} != NULL
        || (want_static ? !(flags & JABS_MH_ACC_STATIC) : (flags & JABS_MH_ACC_STATIC) != 0)) {{
        jabs_mh_raise(JABS_MH_ERR_INTERNAL, what);
        return NULL;
    }}
    const char *cname = mn->{f_mn_clazz} ? jabs_reflect_class_name(mn->{f_mn_clazz}) : NULL;
    const jabs_rclass *rc = cname ? jabs_reflect_info_named(cname) : NULL;
    if (!mn->{f_mn_name}) {{
        jabs_mh_raise(JABS_MH_ERR_INTERNAL, what);
        return NULL;
    }}
    char *fname = jabs_mh_str_utf8(mn->{f_mn_name});
    if (!fname) return NULL;
    char *fdesc = mn->{f_mn_type} ? jabs_mh_type_to_desc(mn->{f_mn_type}) : NULL;
    if (mn->{f_mn_type} && !fdesc) {{ free(fname); return NULL; }}
    const jabs_rfield *found = NULL;
    if (rc) {{
        for (uint32_t i = 0; i < rc->field_count; i++) {{
            const jabs_rfield *rf = &jabs_rfields[rc->fields + i];
            int rf_static = (rf->access & JABS_MH_ACC_STATIC) != 0;
            if (rf_static == want_static
                && strcmp(jabs_rstrings + rf->name, fname) == 0
                && (!fdesc || strcmp(jabs_rstrings + rf->descriptor, fdesc) == 0)) {{
                found = rf;
                break;
            }}
        }}
    }}
    free(fname);
    free(fdesc);
    if (!found) jabs_mh_raise(JABS_MH_ERR_INTERNAL, what);
    return found;
}}

int64_t jabs_mh_object_field_offset(jabs_object *mn_obj) {{
    const jabs_rfield *rf = jabs_mh_resolve_field_row(mn_obj, 0, "MethodHandleNatives.objectFieldOffset");
    return rf ? (int64_t)rf->offset : 0;
}}

int64_t jabs_mh_static_field_offset(jabs_object *mn_obj) {{
    const jabs_rfield *rf = jabs_mh_resolve_field_row(mn_obj, 1, "MethodHandleNatives.staticFieldOffset");
    if (!rf) return 0;
    if (!rf->address) jabs_not_compiled("MethodHandleNatives.staticFieldOffset on a static field whose class statics are not compiled");
    return (int64_t)(intptr_t)rf->address;
}}

jabs_object *jabs_mh_static_field_base(jabs_object *mn_obj) {{
    const jabs_rfield *rf = jabs_mh_resolve_field_row(mn_obj, 1, "MethodHandleNatives.staticFieldBase");
    if (!rf) return NULL;
    if (!rf->address) jabs_not_compiled("MethodHandleNatives.staticFieldBase on a static field whose class statics are not compiled");
    return NULL;
}}

jabs_object *jabs_mh_get_member_vm_info(jabs_object *mn_obj) {{
    if (!mn_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (!jabs_class_assignable(mn_obj->cls, &{mn_record})) jabs_not_compiled("MethodHandleNatives.getMemberVMInfo receiver is not a MemberName");
    {mn_struct} *mn = ({mn_struct} *)mn_obj;
    uint32_t flags = (uint32_t)mn->{f_mn_flags};
    uint32_t ref_kind = (flags >> JABS_MH_REF_KIND_SHIFT) & JABS_MH_REF_KIND_MASK;
    int64_t vmindex = 0;
    jabs_object *vmtarget = NULL;
    if (mn->{f_mn_resolution} != NULL || ref_kind == 0u || (flags & JABS_MH_MN_IS_TYPE) != 0u) {{
        vmindex = 0;
        vmtarget = mn_obj;
    }} else if (ref_kind >= JABS_MH_REF_GET_FIELD && ref_kind <= JABS_MH_REF_PUT_STATIC) {{
        vmindex = (ref_kind == JABS_MH_REF_GET_STATIC || ref_kind == JABS_MH_REF_PUT_STATIC)
            ? jabs_mh_static_field_offset(mn_obj)
            : jabs_mh_object_field_offset(mn_obj);
        if (jabs_pending) return NULL;
        vmtarget = mn->{f_mn_clazz};
    }} else {{
        if (ref_kind == JABS_MH_REF_INVOKE_VIRTUAL) {{
            const jabs_rclass *rc = NULL;
            const jabs_rmethod *rm = NULL;
            if (jabs_mh_rmn_lookup(mn->{f_mn_method}, &rc, &rm) && rm->vtable_slot >= 0) {{
                vmindex = (int64_t)rm->vtable_slot;
            }} else {{
                vmindex = 0;
            }}
        }} else if (ref_kind == JABS_MH_REF_INVOKE_IFACE) {{
            vmindex = 0;
        }} else {{
            vmindex = -2;
        }}
        vmtarget = mn_obj;
    }}
    jabs_object *boxed = jabs_mh_box_long(vmindex);
    if (!boxed) return NULL;
    jabs_object *pair = jabs_new_array(2, sizeof(jabs_object *), JABS_COMPONENT_ANY, NULL, "[Ljava/lang/Object;", NULL);
    if (!pair) return NULL;
    jabs_object **slots = (jabs_object **)jabs_array_data((jabs_array *)pair);
    slots[0] = boxed;
    slots[1] = vmtarget;
    return pair;
}}

jabs_object *jabs_mh_resolve(jabs_object *mn_obj, jabs_object *caller_cls, int32_t lookup_mode, int32_t speculative) {{
    if (!mn_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (!jabs_class_assignable(mn_obj->cls, &{mn_record})) jabs_not_compiled("MethodHandleNatives.resolve receiver is not a MemberName");
    {mn_struct} *mn = ({mn_struct} *)mn_obj;
    jabs_object *clazz = mn->{f_mn_clazz};
    jabs_object *name_obj = mn->{f_mn_name};
    jabs_object *type_obj = mn->{f_mn_type};
    uint32_t ref_kind = ((uint32_t)mn->{f_mn_flags} >> JABS_MH_REF_KIND_SHIFT) & JABS_MH_REF_KIND_MASK;
    if (!clazz || !name_obj || !type_obj || ref_kind == 0u || ref_kind >= 10u) {{
        jabs_mh_raise(JABS_MH_ERR_ILLEGAL_ARGUMENT, "incomplete or invalid MemberName");
        return NULL;
    }}
    const char *cname = jabs_reflect_class_name(clazz);
    if (!cname) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_INTERNAL, "unknown MemberName.clazz");
        return NULL;
    }}
    char *mname = jabs_mh_str_utf8(name_obj);
    if (!mname) return NULL;
    char *mdesc = jabs_mh_type_to_desc(type_obj);
    if (!mdesc) {{ free(mname); return NULL; }}

    jabs_object *result = NULL;
    if (jabs_is_primitive_name(cname)) {{
        if (!speculative) jabs_mh_raise(ref_kind <= JABS_MH_REF_PUT_STATIC ? JABS_MH_ERR_NO_SUCH_FIELD : JABS_MH_ERR_NO_SUCH_METHOD, mname);
        goto done;
    }}

    if (ref_kind <= JABS_MH_REF_PUT_STATIC) {{
        if (cname[0] == '[' || mdesc[0] == '(' || strcmp(mdesc, "V") == 0) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_FIELD, mname);
            goto done;
        }}
        const jabs_rclass *rc = jabs_reflect_info_named(cname);
        if (!rc) jabs_not_compiled_class("MethodHandleNatives.resolve field on class without metadata", cname);
        const jabs_rclass *found_rc = NULL;
        const jabs_rfield *rf = jabs_mh_find_field_in(rc, mname, mdesc, &found_rc);
        if (!rf) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_FIELD, mname);
            goto done;
        }}
        int is_static = (rf->access & JABS_MH_ACC_STATIC) != 0;
        int is_setter = (ref_kind == JABS_MH_REF_PUT_FIELD || ref_kind == JABS_MH_REF_PUT_STATIC);
        uint32_t out_kind = is_static
            ? (is_setter ? JABS_MH_REF_PUT_STATIC : JABS_MH_REF_GET_STATIC)
            : (is_setter ? JABS_MH_REF_PUT_FIELD : JABS_MH_REF_GET_FIELD);
        if (jabs_mh_check_access(caller_cls, lookup_mode, rc, found_rc, rf->access, speculative) != 0) goto done;
        jabs_object *decl_cls = jabs_class_object(jabs_rstrings + found_rc->name);
        if (!decl_cls) goto done;
        uint32_t trusted = ((rf->access & JABS_MH_ACC_FINAL) && (is_static || found_rc->is_record)) ? JABS_MH_MN_TRUSTED_FINAL : 0u;
        uint32_t out_flags = (out_kind << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_FIELD
            | ((uint32_t)rf->access & JABS_MH_FIELD_MODS) | trusted;
        mn->{f_mn_clazz} = decl_cls;
        mn->{f_mn_flags} = (int32_t)out_flags;
        mn->{f_mn_method} = NULL;
        result = mn_obj;
        goto done;
    }}

    if (cname[0] == '[') {{
        if (ref_kind == JABS_MH_REF_INVOKE_IFACE) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_ICCE, "array is not an interface");
            goto done;
        }}
        if (ref_kind == JABS_MH_REF_NEW_SPECIAL || strcmp(mname, "<init>") == 0) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_METHOD, mname);
            goto done;
        }}
        cname = "{OBJECT}";
        clazz = jabs_class_object(cname);
        if (!clazz) goto done;
    }}

    const jabs_rclass *rc = jabs_reflect_info_named(cname);
    if (!rc) jabs_not_compiled_class("MethodHandleNatives.resolve method on class without metadata", cname);

    if (ref_kind == JABS_MH_REF_NEW_SPECIAL || strcmp(mname, "<init>") == 0) {{
        if ((ref_kind != JABS_MH_REF_NEW_SPECIAL && ref_kind != JABS_MH_REF_INVOKE_SPECIAL)
            || strcmp(mname, "<init>") != 0 || (rc->access & JABS_MH_ACC_INTERFACE)) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_METHOD, mname);
            goto done;
        }}
        const jabs_rmethod *rm = jabs_mh_declared_method(rc, "<init>", mdesc);
        if (!rm) {{
            if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_METHOD, mname);
            goto done;
        }}
        if (jabs_mh_check_access(caller_cls, lookup_mode, rc, rc, rm->access, speculative) != 0) goto done;
        uint32_t extra = jabs_mh_extra_flags(cname, "<init>", mdesc);
        uint32_t out_flags = (JABS_MH_REF_INVOKE_SPECIAL << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_CONSTRUCTOR
            | ((uint32_t)rm->access & JABS_MH_METHOD_MODS) | extra;
        jabs_object *rmn = jabs_mh_make_rmn(clazz, rc, rm);
        if (!rmn) goto done;
        mn->{f_mn_clazz} = clazz;
        mn->{f_mn_flags} = (int32_t)out_flags;
        mn->{f_mn_method} = rmn;
        result = mn_obj;
        goto done;
    }}

    if (strcmp(mname, "<clinit>") == 0) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_METHOD, mname);
        goto done;
    }}

    if (ref_kind == JABS_MH_REF_INVOKE_IFACE && !(rc->access & JABS_MH_ACC_INTERFACE)) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_ICCE, "Found class, but interface was expected");
        goto done;
    }}
    if (ref_kind == JABS_MH_REF_INVOKE_VIRTUAL && (rc->access & JABS_MH_ACC_INTERFACE)) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_ICCE, "Found interface, but class was expected");
        goto done;
    }}

    /* MethodHandle / VarHandle signature-polymorphic VM linkers (linkTo*, invokeBasic) accept any MethodType. */
    if (strcmp(cname, "{METHOD_HANDLE}") == 0 || strcmp(cname, "{VAR_HANDLE}") == 0) {{
        for (uint32_t i = 0; i < rc->method_count; i++) {{
            const jabs_rmethod *rm = &jabs_rmethods[rc->methods + i];
            if (strcmp(jabs_rstrings + rm->name, mname) == 0
                && (rm->access & (JABS_MH_ACC_NATIVE | JABS_MH_ACC_VARARGS)) == (JABS_MH_ACC_NATIVE | JABS_MH_ACC_VARARGS)) {{
                int is_appendix_poly = strcmp(cname, "{VAR_HANDLE}") == 0
                    || strcmp(mname, "invoke") == 0
                    || strcmp(mname, "invokeExact") == 0;
                if (is_appendix_poly) {{
                    if (ref_kind != JABS_MH_REF_INVOKE_VIRTUAL) break;
                    if (!speculative) jabs_mh_raise(JABS_MH_ERR_INTERNAL, "appendix");
                    goto done;
                }}
                int want_static = (ref_kind == JABS_MH_REF_INVOKE_STATIC);
                int is_static = (rm->access & JABS_MH_ACC_STATIC) != 0;
                if (want_static != is_static) {{
                    if (!speculative) jabs_mh_raise(JABS_MH_ERR_ICCE, mname);
                    goto done;
                }}
                if (jabs_mh_check_access(caller_cls, lookup_mode, rc, rc, rm->access, speculative) != 0) goto done;
                uint32_t out_kind = is_static ? JABS_MH_REF_INVOKE_STATIC : JABS_MH_REF_INVOKE_SPECIAL;
                uint32_t out_flags = (out_kind << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_METHOD
                    | (((uint32_t)rm->access & JABS_MH_METHOD_MODS & ~JABS_MH_ACC_VARARGS) | JABS_MH_ACC_SYNTHETIC);
                jabs_object *rmn = jabs_mh_make_rmn(clazz, rc, rm);
                if (!rmn) goto done;
                mn->{f_mn_clazz} = clazz;
                mn->{f_mn_flags} = (int32_t)out_flags;
                mn->{f_mn_method} = rmn;
                result = mn_obj;
                goto done;
            }}
        }}
    }}

    const jabs_rclass *found_rc = NULL;
    const jabs_rmethod *found_rm = NULL;
    if (rc->access & JABS_MH_ACC_INTERFACE) {{
        found_rm = jabs_mh_declared_method(rc, mname, mdesc);
        if (found_rm) {{
            found_rc = rc;
        }} else if (ref_kind != JABS_MH_REF_INVOKE_STATIC) {{
            const jabs_rclass *obj_rc = jabs_reflect_info_named("{OBJECT}");
            const jabs_rmethod *obj_rm = jabs_mh_declared_method(obj_rc, mname, mdesc);
            if (obj_rm && (obj_rm->access & JABS_MH_ACC_PUBLIC) && !(obj_rm->access & JABS_MH_ACC_STATIC)) {{
                found_rc = obj_rc;
                found_rm = obj_rm;
            }} else {{
                found_rm = jabs_mh_find_iface_method(rc, mname, mdesc, &found_rc);
            }}
        }}
    }} else {{
        found_rm = jabs_mh_find_class_method(rc, mname, mdesc, ref_kind, &found_rc);
    }}

    if (!found_rm) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_NO_SUCH_METHOD, mname);
        goto done;
    }}
    int want_static = (ref_kind == JABS_MH_REF_INVOKE_STATIC);
    int is_static = (found_rm->access & JABS_MH_ACC_STATIC) != 0;
    if (want_static != is_static) {{
        if (!speculative) jabs_mh_raise(JABS_MH_ERR_ICCE, mname);
        goto done;
    }}
    if (jabs_mh_check_access(caller_cls, lookup_mode, rc, found_rc, found_rm->access, speculative) != 0) goto done;

    uint32_t out_kind = ref_kind;
    jabs_object *out_cls = NULL;
    if (ref_kind == JABS_MH_REF_INVOKE_STATIC || ref_kind == JABS_MH_REF_INVOKE_SPECIAL) {{
        out_cls = jabs_class_object(jabs_rstrings + found_rc->name);
    }} else if ((found_rm->access & (JABS_MH_ACC_PRIVATE | JABS_MH_ACC_FINAL)) || (found_rc->access & JABS_MH_ACC_FINAL)) {{
        out_kind = JABS_MH_REF_INVOKE_SPECIAL;
        out_cls = jabs_class_object(jabs_rstrings + found_rc->name);
    }} else if (found_rc->access & JABS_MH_ACC_INTERFACE) {{
        if (!(rc->access & JABS_MH_ACC_INTERFACE)) {{
            /* Inherited interface method resolved on a class must keep class receiver and REF_invokeVirtual. */
            out_kind = JABS_MH_REF_INVOKE_VIRTUAL;
            out_cls = clazz;
        }} else {{
            out_kind = JABS_MH_REF_INVOKE_IFACE;
            out_cls = jabs_class_object(jabs_rstrings + found_rc->name);
        }}
    }} else {{
        out_kind = JABS_MH_REF_INVOKE_VIRTUAL;
        out_cls = jabs_class_object(jabs_rstrings + found_rc->name);
    }}
    if (!out_cls) goto done;
    uint32_t extra = jabs_mh_extra_flags(jabs_rstrings + found_rc->name, jabs_rstrings + found_rm->name, jabs_rstrings + found_rm->descriptor);
    uint32_t out_flags = (out_kind << JABS_MH_REF_KIND_SHIFT) | JABS_MH_MN_IS_METHOD
        | ((uint32_t)found_rm->access & JABS_MH_METHOD_MODS) | extra;
    jabs_object *rmn = jabs_mh_make_rmn(out_cls, found_rc, found_rm);
    if (!rmn) goto done;
    mn->{f_mn_clazz} = out_cls;
    mn->{f_mn_flags} = (int32_t)out_flags;
    mn->{f_mn_method} = rmn;
    result = mn_obj;

done:
    free(mname);
    free(mdesc);
    return result;
}}
"#,
        extra_count = extra_rows.len(),
        mn_struct = object_struct(MEMBER_NAME),
        mn_record = class_record(MEMBER_NAME),
        rmn_struct = object_struct(RESOLVED_METHOD_NAME),
        rmn_record = class_record(RESOLVED_METHOD_NAME),
        rmn_init = init_symbol(RESOLVED_METHOD_NAME),
        rmn_ctor = symbol(RESOLVED_METHOD_NAME, "<init>", "()V"),
        str_record = class_record(STRING),
        cls_record = class_record(CLASS),
        mt_struct = object_struct(METHOD_TYPE),
        mt_record = class_record(METHOD_TYPE),
        rm_struct = object_struct(REFLECT_METHOD),
        rm_record = class_record(REFLECT_METHOD),
        rc_struct = object_struct(REFLECT_CONSTRUCTOR),
        rc_record = class_record(REFLECT_CONSTRUCTOR),
        rf_struct = object_struct(REFLECT_FIELD),
        rf_record = class_record(REFLECT_FIELD),
        f_vmholder = instance_field("vmholder"),
        f_mn_clazz = instance_field("clazz"),
        f_mn_name = instance_field("name"),
        f_mn_type = instance_field("type"),
        f_mn_flags = instance_field("flags"),
        f_mn_method = instance_field("method"),
        f_mn_resolution = instance_field("resolution"),
        f_rtype = instance_field("rtype"),
        f_ptypes = instance_field("ptypes"),
        f_rm_clazz = instance_field("clazz"),
        f_rm_slot = instance_field("slot"),
        f_rc_clazz = instance_field("clazz"),
        f_rc_slot = instance_field("slot"),
        f_rf_clazz = instance_field("clazz"),
        f_rf_slot = instance_field("slot"),
    );

    out
}
