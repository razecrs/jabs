//! Intrinsics and C glue for `jdk.internal.misc.Unsafe` (park, unpark, allocateInstance,
//! shouldBeInitialized0), `java.lang.ref.Reference` pending-list stubs, `PreviewFeatures`, and
//! `LambdaProxyClassArchive`.

use super::*;

const REFERENCE: &str = "java/lang/ref/Reference";
const PREVIEW_FEATURES: &str = "jdk/internal/misc/PreviewFeatures";
const LAMBDA_ARCHIVE: &str = "java/lang/invoke/LambdaProxyClassArchive";

/// Java methods called from C by these intrinsics (none beyond VM_EXCEPTIONS).
pub(super) fn called_by(_key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    Vec::new()
}

/// Intrinsic body for the supported `Unsafe`, `Reference`, `PreviewFeatures`, and
/// `LambdaProxyClassArchive` natives: `(C body, needs_classes)`.
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (UNSAFE, "park", "(ZJ)V") => Some(("jabs_thread_park(a0, a1);".into(), false)),
        (UNSAFE, "unpark", "(Ljava/lang/Object;)V") => {
            Some(("jabs_thread_unpark(a0);".into(), false))
        }
        (UNSAFE, "allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;") => {
            classes_available.then(|| ("return jabs_unsafe_allocate_instance(a0);".into(), true))
        }
        (UNSAFE, "shouldBeInitialized0", "(Ljava/lang/Class;)Z") => classes_available
            .then(|| ("return jabs_unsafe_should_be_initialized(a0);".into(), true)),
        (REFERENCE, "getAndClearReferencePendingList", "()Ljava/lang/ref/Reference;") => {
            Some(("return jabs_gc_take_pending_refs();".into(), false))
        }
        (REFERENCE, "waitForReferencePendingList", "()V") => {
            Some(("jabs_gc_wait_pending_refs();".into(), false))
        }
        (PREVIEW_FEATURES, "isPreviewEnabled", "()Z") => Some(("return 0;".into(), false)),
        (
            LAMBDA_ARCHIVE,
            "findFromArchive",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MemberName;Ljava/lang/invoke/MethodType;)Ljava/lang/Class;",
        ) => Some(("return NULL;".into(), false)),
        _ => None,
    }
}

/// C glue for `Unsafe.allocateInstance` and `Unsafe.shouldBeInitialized0`.
pub(super) fn glue(inits: &BTreeSet<String>) -> String {
    let mut out = String::from("\n/* ---- Unsafe runtime glue (src/cgen/unsafe_rt.rs) ---- */\n");
    let _ = writeln!(
        out,
        "typedef struct {{ const char *name; jabs_class_init *state; }} jabs_init_state_entry;\nstatic const jabs_init_state_entry jabs_init_states[] = {{"
    );
    for n in inits {
        let _ = writeln!(
            out,
            "    {{ \"{}\", &{}_state }},",
            c_escape(n),
            init_symbol(n)
        );
    }
    let _ = writeln!(out, "    {{ NULL, NULL }}\n}};");
    let _ = write!(
        out,
        r#"static int jabs_compare_init_state(const void *key, const void *entry) {{ return strcmp((const char *)key, ((const jabs_init_state_entry *)entry)->name); }}

int32_t jabs_unsafe_should_be_initialized(jabs_object *c) {{
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    if (e->name[0] == '[' || jabs_is_primitive_name(e->name)) return 0;
    const jabs_init_state_entry *s = (const jabs_init_state_entry *)bsearch(
        e->name, jabs_init_states, {count}, sizeof *s, jabs_compare_init_state);
    if (!s) return 1;
    return jabs_class_initialized(s->state) ? 0 : 1;
}}

jabs_object *jabs_unsafe_allocate_instance(jabs_object *c) {{
    if (!c) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || e->name[0] == '[' || jabs_is_primitive_name(e->name)) {{
        jabs_pending = jabs_vm_throwable(JABS_VM_INSTANTIATION);
        return NULL;
    }}
    if (!e->record) jabs_not_compiled_class("Unsafe.allocateInstance for a class without a compiled record", e->name);
    if (e->record->is_interface || (e->record->access & 0x0400u) != 0) {{
        jabs_pending = jabs_vm_throwable(JABS_VM_INSTANTIATION);
        return NULL;
    }}
    const jabs_init_entry *i = (const jabs_init_entry *)bsearch(
        e->name, jabs_init_functions, sizeof jabs_init_functions / sizeof *jabs_init_functions - 1,
        sizeof *i, jabs_compare_init);
    if (!i) jabs_not_compiled_class("class initialization requested through Unsafe.allocateInstance", e->name);
    if (i->init() != 0) return NULL;
    return jabs_new(e->record);
}}
"#,
        count = inits.len(),
    );
    out
}
