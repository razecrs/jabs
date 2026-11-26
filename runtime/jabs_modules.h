/* Named modules: the tables src/cgen/modules.rs generates from the JDK's module-info.class
 * files, read by runtime/jabs_modules.c, which builds the Module objects with the JDK's own code
 * through the generated glue below. Part of every program's header. */

/* One present module: its name, its module-info.class, the modules it reads (a slice of
 * jabs_module_reads, by index into jabs_module_defs), whether it is an open module (which gets
 * no exports or opens maps, as in Module.defineModules) and whether it declares any `opens`. */
typedef struct {
    const char *name;
    const unsigned char *module_info;
    int32_t module_info_length;
    int32_t reads_start, reads_count;
    int32_t open;
    int32_t has_opens;
} jabs_module_def;
/* A package (internal form, "java/lang") and the index of its module; sorted by strcmp. */
typedef struct {
    const char *package;
    int32_t module;
} jabs_module_package;
/* Each table ends with a sentinel entry (C has no empty arrays); the counts exclude it. */
extern const jabs_module_def jabs_module_defs[];
extern const int32_t jabs_module_def_count;
extern const int32_t jabs_module_reads[];
extern const int32_t jabs_module_read_count;
extern const jabs_module_package jabs_module_packages[];
extern const int32_t jabs_module_package_count;

/* The named Module of a JDK class (internal name), built on first use; NULL with nothing
 * pending when its package is in no present module (input classes, the default package, a
 * program without module data), NULL with an exception pending only on out of memory. */
jabs_object *jabs_module_of_class(const char *name);

/* The JDK's own module code, called from C (generated: modules::glue in src/cgen/modules.rs).
 * Each returns NULL / -1 with an exception pending on failure. */
jabs_object *jabs_modules_read_descriptor(const unsigned char *bytes, int32_t length);
jabs_object *jabs_modules_new_module(jabs_object *descriptor);
jabs_object *jabs_modules_new_map(void);
int32_t jabs_modules_map_put(jabs_object *map, const char *name, jabs_object *module);
int32_t jabs_modules_set_reads(jabs_object *module, jabs_object *const *reads, int32_t count);
int32_t jabs_modules_init_exports(jabs_object *module, jabs_object *name_map, int32_t has_opens);
