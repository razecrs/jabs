//! java.lang.StackWalker / java.lang.StackStreamFactory and jdk.internal.vm.Continuation
//! natives: walks the current thread's native stack via `jabs_platform_stack_functions` and
//! `jabs_frames[]`, filling OpenJDK 25's pre-allocated `ClassFrameInfo` / `StackFrameInfo` batch
//! buffers and resuming lazily across batches through a stack-local cursor passed as `anchor`.

use super::*;

const STACK_STREAM_FACTORY: &str = "java/lang/StackStreamFactory";
const ABSTRACT_WALKER: &str = "java/lang/StackStreamFactory$AbstractStackWalker";
const STACK_WALKER: &str = "java/lang/StackWalker";
const CLASS_FRAME_INFO: &str = "java/lang/ClassFrameInfo";
const STACK_FRAME_INFO: &str = "java/lang/StackFrameInfo";
const STACK_TRACE_ELEMENT: &str = "java/lang/StackTraceElement";
const RESOLVED_METHOD_NAME: &str = "java/lang/invoke/ResolvedMethodName";
const METHOD_HANDLE_IMPL: &str = "java/lang/invoke/MethodHandleImpl";
const CONTINUATION: &str = "jdk/internal/vm/Continuation";
const CONTINUATION_SUPPORT: &str = "jdk/internal/vm/ContinuationSupport";

const CALL_STACK_WALK_DESC: &str = "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;";
const FETCH_STACK_FRAMES_DESC: &str = "(IJIII[Ljava/lang/Object;)I";
const SET_CONTINUATION_DESC: &str = "(J[Ljava/lang/Object;Ljdk/internal/vm/Continuation;)V";
const DO_STACK_WALK_DESC: &str = "(JIIII)Ljava/lang/Object;";
const INIT_TRACE_ELEMENT_DESC: &str = "(Ljava/lang/StackTraceElement;Ljava/lang/StackFrameInfo;)V";

/// HotSpot MemberName / Modifier bits carried in `ClassFrameInfo.flags` (low 24 bits).
const ACC_NATIVE_FLAG: u32 = 0x0100;
const MN_CALLER_SENSITIVE: u32 = 0x0010_0000;
pub(super) const MN_HIDDEN_MEMBER: u32 = 0x0040_0000;

/// Method flags stored in `jabs_frame_entry.flags` for StackWalker frame filtering and queries.
pub(super) fn method_flags(
    unit: &Unit,
    class: &str,
    name: &str,
    descriptor: &str,
    hidden_class: bool,
) -> u32 {
    let mut flags = if hidden_class { MN_HIDDEN_MEMBER } else { 0 };
    if let Some(m) = unit.get(class).and_then(|c| {
        c.methods
            .iter()
            .find(|m| m.name == name && m.descriptor == descriptor)
    }) {
        if m.access & ACC_NATIVE != 0 {
            flags |= ACC_NATIVE_FLAG;
        }
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
    }
    flags
}

/// Java methods `StackWalker` natives invoke or allocate from C.
///
/// `StackFrameBuffer` allocates `StackFrameInfo` reflectively via `Constructor.newInstance`, and
/// `ClassFrameInfo.<clinit>` calls `SharedSecrets.getJavaLangInvokeAccess()`, which uses
/// `Class.forName("java.lang.invoke.MethodHandleImpl", true, null)`. Requiring their `<init>` /
/// `<clinit>` here ensures RTA instantiates `StackFrameInfo` and compiles `MethodHandleImpl`.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if key.0 == ABSTRACT_WALKER && key.1 == "callStackWalk" && key.2 == CALL_STACK_WALK_DESC {
        return vec![
            (ABSTRACT_WALKER, "doStackWalk", DO_STACK_WALK_DESC),
            (RESOLVED_METHOD_NAME, "<init>", "()V"),
            (STACK_FRAME_INFO, "<init>", "(Ljava/lang/StackWalker;)V"),
            (METHOD_HANDLE_IMPL, "<clinit>", "()V"),
        ];
    }
    Vec::new()
}

