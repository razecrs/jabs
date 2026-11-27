//! Named modules: `Class.getModule` answers with a JDK class's real module (java.base,
//! java.sql, ...), built from the JDK's own module-info.class files, so the JDK's Java code
//! (AccessibleObject.checkCanSetAccessible, Reflection.verifyModuleAccess, Lookup's access
//! checks, ServiceLoader's `uses` check) decides accessibility exactly as on the JVM. Input
//! classes stay in the app class loader's unnamed module, as on the JVM's class path.
//!
//! Evidence and division of labour:
//! - Build time (here): every jmod's module-info.class (`jdk::modules`). A module is *present*
//!   when the unit has at least one non-input class in one of its packages (ModulePackages); only
//!   present modules get Module objects, because only they can be reached at run time (through a
//!   class's getModule). Readability is computed here the way java.lang.module.Configuration
//!   resolves it: a module reads each module it requires, and every module those require
//!   transitively (implied readability), through absent modules too.
//! - Run time (runtime/jabs_modules.c): on the first getModule of a JDK class, under a once-gate,
//!   each present module's descriptor is parsed by the JDK's own `ModuleDescriptor.read` from the
//!   embedded module-info bytes, its Module is made with the JDK's own constructor, `reads` is set
//!   to the computed modules, and its exports and opens are set by the JDK's own
//!   `Module.initExports` / `initExportsAndOpens`, exactly as `Module.defineModules` does for the
//!   boot layer. Afterwards the Module objects are never changed by JABS (the JDK's own
//!   addExports/addOpens still work through Module.ReflectionData).
//!
//! JDK 25 defaults: no `--add-opens` / `--add-exports` / `--add-reads`, so nothing beyond the
//! module declarations is open to the unnamed module.
//!
//! Known differences from the JVM, on purpose or not yet done:
//! - Module.getLayer() is null and ModuleLayer.boot() stays null (no Configuration is built).
//! - Module.getClassLoader() is null for every JDK module (JABS's Class.getClassLoader is null for
//!   every JDK class too); the JVM gives platform modules (java.sql, ...) the platform loader.
//! - `requires static` counts as a read edge only when the required module is present.
//! - The VM-side module table (defineModule0, addReads0, addExports0...) is not kept: bytecode
//!   linkage is not checked against module exports (the JVM throws IllegalAccessError when an
//!   input class links to a public class of a non-exported package). Only reflection and
//!   method-handle checks, which the JDK's Java code makes, see modules.
//! - The Module objects have no location (the JVM's is `jrt:/<name>`).

use super::*;
use crate::jdk::JdkModule;

const MODULE: &str = "java/lang/Module";
const MODULES: &str = "jdk/internal/module/Modules";
const OPTIONAL: &str = "java/util/Optional";
const MODULE_DESCRIPTOR: &str = "java/lang/module/ModuleDescriptor";
const BYTE_ARRAY_INPUT_STREAM: &str = "java/io/ByteArrayInputStream";
const HASH_MAP: &str = "java/util/HashMap";
const HASH_SET: &str = "java/util/HashSet";
const ARRAY_LIST: &str = "java/util/ArrayList";
const FIND_LOADED_MODULE_DESC: &str = "(Ljava/lang/String;)Ljava/util/Optional;";
const OF_NULLABLE_DESC: &str = "(Ljava/lang/Object;)Ljava/util/Optional;";

const READ: (&str, &str) = (
    "read",
    "(Ljava/io/InputStream;)Ljava/lang/module/ModuleDescriptor;",
);
const MODULE_INIT: &str = "(Ljava/lang/ModuleLayer;Ljava/lang/ClassLoader;Ljava/lang/module/ModuleDescriptor;Ljava/net/URI;)V";
const PUT: (&str, &str) = (
    "put",
    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
);
const ADD: (&str, &str) = ("add", "(Ljava/lang/Object;)Z");
const INIT_EXPORTS: (&str, &str) = ("initExports", "(Ljava/lang/Module;Ljava/util/Map;)V");
const INIT_EXPORTS_AND_OPENS: (&str, &str) = (
    "initExportsAndOpens",
    "(Ljava/lang/Module;Ljava/util/Map;Ljava/util/Map;Ljava/util/List;)V",
);

