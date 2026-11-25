#include "jabs_unit.h"
#include <stdio.h>
/* A throwable's message (detailMessage), if it is plain ASCII. */
static const char *message_of(jabs_object *e) {
    static char text[1024];
    jabs_object *m = ((struct jo_java_2flang_2fThrowable *)e)->f_detailMessage;
    return m && jabs_string_to_ascii(m, text, sizeof text) ? text : "";
}

/* Print a thrown object and its cause chain. */
static void report(const char *what, jabs_object *e) {
    printf("%s threw %s: %s (last VM raise in %s)\n", what, e->cls->name, message_of(e), jabs_where);
    for (jabs_object *c = jabs_cause_of(e); c; c = jabs_cause_of(c)) printf("  caused by %s: %s\n", c->cls->name, message_of(c));
}
/* Native entry point: what the JVM's launcher does. Leading -Dkey=value arguments are system
 * properties and "--" ends them, as before the class name on a java command line; the rest are
 * main's arguments. Then JVM startup (System.initPhase1), then main. */
#define MAX_DEFINES 256
int main(int argc, char **argv) {
    static const char *defines[MAX_DEFINES];
    int first = 1, count = 0;
    while (first < argc && strncmp(argv[first], "-D", 2) == 0) {
        if (count == MAX_DEFINES) { printf("more than %d -D properties\n", MAX_DEFINES); return 2; }
        defines[count++] = argv[first++] + 2;
    }
    if (first < argc && strcmp(argv[first], "--") == 0) first++;
    if (jabs_set_command_line_properties(count, defines) != 0) { printf("a -D property has no key\n"); return 2; }
    if (jabs_vm_startup() != 0) { report("JVM startup (System.initPhase1)", jabs_pending); return 1; }
    jabs_object *args = jabs_new_array(argc - first, sizeof(jabs_object *), JABS_COMPONENT_UNCHECKED, NULL, "[Ljava/lang/String;", NULL);
    if (!args) { printf("out of memory building the arguments\n"); return 1; }
    for (int i = first; i < argc; i++) {
        jabs_object *s = jabs_string_from_utf8(argv[i]);
        if (!s) { printf("out of memory building the arguments\n"); return 1; }
        ((jabs_object **)jabs_array_data((jabs_array *)args))[i - first] = s;
    }
    if (jc_net_2fminecraft_2fserver_2fMinecraftServer__init() != 0) { report("initializing MinecraftServer", jabs_pending); return 1; }
    jm_net_2fminecraft_2fserver_2fMinecraftServer__main___28_5bLjava_2flang_2fString_3b_29V(args);
    int status = 0;
    if (jabs_pending) { report("main", jabs_pending); status = 1; }
    else printf("main returned normally; waiting for non-daemon threads\n");
    fflush(stdout);
    /* As the java launcher does: the server runs on its own "Server thread" after main returns. */
    jabs_destroy_java_vm();
    return status;
}