/// Extract internal class names (`pkg/Name`) from object descriptors (`L...;`) in a signature.
fn descriptor_classes(descriptor: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = descriptor;
    while let Some(start) = rest.find('L') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(';') else { break };
        out.push(&after[..end]);
        rest = &after[end + 1..];
    }
    out
}

/// Every class with a compiled method (and any reference class in its descriptor, for
/// `StackFrameInfo.getMethodType`) can surface from `StackWalker`, so keep runtime records for
/// them when `StackWalker` or `StackTraceElement.initStackTraceElement` is reachable.
pub(super) fn reach(
    unit: &Unit,
    records: &BTreeSet<String>,
    methods: &BTreeSet<MethodKey>,
) -> Vec<String> {
    let walking = methods.iter().any(|k| {
        (k.0 == ABSTRACT_WALKER && k.1 == "callStackWalk" && k.2 == CALL_STACK_WALK_DESC)
            || (k.0 == STACK_TRACE_ELEMENT
                && k.1 == "initStackTraceElement"
                && k.2 == INIT_TRACE_ELEMENT_DESC)
    });
    if !walking {
        return Vec::new();
    }
    let mut missing = Vec::new();
    for key in methods {
        for class in std::iter::once(key.0.as_str()).chain(descriptor_classes(&key.2)) {
            if !records.contains(class) && unit.get(class).is_some() {
                missing.push(class.to_string());
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// The body of a StackWalker or Continuation native, if JABS implements it: (C body, needs registry).
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (STACK_STREAM_FACTORY, "checkStackWalkModes", "()Z") => Some(("return 1;".into(), false)),
        (ABSTRACT_WALKER, "callStackWalk", CALL_STACK_WALK_DESC) => classes_available
            .then(|| ("return jabs_stack_call_walk(self, a0, a1, a2, a3, a4, a5, a6);".into(), true)),
        (ABSTRACT_WALKER, "fetchStackFrames", FETCH_STACK_FRAMES_DESC) => classes_available
            .then(|| ("return jabs_stack_fetch_frames(self, a0, a1, a2, a3, a4, a5);".into(), true)),
        (ABSTRACT_WALKER, "setContinuation", SET_CONTINUATION_DESC) => Some((
            "jabs_not_compiled(\"AbstractStackWalker.setContinuation (continuations are not supported)\");".into(),
            false,
        )),
        (STACK_FRAME_INFO, "expandStackFrameInfo", "()V") => {
            classes_available.then(|| ("jabs_stack_expand_frame_info(self);".into(), true))
        }
        (STACK_TRACE_ELEMENT, "initStackTraceElement", INIT_TRACE_ELEMENT_DESC) => {
            classes_available.then(|| ("jabs_stack_init_trace_element(a0, a1);".into(), true))
        }
        (CONTINUATION, "pin", "()V") | (CONTINUATION, "unpin", "()V") => {
            Some(("/* no virtual-thread continuations in JABS */".into(), false))
        }
        (CONTINUATION_SUPPORT, "isSupported0", "()Z") => Some(("return 0;".into(), false)),
        _ => None,
    }
}

/// C glue emitted into `unit_0.c` so it has direct access to static `jabs_frames[]` and `jabs_frame_of`.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
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

    let walk_ok = records.contains(ABSTRACT_WALKER)
        && records.contains(CLASS_FRAME_INFO)
        && records.contains(STACK_FRAME_INFO)
        && records.contains(RESOLVED_METHOD_NAME)
        && inits.contains(RESOLVED_METHOD_NAME)
        && has_field(ABSTRACT_WALKER, "anchor", "J")
        && has_method(ABSTRACT_WALKER, "doStackWalk", DO_STACK_WALK_DESC)
        && has_field(CLASS_FRAME_INFO, "classOrMemberName", "Ljava/lang/Object;")
        && has_field(CLASS_FRAME_INFO, "flags", "I")
        && has_field(STACK_FRAME_INFO, "name", "Ljava/lang/String;")
        && has_field(STACK_FRAME_INFO, "type", "Ljava/lang/Object;")
        && has_field(STACK_FRAME_INFO, "bci", "I")
        && has_field(RESOLVED_METHOD_NAME, "vmholder", "Ljava/lang/Class;")
        && has_method(RESOLVED_METHOD_NAME, "<init>", "()V");

    let trace_ok = records.contains(CLASS_FRAME_INFO)
        && records.contains(STACK_FRAME_INFO)
        && records.contains(RESOLVED_METHOD_NAME)
        && records.contains(STACK_TRACE_ELEMENT)
        && has_field(CLASS_FRAME_INFO, "classOrMemberName", "Ljava/lang/Object;")
        && has_field(CLASS_FRAME_INFO, "flags", "I")
        && has_field(STACK_FRAME_INFO, "name", "Ljava/lang/String;")
        && has_field(RESOLVED_METHOD_NAME, "vmholder", "Ljava/lang/Class;")
        && has_field(
            STACK_TRACE_ELEMENT,
            "declaringClassObject",
            "Ljava/lang/Class;",
        )
        && has_field(STACK_TRACE_ELEMENT, "declaringClass", "Ljava/lang/String;")
        && has_field(STACK_TRACE_ELEMENT, "methodName", "Ljava/lang/String;")
        && has_field(STACK_TRACE_ELEMENT, "fileName", "Ljava/lang/String;")
        && has_field(STACK_TRACE_ELEMENT, "lineNumber", "I");

    let mut out = String::from("\n/* ---- StackWalker glue (src/cgen/stack.rs) ---- */\n");

    let call_sig = "jabs_object *jabs_stack_call_walk(jabs_object *self, int32_t mode, int32_t skip_frames, jabs_object *scope, jabs_object *cont, int32_t batch_size, int32_t start_index, jabs_object *frames_obj)";
    let fetch_sig = "int32_t jabs_stack_fetch_frames(jabs_object *self, int32_t mode, int64_t anchor, int32_t last_batch_count, int32_t fetch_size, int32_t start_index, jabs_object *frames_obj)";
    let expand_sig = "void jabs_stack_expand_frame_info(jabs_object *self)";
    let trace_sig = "void jabs_stack_init_trace_element(jabs_object *ste, jabs_object *sfi)";

    if !walk_ok {
        let _ = writeln!(
            out,
            "{call_sig} {{ (void)self; (void)mode; (void)skip_frames; (void)scope; (void)cont; (void)batch_size; (void)start_index; (void)frames_obj; jabs_not_compiled(\"AbstractStackWalker.callStackWalk (unknown StackWalker layout)\"); }}"
        );
        let _ = writeln!(
            out,
            "{fetch_sig} {{ (void)self; (void)mode; (void)anchor; (void)last_batch_count; (void)fetch_size; (void)start_index; (void)frames_obj; jabs_not_compiled(\"AbstractStackWalker.fetchStackFrames (unknown StackWalker layout)\"); }}"
        );
        let _ = writeln!(
            out,
            "{expand_sig} {{ (void)self; jabs_not_compiled(\"StackFrameInfo.expandStackFrameInfo (unknown layout)\"); }}"
        );
    } else {
        let rmn_init = format!(
            "if ({}() != 0) return -1; ",
            init_symbol(RESOLVED_METHOD_NAME)
        );
        let _ = write!(
            out,
            r#"#define JABS_STACK_MAX_FRAMES 1024
#define JABS_STACK_MODE_SHOW_HIDDEN 32u
#define JABS_STACK_MEMBER_FLAGS_MASK 0x00FFFFFFu
#define JABS_STACK_MN_HIDDEN_MEMBER 0x00400000u

typedef struct {{
    jabs_object *walker;
    const jabs_frame_entry *frames[JABS_STACK_MAX_FRAMES];
    int32_t count;
    int32_t pos;
}} jabs_stack_cursor;

static int32_t jabs_stack_fill_frames(jabs_stack_cursor *cursor, jabs_array *arr, int32_t start_index, int32_t capacity) {{
    int32_t filled = 0;
    jabs_object **slots = (jabs_object **)jabs_array_data(arr);
    while (filled < capacity && cursor->pos < cursor->count) {{
        const jabs_frame_entry *e = cursor->frames[cursor->pos++];
        jabs_object *frame = slots[start_index + filled];
        if (!frame) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return -1; }}
        if (!jabs_class_assignable(frame->cls, &{cfi_record})) jabs_not_compiled("StackWalker frame slot is not a ClassFrameInfo");
        jabs_object *cls = jabs_class_object(e->class_name);
        if (!cls) return -1;
        {cfi_struct} *cfi = ({cfi_struct} *)frame;
        cfi->{f_flags} = (int32_t)(((uint32_t)cfi->{f_flags} & ~JABS_STACK_MEMBER_FLAGS_MASK) | (e->flags & JABS_STACK_MEMBER_FLAGS_MASK));
        if (jabs_class_assignable(frame->cls, &{sfi_record})) {{
            {rmn_init}jabs_object *rmn = jabs_new(&{rmn_record});
            if (!rmn) return -1;
            {rmn_ctor}(rmn);
            if (jabs_pending) return -1;
            (({rmn_struct} *)rmn)->{f_vmholder} = cls;
            cfi->{f_member} = rmn;
            jabs_object *mname = jabs_string_from_utf8(e->name);
            if (!mname) {{
                if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
                return -1;
            }}
            jabs_object *mdesc = jabs_string_from_utf8(e->descriptor);
            if (!mdesc) {{
                if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
                return -1;
            }}
            {sfi_struct} *sfi = ({sfi_struct} *)frame;
            sfi->{f_name} = mname;
            sfi->{f_type} = mdesc;
            sfi->{f_bci} = -1;
        }} else {{
            cfi->{f_member} = cls;
        }}
        filled++;
    }}
    return filled;
}}

{call_sig} {{
    (void)scope;
    if (!self) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (cont) jabs_not_compiled("StackWalker on a Continuation is not supported");
    if (!frames_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (frames_obj->cls != jabs_array_class()) jabs_not_compiled("AbstractStackWalker.callStackWalk frames is not an array");
    jabs_array *arr = (jabs_array *)frames_obj;
    if (arr->component_kind == JABS_COMPONENT_PRIMITIVE) jabs_not_compiled("AbstractStackWalker.callStackWalk frames is a primitive array");
    if (start_index < 0 || batch_size < start_index || batch_size > arr->length) {{
        jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX);
        return NULL;
    }}
    jabs_stack_cursor cursor;
    cursor.walker = self;
    cursor.count = 0;
    cursor.pos = 0;

    const void *starts[JABS_STACK_MAX_FRAMES];
    int n = jabs_platform_stack_functions(starts, JABS_STACK_MAX_FRAMES);
    int i = 0;
    int found_entry = 0;
    while (i < n) {{
        const jabs_frame_entry *e = starts[i] ? jabs_frame_of(starts[i]) : NULL;
        i++;
        if (e && strcmp(e->class_name, "{ABSTRACT_WALKER}") == 0 && strcmp(e->name, "callStackWalk") == 0) {{
            found_entry = 1;
            break;
        }}
    }}
    if (!found_entry) i = 0;
    /* Skip StackWalker/StackStreamFactory entry frames directly above callStackWalk so small initial batches
     * (like CallerClassFinder's 2 usable slots) receive user frames right away; nested StackWalker calls deeper
     * on the stack remain visible. */
    while (i < n) {{
        const jabs_frame_entry *e = starts[i] ? jabs_frame_of(starts[i]) : NULL;
        if (!e) {{ i++; continue; }}
        if (strcmp(e->class_name, "{STACK_WALKER}") == 0
            || strcmp(e->class_name, "{STACK_STREAM_FACTORY}") == 0
            || strncmp(e->class_name, "{STACK_STREAM_FACTORY}$", sizeof("{STACK_STREAM_FACTORY}$") - 1) == 0) {{
            i++;
            continue;
        }}
        break;
    }}
    int32_t to_skip = skip_frames;
    for (; i < n && cursor.count < JABS_STACK_MAX_FRAMES; i++) {{
        const jabs_frame_entry *e = starts[i] ? jabs_frame_of(starts[i]) : NULL;
        if (!e) continue;
        if (e->hidden && !(e->flags & JABS_STACK_MN_HIDDEN_MEMBER)) continue;
        int is_hidden = (e->flags & JABS_STACK_MN_HIDDEN_MEMBER) != 0;
        if (is_hidden && !((uint32_t)mode & JABS_STACK_MODE_SHOW_HIDDEN)) continue;
        if (to_skip > 0) {{ to_skip--; continue; }}
        cursor.frames[cursor.count++] = e;
    }}

    int32_t filled = jabs_stack_fill_frames(&cursor, arr, start_index, batch_size - start_index);
    if (filled < 0) return NULL;
    /* Pass cursor stack address as anchor; AbstractStackWalker.walkHelper().finally closes it to -1 before return. */
    return {do_stack_walk}(self, (int64_t)(intptr_t)&cursor, skip_frames, filled, start_index, start_index + filled);
}}

{fetch_sig} {{
    (void)mode;
    (void)last_batch_count;
    if (!self) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    if (!jabs_class_assignable(self->cls, &{aw_record})) jabs_not_compiled("AbstractStackWalker.fetchStackFrames receiver is not an AbstractStackWalker");
    if (!anchor || anchor == -1 || anchor != (({aw_struct} *)self)->{f_anchor}) {{
        jabs_not_compiled("AbstractStackWalker.fetchStackFrames with an invalid anchor");
    }}
    jabs_stack_cursor *cursor = (jabs_stack_cursor *)(intptr_t)anchor;
    if (cursor->walker != self) jabs_not_compiled("AbstractStackWalker.fetchStackFrames with mismatched walker");
    if (!frames_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    if (frames_obj->cls != jabs_array_class()) jabs_not_compiled("AbstractStackWalker.fetchStackFrames frames is not an array");
    jabs_array *arr = (jabs_array *)frames_obj;
    if (arr->component_kind == JABS_COMPONENT_PRIMITIVE) jabs_not_compiled("AbstractStackWalker.fetchStackFrames frames is a primitive array");
    if (start_index < 0 || fetch_size < start_index || fetch_size > arr->length) {{
        jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX);
        return 0;
    }}
    int32_t filled = jabs_stack_fill_frames(cursor, arr, start_index, fetch_size - start_index);
    return filled < 0 ? 0 : filled;
}}

{expand_sig} {{
    if (!self) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (!jabs_class_assignable(self->cls, &{sfi_record})) jabs_not_compiled("StackFrameInfo.expandStackFrameInfo receiver is not a StackFrameInfo");
    {sfi_struct} *sfi = ({sfi_struct} *)self;
    if (sfi->{f_name} && sfi->{f_type}) return;
    jabs_not_compiled("StackFrameInfo.expandStackFrameInfo on an unexpanded frame");
}}
"#,
            cfi_struct = object_struct(CLASS_FRAME_INFO),
            cfi_record = class_record(CLASS_FRAME_INFO),
            sfi_struct = object_struct(STACK_FRAME_INFO),
            sfi_record = class_record(STACK_FRAME_INFO),
            rmn_struct = object_struct(RESOLVED_METHOD_NAME),
            rmn_record = class_record(RESOLVED_METHOD_NAME),
            rmn_ctor = symbol(RESOLVED_METHOD_NAME, "<init>", "()V"),
            aw_struct = object_struct(ABSTRACT_WALKER),
            aw_record = class_record(ABSTRACT_WALKER),
            do_stack_walk = symbol(ABSTRACT_WALKER, "doStackWalk", DO_STACK_WALK_DESC),
            f_flags = instance_field("flags"),
            f_member = instance_field("classOrMemberName"),
            f_vmholder = instance_field("vmholder"),
            f_name = instance_field("name"),
            f_type = instance_field("type"),
            f_bci = instance_field("bci"),
            f_anchor = instance_field("anchor"),
        );
    }

    if !trace_ok {
        let _ = writeln!(
            out,
            "{trace_sig} {{ (void)ste; (void)sfi; jabs_not_compiled(\"StackTraceElement.initStackTraceElement (unknown layout)\"); }}"
        );
    } else {
        let _ = write!(
            out,
            r#"
{trace_sig} {{
    if (!ste || !sfi) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (!jabs_class_assignable(ste->cls, &{ste_record}) || !jabs_class_assignable(sfi->cls, &{sfi_record})) jabs_not_compiled("StackTraceElement.initStackTraceElement with unexpected argument type");
    {cfi_struct} *cfi = ({cfi_struct} *)sfi;
    {sfi_struct} *info = ({sfi_struct} *)sfi;
    jabs_object *rmn = cfi->{f_member};
    if (!rmn) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (!jabs_class_assignable(rmn->cls, &{rmn_record})) jabs_not_compiled("StackTraceElement.initStackTraceElement member is not a ResolvedMethodName");
    jabs_object *cls = (({rmn_struct} *)rmn)->{f_vmholder};
    if (!cls) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    jabs_class_entry *ce = jabs_class_entry_of(cls);
    if (!ce) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    size_t length = strlen(ce->name);
    char *dotted = (char *)malloc(length + 1);
    if (!dotted) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return; }}
    for (size_t k = 0; k <= length; k++) dotted[k] = ce->name[k] == '/' ? '.' : ce->name[k];
    jabs_object *class_name = jabs_string_from_utf8(dotted);
    free(dotted);
    if (!class_name) {{
        if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return;
    }}
    if (!info->{f_name}) jabs_not_compiled("StackTraceElement.initStackTraceElement on an unexpanded StackFrameInfo");
    {ste_struct} *el = ({ste_struct} *)ste;
    el->{f_decl_cls} = cls;
    el->{f_decl_name} = class_name;
    el->{f_ste_method} = info->{f_name};
    el->{f_file} = NULL;
    el->{f_line} = ((uint32_t)cfi->{f_flags} & 0x0100u) ? -2 : -1;
}}
"#,
            cfi_struct = object_struct(CLASS_FRAME_INFO),
            sfi_struct = object_struct(STACK_FRAME_INFO),
            sfi_record = class_record(STACK_FRAME_INFO),
            rmn_struct = object_struct(RESOLVED_METHOD_NAME),
            rmn_record = class_record(RESOLVED_METHOD_NAME),
            ste_struct = object_struct(STACK_TRACE_ELEMENT),
            ste_record = class_record(STACK_TRACE_ELEMENT),
            f_member = instance_field("classOrMemberName"),
            f_vmholder = instance_field("vmholder"),
            f_name = instance_field("name"),
            f_flags = instance_field("flags"),
            f_decl_cls = instance_field("declaringClassObject"),
            f_decl_name = instance_field("declaringClass"),
            f_ste_method = instance_field("methodName"),
            f_file = instance_field("fileName"),
            f_line = instance_field("lineNumber"),
        );
    }

    out
}