/// The Java methods the module glue calls from C: (class, name, descriptor). Every one is the
/// JDK's own code doing what Module.defineModules does for the boot layer.
const JAVA_CALLS: [(&str, &str, &str); 10] = [
    (BYTE_ARRAY_INPUT_STREAM, "<init>", "([B)V"),
    (MODULE_DESCRIPTOR, READ.0, READ.1),
    (MODULE, "<init>", MODULE_INIT),
    (HASH_MAP, "<init>", "()V"),
    (HASH_MAP, PUT.0, PUT.1),
    (HASH_SET, "<init>", "()V"),
    (HASH_SET, ADD.0, ADD.1),
    (ARRAY_LIST, "<init>", "()V"),
    (MODULE, INIT_EXPORTS.0, INIT_EXPORTS.1),
    (MODULE, INIT_EXPORTS_AND_OPENS.0, INIT_EXPORTS_AND_OPENS.1),
];

/// Most modules one program may have; JDK 25 has 70. The runtime has the same cap
/// (JABS_MAX_MODULES in runtime/jabs_modules.c).
const MAX_MODULES: usize = 1024;

/// Java methods the module glue calls from C once Class.getModule is reachable and the program
/// has module data, so reachable() keeps them.
pub(super) fn called_by(
    key: &MethodKey,
    has_modules: bool,
) -> Vec<(&'static str, &'static str, &'static str)> {
    if key.0 == MODULES && key.1 == "findLoadedModule" && key.2 == FIND_LOADED_MODULE_DESC {
        let mut deps = vec![(OPTIONAL, "ofNullable", OF_NULLABLE_DESC)];
        if has_modules {
            deps.extend_from_slice(&JAVA_CALLS);
        }
        return deps;
    }
    let get_module = key.0 == CLASS && key.1 == "getModule" && key.2 == "()Ljava/lang/Module;";
    if !get_module || !has_modules {
        return Vec::new();
    }
    JAVA_CALLS.to_vec()
}

/// Java methods replaced for module queries: `Modules.findLoadedModule` queries the modules
/// built by `runtime/jabs_modules.c` (since `ModuleLayer.boot()` is not populated).
pub(super) fn replacement(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if key.0 == MODULES && key.1 == "findLoadedModule" && key.2 == FIND_LOADED_MODULE_DESC {
        return classes_available.then(|| ("return jabs_modules_find_loaded(a0);".into(), true));
    }
    None
}

/// Module's natives that tell HotSpot's own module table what the Java side decided. JABS keeps
/// no such table (see the module comment): every access decision is made by the JDK's Java code
/// from the Module objects, so these have nothing to record.
pub(super) fn intrinsic(key: &MethodKey) -> Option<(String, bool)> {
    if key.0 != MODULE {
        return None;
    }
    let body = match (key.1.as_str(), key.2.as_str()) {
        (
            "defineModule0",
            "(Ljava/lang/Module;ZLjava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V",
        )
        | ("addReads0", "(Ljava/lang/Module;Ljava/lang/Module;)V")
        | ("addExports0", "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V")
        | ("addExportsToAll0", "(Ljava/lang/Module;Ljava/lang/String;)V")
        | ("addExportsToAllUnnamed0", "(Ljava/lang/Module;Ljava/lang/String;)V") => {
            "/* no VM-side module table: the Module objects are the only record (src/cgen/modules.rs) */"
        }
        _ => return None,
    };
    Some((body.to_string(), false))
}

/// One module the program has Module objects for.
struct Present<'a> {
    module: &'a JdkModule,
    /// Indexes (into the present list) of the modules it reads, itself excluded.
    reads: Vec<usize>,
}

/// The present modules (sorted by name) and the package table: (package, present index),
/// sorted by package bytes, as the runtime's strcmp bsearch needs.
struct Plan<'a> {
    present: Vec<Present<'a>>,
    packages: Vec<(&'a str, usize)>,
}

/// The package of an internal class name ("java/lang" for "java/lang/Object"); None for the
/// default package.
fn package_of(class: &str) -> Option<&str> {
    class.rfind('/').map(|at| &class[..at])
}

