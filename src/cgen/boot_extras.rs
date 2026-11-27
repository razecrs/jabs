//! Small `java.base` boot natives: `jdk.internal.perf.Perf`, `Class.getPermittedSubclasses0`,
//! `BootLoader.getSystemPackageLocation`, `NativeImageBuffer.getNativeMap`, and
//! `NativeLibrary.findEntry0`.

use super::*;

const PERF: &str = "jdk/internal/perf/Perf";
const BOOT_LOADER: &str = "jdk/internal/loader/BootLoader";
const NATIVE_IMAGE_BUFFER: &str = "jdk/internal/jimage/NativeImageBuffer";
const NATIVE_LIBRARY: &str = "jdk/internal/loader/NativeLibrary";
const DIRECT_BYTE_BUFFER: &str = "java/nio/DirectByteBuffer";
const ILLEGAL_ARGUMENT: &str = "java/lang/IllegalArgumentException";
const DIRECT_BYTE_BUFFER_INIT: &str = "(JJ)V";
const CREATE_LONG_DESC: &str = "(Ljava/lang/String;IIJ)Ljava/nio/ByteBuffer;";
const CREATE_BYTE_ARRAY_DESC: &str = "(Ljava/lang/String;II[BI)Ljava/nio/ByteBuffer;";
const GET_PERMITTED_SUBCLASSES_DESC: &str = "()[Ljava/lang/Class;";

/// Java constructors `Perf` natives invoke from C to wrap native buffers in `DirectByteBuffer`.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if key.0 == PERF
        && matches!(
            (key.1.as_str(), key.2.as_str()),
            ("createLong", CREATE_LONG_DESC) | ("createByteArray", CREATE_BYTE_ARRAY_DESC)
        )
    {
        return vec![
            (DIRECT_BYTE_BUFFER, "<init>", DIRECT_BYTE_BUFFER_INIT),
            (ILLEGAL_ARGUMENT, "<init>", "(Ljava/lang/String;)V"),
        ];
    }
    Vec::new()
}

