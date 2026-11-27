/* Named modules (see src/cgen/modules.rs for the design and the evidence). Class.getModule of a
 * JDK class asks jabs_module_of_class, which finds the class's module by its package in the
 * generated tables and, the first time any thread asks, builds every present module's Module
 * object with the JDK's own code (ModuleDescriptor.read, the Module constructor,
 * Module.initExports / initExportsAndOpens), as Module.defineModules does for the boot layer.
 *
 * Threads: building runs Java code, so it is guarded by the class-initialization gate
 * (jabs_class_init_begin): one thread builds, any other waits until it is done. Once the gate
 * says done, the table of Module objects is never written again; readers see it through the
 * gate's release/acquire pair. A failure while building is a JABS bug or a JDK this runtime does
 * not understand, so it stops the program with the module and step named. */
#include "jabs_unit.h"

/* Most modules one program may have (MAX_MODULES in src/cgen/modules.rs). */
#define JABS_MAX_MODULES 1024
/* Longest package name looked up; a longer class name has no named module (JVMS names may be
 * longer, but no JDK package comes close). */
#define JABS_MAX_PACKAGE 1024

static jabs_class_init jabs_modules_gate;
/* Set (before the gate's release) once every Module is complete. */
static bool jabs_modules_ready;
static jabs_object *jabs_module_objects[JABS_MAX_MODULES];

static _Noreturn void jabs_modules_stop(const char *step, const char *module) {
    const char *thrown = jabs_pending && jabs_pending->cls ? jabs_pending->cls->name : "no exception";
    fprintf(stderr, "jabs: building the named modules: %s for module %s failed (%s); see runtime/jabs_modules.c and src/cgen/modules.rs\n",
            step, module ? module : "?", thrown);
    if (jabs_on_not_compiled) jabs_on_not_compiled();
    abort();
}

static int compare_package(const void *key, const void *entry) {
    return strcmp((const char *)key, ((const jabs_module_package *)entry)->package);
}

/* Build every present module; stops on any failure (never returns partly built). */
static void jabs_modules_build(void) {
    int32_t count = jabs_module_def_count;
    if (count < 0 || count > JABS_MAX_MODULES) jabs_modules_stop("checking the module count", NULL);
    for (int32_t i = 0; i < count; i++) {
        const jabs_module_def *def = &jabs_module_defs[i];
        jabs_object *descriptor = jabs_modules_read_descriptor(def->module_info, def->module_info_length);
        if (!descriptor) jabs_modules_stop("ModuleDescriptor.read", def->name);
        jabs_object *module = jabs_modules_new_module(descriptor);
        if (!module) jabs_modules_stop("new Module", def->name);
        jabs_module_objects[i] = module;
    }
    jabs_object *names = jabs_modules_new_map();
    if (!names) jabs_modules_stop("new HashMap", NULL);
    for (int32_t i = 0; i < count; i++) {
        if (jabs_modules_map_put(names, jabs_module_defs[i].name, jabs_module_objects[i]) != 0) {
            jabs_modules_stop("HashMap.put", jabs_module_defs[i].name);
        }
    }
    jabs_object *reads[JABS_MAX_MODULES];
    for (int32_t i = 0; i < count; i++) {
        const jabs_module_def *def = &jabs_module_defs[i];
        if (def->reads_count < 0 || def->reads_count > count || def->reads_start < 0 ||
            def->reads_start > jabs_module_read_count - def->reads_count) {
            jabs_modules_stop("checking the read list", def->name);
        }
        for (int32_t j = 0; j < def->reads_count; j++) {
            int32_t target = jabs_module_reads[def->reads_start + j];
            if (target < 0 || target >= count || target == i) jabs_modules_stop("checking a read edge", def->name);
            reads[j] = jabs_module_objects[target];
        }
        if (jabs_modules_set_reads(jabs_module_objects[i], reads, def->reads_count) != 0) jabs_modules_stop("setting Module.reads", def->name);
        /* Module.defineModules: open (and automatic) modules get no exports or opens maps; every
         * package of theirs is open through the descriptor. */
        if (def->open) continue;
        if (jabs_modules_init_exports(jabs_module_objects[i], names, def->has_opens) != 0) {
            jabs_modules_stop("Module.initExports", def->name);
        }
    }
}

static void jabs_modules_ensure_ready(const char *what) {
    if (jabs_class_initialized(&jabs_modules_gate)) return;
    switch (jabs_class_init_begin(&jabs_modules_gate)) {
    case JABS_INIT_RUN: {
        /* Building must not see a caller's pending exception as its own failure. */
        jabs_object *pending = jabs_pending;
        jabs_pending = NULL;
        jabs_modules_build();
        jabs_pending = pending;
        jabs_modules_ready = true;
        jabs_runtime_once_end(&jabs_modules_gate, true);
        break;
    }
    case JABS_INIT_DONE:
        /* Done, or this thread is building them and the JDK code it runs asked for a
         * module: there is no answer yet that the JVM would give, so stop. */
        if (!jabs_modules_ready) jabs_modules_stop("querying modules while they are being built", what);
        break;
    default:
        jabs_modules_stop("waiting for another thread to build the modules", what);
    }
}

const char *jabs_module_name_of_package(const char *package) {
    if (!package || jabs_module_package_count <= 0) return NULL;
    const jabs_module_package *found = (const jabs_module_package *)bsearch(
        package, jabs_module_packages, (size_t)jabs_module_package_count, sizeof *jabs_module_packages, compare_package);
    if (!found) return NULL;
    if (found->module < 0 || found->module >= jabs_module_def_count || found->module >= JABS_MAX_MODULES) {
        jabs_modules_stop("looking up a package", package);
    }
    return jabs_module_defs[found->module].name;
}

jabs_object *jabs_module_named(const char *name) {
    if (!name || jabs_module_def_count <= 0) return NULL;
    for (int32_t i = 0; i < jabs_module_def_count; i++) {
        if (strcmp(jabs_module_defs[i].name, name) == 0) {
            jabs_modules_ensure_ready(name);
            return jabs_module_objects[i];
        }
    }
    return NULL;
}

jabs_object *jabs_module_of_class(const char *name) {
    if (!name || jabs_module_package_count <= 0) return NULL;
    const char *slash = strrchr(name, '/');
    if (!slash) return NULL; /* the default package belongs to no named module */
    size_t length = (size_t)(slash - name);
    char stack_pkg[JABS_MAX_PACKAGE];
    char *heap_pkg = NULL;
    char *package = length < sizeof stack_pkg ? stack_pkg : (heap_pkg = (char *)malloc(length + 1));
    if (!package) jabs_modules_stop("looking up a package (out of memory)", name);
    memcpy(package, name, length);
    package[length] = 0;
    const jabs_module_package *found = (const jabs_module_package *)bsearch(
        package, jabs_module_packages, (size_t)jabs_module_package_count, sizeof *jabs_module_packages, compare_package);
    free(heap_pkg);
    if (!found) return NULL;
    if (found->module < 0 || found->module >= jabs_module_def_count || found->module >= JABS_MAX_MODULES) {
        jabs_modules_stop("looking up a package", name);
    }
    jabs_modules_ensure_ready(name);
    return jabs_module_objects[found->module];
}