/// Which modules are present and what each reads. Panics on JDK data no JVM would accept (a
/// package in two modules, a duplicate name): that is evidence JABS misread the JDK.
fn plan<'a>(
    jdk: &'a [JdkModule],
    classes: &[ClassFile],
    app_classes: &BTreeSet<String>,
) -> Plan<'a> {
    if jdk.len() > MAX_MODULES {
        panic!(
            "jabs: the JDK has {} modules; limit is {MAX_MODULES}",
            jdk.len()
        );
    }
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (index, module) in jdk.iter().enumerate() {
        for package in &module.packages {
            if let Some(other) = owner.insert(package.as_str(), index) {
                panic!(
                    "jabs: package {package} is in modules {} and {}; a JVM refuses that",
                    jdk[other].declaration.name, module.declaration.name
                );
            }
        }
    }
    let by_name: HashMap<&str, usize> = jdk
        .iter()
        .enumerate()
        .map(|(i, m)| (m.declaration.name.as_str(), i))
        .collect();
    if by_name.len() != jdk.len() {
        panic!("jabs: a JDK module name is declared twice");
    }
    let mut used = vec![false; jdk.len()];
    for class in classes {
        if app_classes.contains(&class.name) {
            continue;
        }
        let Some(package) = package_of(&class.name) else {
            continue;
        };
        if let Some(&index) = owner.get(package) {
            used[index] = true;
        }
    }
    // Present modules keep the jdk order (by name); position maps a jdk index to its present one.
    let mut position = vec![usize::MAX; jdk.len()];
    let mut present: Vec<Present> = Vec::new();
    for (index, module) in jdk.iter().enumerate() {
        if used[index] {
            position[index] = present.len();
            present.push(Present {
                module,
                reads: Vec::new(),
            });
        }
    }
    // Readability (java.lang.module.Configuration): each `requires` (a static one only when the
    // required module is present), then implied readability through `requires transitive`,
    // followed through absent modules too. The worklist visits each module at most once.
    for (at, entry) in present.iter_mut().enumerate() {
        let mut reads = vec![false; jdk.len()];
        let mut work: Vec<usize> = Vec::new();
        for requires in &entry.module.declaration.requires {
            let Some(&target) = by_name.get(requires.module.as_str()) else {
                // A requires of a module this JDK lacks: it cannot be read.
                continue;
            };
            if requires.flags & crate::classfile::ACC_STATIC_PHASE != 0 && !used[target] {
                continue;
            }
            if !reads[target] {
                reads[target] = true;
                work.push(target);
            }
        }
        let mut steps = 0usize;
        while let Some(next) = work.pop() {
            steps += 1;
            if steps > jdk.len() {
                panic!("jabs: implied readability visited a module twice (src/cgen/modules.rs)");
            }
            for requires in &jdk[next].declaration.requires {
                if requires.flags & crate::classfile::ACC_TRANSITIVE == 0 {
                    continue;
                }
                let Some(&target) = by_name.get(requires.module.as_str()) else {
                    continue;
                };
                if requires.flags & crate::classfile::ACC_STATIC_PHASE != 0 && !used[target] {
                    continue;
                }
                if !reads[target] {
                    reads[target] = true;
                    work.push(target);
                }
            }
        }
        entry.reads = reads
            .iter()
            .enumerate()
            .filter(|(i, r)| **r && position[*i] != usize::MAX && position[*i] != at)
            .map(|(i, _)| position[i])
            .collect();
    }
    let mut packages: Vec<(&str, usize)> = Vec::new();
    for (at, entry) in present.iter().enumerate() {
        for package in &entry.module.packages {
            packages.push((package.as_str(), at));
        }
    }
    packages.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for w in packages.windows(2) {
        assert!(
            w[0].0 != w[1].0,
            "package {} declared by multiple present modules (src/cgen/modules.rs)",
            w[0].0
        );
    }
    Plan { present, packages }
}

/// A C string literal of arbitrary bytes (octal escapes), split so no literal piece is huge.
fn byte_literal(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 4096 == 0 {
            out.push_str("\"\n\"");
        }
        let _ = write!(out, "\\{b:03o}");
    }
    out.push('"');
    out
}