/// Permitted subclasses of every recorded sealed class/interface must keep class records when
/// `Class.getPermittedSubclasses0` is reachable, because `Class.getPermittedSubclasses()` verifies
/// `isDirectSubType` via `getSuperclass()` / `getInterfaces0()`.
pub(super) fn reach(
    unit: &Unit,
    records: &BTreeSet<String>,
    methods: &BTreeSet<MethodKey>,
) -> Vec<String> {
    let key = (
        CLASS.to_string(),
        "getPermittedSubclasses0".to_string(),
        GET_PERMITTED_SUBCLASSES_DESC.to_string(),
    );
    if !methods.contains(&key) {
        return Vec::new();
    }
    let mut missing = Vec::new();
    for name in records {
        let Some(class) = unit.get(name) else {
            continue;
        };
        let Some(subclasses) = &class.reflect.permitted_subclasses else {
            continue;
        };
        for sub in subclasses {
            if !records.contains(sub)
                && unit.get(sub).is_some()
                && (unit.is_interface(sub) || unit.chain(sub).is_ok())
            {
                missing.push(sub.clone());
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// The body of a boot-extra native, if JABS implements it: (C body, needs the class registry).
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (CLASS, "getPermittedSubclasses0", GET_PERMITTED_SUBCLASSES_DESC) => {
            classes_available.then(|| ("return jabs_boot_permitted_subclasses(self);".into(), true))
        }
        (BOOT_LOADER, "getSystemPackageLocation", "(Ljava/lang/String;)Ljava/lang/String;") => {
            classes_available.then(|| ("return jabs_boot_package_location(a0);".into(), true))
        }
        (NATIVE_IMAGE_BUFFER, "getNativeMap", "(Ljava/lang/String;)Ljava/nio/ByteBuffer;") => {
            Some(("return NULL;".into(), false))
        }
        (NATIVE_LIBRARY, "findEntry0", "(JLjava/lang/String;)J") => {
            Some(("return 0;".into(), false))
        }
        (PERF, "registerNatives", "()V") => {
            Some(("/* natives are linked statically */".into(), false))
        }
        (PERF, "createLong", CREATE_LONG_DESC) => classes_available.then(|| {
            (
                "return jabs_boot_perf_create_long(a0, a1, a2, a3);".into(),
                true,
            )
        }),
        (PERF, "createByteArray", CREATE_BYTE_ARRAY_DESC) => classes_available.then(|| {
            (
                "return jabs_boot_perf_create_byte_array(a0, a1, a2, a3, a4);".into(),
                true,
            )
        }),
        (PERF, "highResCounter", "()J") => Some(("return jabs_nano_time();".into(), false)),
        (PERF, "highResFrequency", "()J") => Some(("return INT64_C(1000000000);".into(), false)),
        (PERF, "attach0", "(I)Ljava/nio/ByteBuffer;") => Some((
            "jabs_pending = jabs_vm_throwable(JABS_VM_IO); return NULL;".into(),
            false,
        )),
        (PERF, "detach", "(Ljava/nio/ByteBuffer;)V") => {
            Some(("/* no hsperfdata attachment in JABS */".into(), false))
        }
        _ => None,
    }
}

/// C glue emitted into `unit_0.c` for `Perf` direct buffers and `Class.getPermittedSubclasses0`.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
    registry: bool,
) -> String {
    let mut out = String::from("\n/* ---- Boot extras glue (src/cgen/boot_extras.rs) ---- */\n");
    let dbb_ctor = (
        DIRECT_BYTE_BUFFER.to_string(),
        "<init>".to_string(),
        DIRECT_BYTE_BUFFER_INIT.to_string(),
    );
    let dbb_ok = unit.get(DIRECT_BYTE_BUFFER).is_some()
        && records.contains(DIRECT_BYTE_BUFFER)
        && inits.contains(DIRECT_BYTE_BUFFER)
        && methods.contains(&dbb_ctor);

    if dbb_ok {
        let _ = writeln!(
            out,
            "static jabs_object *jabs_boot_new_direct_byte_buffer(int64_t addr, int64_t cap) {{\n\
             \x20   if ({init}() != 0) return NULL;\n\
             \x20   jabs_object *bb = jabs_new(&{record});\n\
             \x20   if (!bb) return NULL;\n\
             \x20   {ctor}(bb, addr, cap);\n\
             \x20   return jabs_pending ? NULL : bb;\n\
             }}",
            init = init_symbol(DIRECT_BYTE_BUFFER),
            record = class_record(DIRECT_BYTE_BUFFER),
            ctor = symbol(DIRECT_BYTE_BUFFER, "<init>", DIRECT_BYTE_BUFFER_INIT),
        );
    } else {
        out.push_str(
            "static jabs_object *jabs_boot_new_direct_byte_buffer(int64_t addr, int64_t cap) {\n\
             \x20   (void)addr; (void)cap;\n\
             \x20   jabs_not_compiled(\"DirectByteBuffer(long, long) is not in the program (src/cgen/boot_extras.rs)\");\n\
             }\n",
        );
    }

    let iae_ctor = (
        ILLEGAL_ARGUMENT.to_string(),
        "<init>".to_string(),
        "(Ljava/lang/String;)V".to_string(),
    );
    let iae_ok = unit.get(ILLEGAL_ARGUMENT).is_some()
        && records.contains(ILLEGAL_ARGUMENT)
        && inits.contains(ILLEGAL_ARGUMENT)
        && methods.contains(&iae_ctor);
    if registry && iae_ok {
        let _ = writeln!(
            out,
            "static void jabs_boot_raise_iae(const char *msg) {{\n\
             \x20   if ({init}() != 0) return;\n\
             \x20   jabs_object *o = jabs_new(&{record});\n\
             \x20   if (!o) return;\n\
             \x20   jabs_object *s = jabs_string_from_utf8(msg);\n\
             \x20   if (!s) return;\n\
             \x20   {ctor}(o, s);\n\
             \x20   if (!jabs_pending) jabs_pending = o;\n\
             }}",
            init = init_symbol(ILLEGAL_ARGUMENT),
            record = class_record(ILLEGAL_ARGUMENT),
            ctor = symbol(ILLEGAL_ARGUMENT, "<init>", "(Ljava/lang/String;)V"),
        );
    } else {
        out.push_str(
            "static void jabs_boot_raise_iae(const char *msg) {\n\
             \x20   (void)msg;\n\
             \x20   jabs_not_compiled(\"IllegalArgumentException(String) is not in the program (src/cgen/boot_extras.rs)\");\n\
             }\n",
        );
    }

    out.push_str(
        "jabs_object *jabs_boot_perf_create_long(jabs_object *name, int32_t variability, int32_t units, int64_t value) {\n\
         \x20   if (!name) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }\n\
         \x20   if (variability < 1 || variability > 3 || units < 1 || units > 6) {\n\
         \x20       jabs_boot_raise_iae(\"unexpected variability or units value\"); return NULL;\n\
         \x20   }\n\
         \x20   int64_t *slot = (int64_t *)malloc(sizeof(int64_t));\n\
         \x20   if (!slot) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }\n\
         \x20   *slot = value;\n\
         \x20   jabs_object *bb = jabs_boot_new_direct_byte_buffer((int64_t)(uintptr_t)slot, INT64_C(8));\n\
         \x20   if (!bb) { free(slot); return NULL; }\n\
         \x20   return bb;\n\
         }\n\
         jabs_object *jabs_boot_perf_create_byte_array(jabs_object *name, int32_t variability, int32_t units, jabs_object *bytes, int32_t max_len) {\n\
         \x20   if (!name || !bytes) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }\n\
         \x20   if ((variability != 1 && variability != 3) || units != 5 || (variability == 3 && max_len < -1)) {\n\
         \x20       jabs_boot_raise_iae(\"unexpected variability, units, or maxLength for Perf string\"); return NULL;\n\
         \x20   }\n\
         \x20   jabs_array *arr = (jabs_array *)bytes;\n\
         \x20   int32_t len = arr->length;\n\
         \x20   if (len < 0) {\n\
         \x20       jabs_boot_raise_iae(\"negative byte array length\"); return NULL;\n\
         \x20   }\n\
         \x20   const uint8_t *src = len > 0 ? (const uint8_t *)jabs_array_data(arr) : NULL;\n\
         \x20   size_t slen = 0;\n\
         \x20   while (slen < (size_t)len && src[slen] != 0) slen++;\n\
         \x20   size_t cap = variability == 1\n\
         \x20       ? slen + 1u\n\
         \x20       : (max_len < 0 ? 0u : (size_t)max_len + 1u);\n\
         \x20   size_t alloc_size = cap > 0 ? cap : 1u;\n\
         \x20   uint8_t *buf = (uint8_t *)calloc(1, alloc_size);\n\
         \x20   if (!buf) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }\n\
         \x20   if (cap > 1u && slen > 0) {\n\
         \x20       size_t ncopy = slen < cap - 1u ? slen : cap - 1u;\n\
         \x20       memcpy(buf, src, ncopy);\n\
         \x20   }\n\
         \x20   jabs_object *bb = jabs_boot_new_direct_byte_buffer((int64_t)(uintptr_t)buf, (int64_t)cap);\n\
         \x20   if (!bb) { free(buf); return NULL; }\n\
         \x20   return bb;\n\
         }\n",
    );

    if !registry {
        out.push_str(
            "jabs_object *jabs_boot_package_location(jabs_object *pkg) {\n\
             \x20   (void)pkg;\n\
             \x20   jabs_not_compiled(\"BootLoader.getSystemPackageLocation without a class registry (src/cgen/boot_extras.rs)\");\n\
             }\n\
             jabs_object *jabs_boot_permitted_subclasses(jabs_object *c) {\n\
             \x20   (void)c;\n\
             \x20   jabs_not_compiled(\"Class.getPermittedSubclasses0 without a class registry (src/cgen/boot_extras.rs)\");\n\
             }\n",
        );
        return out;
    }

    out.push_str(
        "jabs_object *jabs_boot_package_location(jabs_object *pkg_obj) {\n\
         \x20   if (!pkg_obj) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }\n\
         \x20   char *pkg = jabs_string_to_utf8(pkg_obj);\n\
         \x20   if (!pkg) { if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }\n\
         \x20   const char *mod = jabs_module_name_of_package(pkg);\n\
         \x20   free(pkg);\n\
         \x20   if (!mod) return NULL;\n\
         \x20   size_t n = strlen(mod);\n\
         \x20   char stack_uri[256];\n\
         \x20   char *heap_uri = NULL;\n\
         \x20   char *uri = (6u + n) < sizeof stack_uri ? stack_uri : (heap_uri = (char *)malloc(7u + n));\n\
         \x20   if (!uri) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }\n\
         \x20   memcpy(uri, \"jrt:/\", 5);\n\
         \x20   memcpy(uri + 5, mod, n + 1);\n\
         \x20   jabs_object *res = jabs_string_from_utf8(uri);\n\
         \x20   free(heap_uri);\n\
         \x20   return res;\n\
         }\n",
    );

    let usable = |name: &str| {
        records.contains(name)
            && unit.get(name).is_some()
            && (unit.is_interface(name) || unit.chain(name).is_ok())
    };
    let mut sealed: Vec<(&str, Vec<&str>)> = Vec::new();
    for name in records {
        if !usable(name) {
            continue;
        }
        let Some(class) = unit.get(name) else {
            continue;
        };
        let Some(subs) = &class.reflect.permitted_subclasses else {
            continue;
        };
        let present: Vec<&str> = subs
            .iter()
            .map(String::as_str)
            .filter(|s| usable(s))
            .collect();
        sealed.push((name.as_str(), present));
    }
    sealed.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    for (i, (_, subs)) in sealed.iter().enumerate() {
        if subs.is_empty() {
            continue;
        }
        let items: Vec<String> = subs
            .iter()
            .map(|s| format!("\"{}\"", c_escape(s)))
            .collect();
        let _ = writeln!(
            out,
            "static const char *const jabs_boot_perm_{i}[] = {{ {} }};",
            items.join(", ")
        );
    }
    out.push_str(
        "typedef struct { const char *name; int32_t count; const char *const *subclasses; } jabs_boot_permitted_entry;\n\
         static const jabs_boot_permitted_entry jabs_boot_permitted_table[] = {\n",
    );
    for (i, (name, subs)) in sealed.iter().enumerate() {
        let ptr = if subs.is_empty() {
            "NULL".to_string()
        } else {
            format!("jabs_boot_perm_{i}")
        };
        let _ = writeln!(
            out,
            "    {{ \"{}\", {}, {ptr} }},",
            c_escape(name),
            subs.len()
        );
    }
    let _ = write!(
        out,
        "    {{ NULL, 0, NULL }}\n\
         }};\n\
         static int jabs_boot_compare_permitted(const void *key, const void *entry) {{\n\
         \x20   return strcmp((const char *)key, ((const jabs_boot_permitted_entry *)entry)->name);\n\
         }}\n\
         jabs_object *jabs_boot_permitted_subclasses(jabs_object *c) {{\n\
         \x20   if (!c) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}\n\
         \x20   jabs_class_entry *e = jabs_class_entry_of(c);\n\
         \x20   if (!e) jabs_not_compiled(\"Class.getPermittedSubclasses0 on a Class the registry did not make\");\n\
         \x20   if ({count} == 0) return NULL;\n\
         \x20   const jabs_boot_permitted_entry *found = (const jabs_boot_permitted_entry *)bsearch(\n\
         \x20       e->name, jabs_boot_permitted_table, {count}, sizeof *jabs_boot_permitted_table, jabs_boot_compare_permitted);\n\
         \x20   if (!found) return NULL;\n\
         \x20   jabs_object *array = jabs_new_array(found->count, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{class_rec}, \"[Ljava/lang/Class;\", NULL);\n\
         \x20   if (!array) return NULL;\n\
         \x20   for (int32_t i = 0; i < found->count; i++) {{\n\
         \x20       jabs_object *sub = jabs_class_object(found->subclasses[i]);\n\
         \x20       if (!sub) return NULL;\n\
         \x20       ((jabs_object **)jabs_array_data((jabs_array *)array))[i] = sub;\n\
         \x20   }}\n\
         \x20   return array;\n\
         }}\n",
        count = sealed.len(),
        class_rec = class_record(CLASS),
    );
    out
}