/// The module tables runtime/jabs_modules.c reads (declared in runtime/jabs_modules.h). Without
/// module data (`UnitExtras::jdk_modules` empty) the tables are empty and every class keeps the
/// unnamed module of its loader, as before named modules existed.
pub(super) fn tables(
    jdk: &[JdkModule],
    classes: &[ClassFile],
    app_classes: &BTreeSet<String>,
) -> String {
    let plan = plan(jdk, classes, app_classes);
    let mut out = String::from("\n/* ---- Named modules (src/cgen/modules.rs) ---- */\n");
    for (i, entry) in plan.present.iter().enumerate() {
        let bytes = &entry.module.class_bytes;
        let _ = writeln!(
            out,
            "static const unsigned char jabs_module_info_{i}[{}] = {};",
            bytes.len() + 1,
            byte_literal(bytes)
        );
    }
    out.push_str("const int32_t jabs_module_reads[] = {");
    let mut starts = Vec::new();
    let mut total = 0usize;
    for entry in &plan.present {
        starts.push(total);
        for r in &entry.reads {
            let _ = write!(out, " {r},");
            total += 1;
        }
    }
    // C has no empty arrays: a sentinel no entry points at.
    out.push_str(" -1 };\n");
    let total_i32 = i32::try_from(total).expect("module read count fits in i32");
    let _ = writeln!(out, "const int32_t jabs_module_read_count = {total_i32};");
    out.push_str("const jabs_module_def jabs_module_defs[] = {\n");
    for (i, entry) in plan.present.iter().enumerate() {
        let declaration = &entry.module.declaration;
        let open = declaration.flags & crate::classfile::ACC_OPEN != 0;
        let info_len =
            i32::try_from(entry.module.class_bytes.len()).expect("module-info fits in i32");
        let start_i32 = i32::try_from(starts[i]).expect("module read start fits in i32");
        let reads_len = i32::try_from(entry.reads.len()).expect("module reads count fits in i32");
        let _ = writeln!(
            out,
            "    {{ \"{}\", jabs_module_info_{i}, {}, {}, {}, {}, {} }},",
            c_escape(&declaration.name),
            info_len,
            start_i32,
            reads_len,
            i32::from(open),
            i32::from(!declaration.opens.is_empty()),
        );
    }
    out.push_str("    { NULL, NULL, 0, 0, 0, 0, 0 }\n};\n");
    let def_count = i32::try_from(plan.present.len()).expect("module def count fits in i32");
    let _ = writeln!(out, "const int32_t jabs_module_def_count = {def_count};");
    out.push_str("const jabs_module_package jabs_module_packages[] = {\n");
    for (package, at) in &plan.packages {
        let _ = writeln!(out, "    {{ \"{}\", {at} }},", c_escape(package));
    }
    out.push_str("    { NULL, -1 }\n};\n");
    let pkg_count = i32::try_from(plan.packages.len()).expect("module package count fits in i32");
    let _ = writeln!(
        out,
        "const int32_t jabs_module_package_count = {pkg_count};"
    );
    out
}

/// C glue runtime/jabs_modules.c calls to run the JDK's own module code (JAVA_CALLS). A call
/// whose method, class record or initializer is not in the program stops when it is reached.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let has = |class: &str, name: &str, descriptor: &str| {
        unit.get(class).is_some()
            && records.contains(class)
            && inits.contains(class)
            && methods.contains(&(class.to_string(), name.to_string(), descriptor.to_string()))
    };
    let has_field = |class: &str, field: &str, descriptor: &str| {
        records.contains(class)
            && unit.chain(class).is_ok()
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == field && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    // A stub that stops; its parameters are cast to void (-Wunused-parameter is an error).
    let missing = |signature: &str, what: &str| {
        let mut unused = String::new();
        let params = signature
            .split_once('(')
            .and_then(|(_, rest)| rest.rsplit_once(')'))
            .map_or("", |(params, _)| params);
        for param in params.split(',').map(str::trim) {
            let name = param.rsplit([' ', '*']).next().unwrap_or("");
            if !name.is_empty() && name != "void" {
                let _ = write!(unused, "(void){name}; ");
            }
        }
        format!(
            "{signature} {{ {unused}jabs_not_compiled(\"{what} is not in the program (module glue, src/cgen/modules.rs)\"); }}\n"
        )
    };
    let mut out = String::from("\n/* ---- Module glue (src/cgen/modules.rs) ---- */\n");

    let signature =
        "jabs_object *jabs_modules_read_descriptor(const unsigned char *bytes, int32_t length)";
    if has(BYTE_ARRAY_INPUT_STREAM, "<init>", "([B)V") && has(MODULE_DESCRIPTOR, READ.0, READ.1) {
        let _ = writeln!(
            out,
            "/* ModuleDescriptor.read(new ByteArrayInputStream(module-info bytes)): the JDK's own parser. */
{signature} {{
    if (!bytes || length < 0) jabs_not_compiled(\"jabs_modules_read_descriptor without module-info bytes\");
    jabs_object *array = jabs_new_array(length, 1, JABS_COMPONENT_PRIMITIVE, NULL, \"[B\", NULL);
    if (!array) return NULL;
    memcpy(jabs_array_data((jabs_array *)array), bytes, (size_t)length);
    if ({stream_init}() != 0) return NULL;
    jabs_object *in = jabs_new(&{stream_record});
    if (!in) return NULL;
    {stream_ctor}(in, array);
    if (jabs_pending) return NULL;
    if ({descriptor_init}() != 0) return NULL;
    jabs_object *descriptor = {read}(in);
    return jabs_pending ? NULL : descriptor;
}}",
            stream_init = init_symbol(BYTE_ARRAY_INPUT_STREAM),
            stream_record = class_record(BYTE_ARRAY_INPUT_STREAM),
            stream_ctor = symbol(BYTE_ARRAY_INPUT_STREAM, "<init>", "([B)V"),
            descriptor_init = init_symbol(MODULE_DESCRIPTOR),
            read = symbol(MODULE_DESCRIPTOR, READ.0, READ.1),
        );
    } else {
        out.push_str(&missing(
            signature,
            "ModuleDescriptor.read or ByteArrayInputStream",
        ));
    }

    let signature = "jabs_object *jabs_modules_new_module(jabs_object *descriptor)";
    if has(MODULE, "<init>", MODULE_INIT) {
        let _ = writeln!(
            out,
            "/* new Module(null layer, null (boot) loader, descriptor, null location): the constructor
 * Module.defineModules uses. */
{signature} {{
    if ({init}() != 0) return NULL;
    jabs_object *module = jabs_new(&{record});
    if (!module) return NULL;
    {ctor}(module, NULL, NULL, descriptor, NULL);
    return jabs_pending ? NULL : module;
}}",
            init = init_symbol(MODULE),
            record = class_record(MODULE),
            ctor = symbol(MODULE, "<init>", MODULE_INIT),
        );
    } else {
        out.push_str(&missing(signature, "Module's constructor"));
    }

    let signature = "jabs_object *jabs_modules_new_map(void)";
    if has(HASH_MAP, "<init>", "()V") {
        let _ = writeln!(
            out,
            "{signature} {{
    if ({init}() != 0) return NULL;
    jabs_object *map = jabs_new(&{record});
    if (!map) return NULL;
    {ctor}(map);
    return jabs_pending ? NULL : map;
}}",
            init = init_symbol(HASH_MAP),
            record = class_record(HASH_MAP),
            ctor = symbol(HASH_MAP, "<init>", "()V"),
        );
    } else {
        out.push_str(&missing(signature, "HashMap's constructor"));
    }

    let signature =
        "int32_t jabs_modules_map_put(jabs_object *map, const char *name, jabs_object *module)";
    if has(HASH_MAP, PUT.0, PUT.1) {
        let _ = writeln!(
            out,
            "/* map.put(name, module); 0, or -1 with an exception pending. The map is a HashMap this
 * glue made, so HashMap.put is the method a virtual call would pick. */
{signature} {{
    jabs_object *key = jabs_string_from_utf8(name);
    if (!key) return -1;
    (void){put}(map, key, module);
    return jabs_pending ? -1 : 0;
}}",
            put = symbol(HASH_MAP, PUT.0, PUT.1),
        );
    } else {
        out.push_str(&missing(signature, "HashMap.put"));
    }

    let signature = "int32_t jabs_modules_set_reads(jabs_object *module, jabs_object *const *reads, int32_t count)";
    if has(HASH_SET, "<init>", "()V")
        && has(HASH_SET, ADD.0, ADD.1)
        && has_field(MODULE, "reads", "Ljava/util/Set;")
    {
        let _ = writeln!(
            out,
            "/* module.reads = a HashSet of the modules it reads, as Module.defineModules sets it; 0, or
 * -1 with an exception pending. Written before any other thread can see the module (the
 * module gate in runtime/jabs_modules.c publishes it). */
{signature} {{
    if (count < 0 || (count > 0 && !reads)) jabs_not_compiled(\"jabs_modules_set_reads with a bad read list\");
    if ({init}() != 0) return -1;
    jabs_object *set = jabs_new(&{record});
    if (!set) return -1;
    {ctor}(set);
    if (jabs_pending) return -1;
    for (int32_t i = 0; i < count; i++) {{
        (void){add}(set, reads[i]);
        if (jabs_pending) return -1;
    }}
    (({module_struct} *)module)->{reads_field} = set;
    return 0;
}}",
            init = init_symbol(HASH_SET),
            record = class_record(HASH_SET),
            ctor = symbol(HASH_SET, "<init>", "()V"),
            add = symbol(HASH_SET, ADD.0, ADD.1),
            module_struct = object_struct(MODULE),
            reads_field = instance_field("reads"),
        );
    } else {
        out.push_str(&missing(signature, "HashSet or Module.reads"));
    }

    let signature = "int32_t jabs_modules_init_exports(jabs_object *module, jabs_object *name_map, int32_t has_opens)";
    if has(MODULE, INIT_EXPORTS.0, INIT_EXPORTS.1)
        && has(MODULE, INIT_EXPORTS_AND_OPENS.0, INIT_EXPORTS_AND_OPENS.1)
        && has(HASH_MAP, "<init>", "()V")
        && has(ARRAY_LIST, "<init>", "()V")
    {
        let _ = writeln!(
            out,
            "/* The module's exports and opens, by Module.defineModules's boot-layer rule: initExports when
 * the module opens nothing, else initExportsAndOpens with no modules from parent layers (an
 * empty name-to-source map and an empty parent list: the boot layer's only parent, the empty
 * layer, has no modules). 0, or -1 with an exception pending. */
{signature} {{
    if ({module_init}() != 0) return -1;
    if (!has_opens) {{
        {init_exports}(module, name_map);
        return jabs_pending ? -1 : 0;
    }}
    jabs_object *sources = jabs_modules_new_map();
    if (!sources) return -1;
    if ({list_init}() != 0) return -1;
    jabs_object *parents = jabs_new(&{list_record});
    if (!parents) return -1;
    {list_ctor}(parents);
    if (jabs_pending) return -1;
    {init_exports_and_opens}(module, sources, name_map, parents);
    return jabs_pending ? -1 : 0;
}}",
            module_init = init_symbol(MODULE),
            init_exports = symbol(MODULE, INIT_EXPORTS.0, INIT_EXPORTS.1),
            list_init = init_symbol(ARRAY_LIST),
            list_record = class_record(ARRAY_LIST),
            list_ctor = symbol(ARRAY_LIST, "<init>", "()V"),
            init_exports_and_opens =
                symbol(MODULE, INIT_EXPORTS_AND_OPENS.0, INIT_EXPORTS_AND_OPENS.1),
        );
    } else {
        out.push_str(&missing(
            signature,
            "Module.initExports, initExportsAndOpens, HashMap or ArrayList",
        ));
    }

    let find_sig = "jabs_object *jabs_modules_find_loaded(jabs_object *name_obj)";
    if has(OPTIONAL, "ofNullable", OF_NULLABLE_DESC) && inits.contains(OPTIONAL) {
        let _ = writeln!(
            out,
            "{find_sig} {{\n\
             \x20   if (!name_obj) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}\n\
             \x20   if ({opt_init}() != 0) return NULL;\n\
             \x20   char *utf = jabs_string_to_utf8(name_obj);\n\
             \x20   if (!utf) {{ if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}\n\
             \x20   jabs_object *mod = jabs_module_named(utf);\n\
             \x20   free(utf);\n\
             \x20   return {of_nullable}(mod);\n\
             }}",
            opt_init = init_symbol(OPTIONAL),
            of_nullable = symbol(OPTIONAL, "ofNullable", OF_NULLABLE_DESC),
        );
    } else {
        out.push_str(&missing(find_sig, "Optional.ofNullable"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classfile::{ModuleDeclaration, ModuleRequires};

    fn module(name: &str, packages: &[&str], requires: &[(&str, u16)]) -> JdkModule {
        JdkModule {
            declaration: ModuleDeclaration {
                name: name.to_string(),
                flags: 0,
                requires: requires
                    .iter()
                    .map(|(m, flags)| ModuleRequires {
                        module: m.to_string(),
                        flags: *flags,
                    })
                    .collect(),
                exports: Vec::new(),
                opens: Vec::new(),
            },
            packages: packages.iter().map(|p| p.to_string()).collect(),
            resolution: 0,
            class_bytes: Vec::new(),
        }
    }

    fn class(name: &str) -> ClassFile {
        ClassFile {
            minor: 0,
            major: 52,
            pool: Vec::new(),
            access: 0,
            name: name.to_string(),
            super_name: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            string_units: Default::default(),
            bootstrap_methods: Vec::new(),
            reflect: Default::default(),
        }
    }

    /// Implied readability goes through an absent module, a static requires of an absent module
    /// is not read, input classes and default-package classes make no module present.
    #[test]
    fn readability_follows_requires_transitive_through_absent_modules() {
        let transitive = crate::classfile::ACC_TRANSITIVE;
        let jdk = vec![
            module("a", &["a/p"], &[("base", 0), ("b", 0), ("s", 0x40)]),
            module("b", &["b/p"], &[("base", 0), ("c", transitive)]),
            module("base", &["java/lang"], &[]),
            module("c", &["c/p"], &[("base", 0)]),
            module("s", &["s/p"], &[]),
        ];
        let classes = vec![
            class("a/p/A"),
            class("c/p/C"),
            class("java/lang/Object"),
            class("s/p/Input"),
            class("Main"),
        ];
        let app: BTreeSet<String> = ["s/p/Input".to_string()].into_iter().collect();
        let plan = plan(&jdk, &classes, &app);
        let names: Vec<&str> = plan
            .present
            .iter()
            .map(|p| p.module.declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["a", "base", "c"],
            "present modules (plan in src/cgen/modules.rs)"
        );
        // a reads base and c (implied by b, which is absent); not s (static, absent).
        assert_eq!(plan.present[0].reads, vec![1, 2]);
        assert_eq!(plan.present[1].reads, Vec::<usize>::new());
        assert_eq!(plan.present[2].reads, vec![1]);
        let packages: Vec<&str> = plan.packages.iter().map(|p| p.0).collect();
        assert_eq!(packages, ["a/p", "c/p", "java/lang"]);

        // A `requires transitive static` edge to an absent module is not followed into further
        // implied reads, while one to a present module is read.
        let jdk2 = vec![
            module("x", &["x/p"], &[("y", 0)]),
            module(
                "y",
                &["y/p"],
                &[("a", transitive | 0x40), ("p", transitive | 0x40)],
            ),
            module("a", &["a/p"], &[("z", transitive)]),
            module("p", &["p/p"], &[]),
            module("z", &["z/p"], &[]),
        ];
        let plan2 = super::plan(
            &jdk2,
            &[class("x/p/X"), class("p/p/P"), class("z/p/Z")],
            &BTreeSet::new(),
        );
        let x = plan2
            .present
            .iter()
            .find(|e| e.module.declaration.name == "x")
            .unwrap();
        let x_reads: Vec<&str> = x
            .reads
            .iter()
            .map(|&r| plan2.present[r].module.declaration.name.as_str())
            .collect();
        assert_eq!(
            x_reads,
            ["p"],
            "static edge to absent `a` leaked `z`, or present `p` missing: {x_reads:?}"
        );
    }
}
