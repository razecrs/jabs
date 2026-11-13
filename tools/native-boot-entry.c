#include "jabs_unit.h"
#include <stdio.h>
/* Print a thrown object and its cause chain. */
static void report(const char *what, jabs_object *e) {
    printf("%s threw %s (last VM raise in %s)\n", what, e->cls->name, jabs_where);
    for (jabs_object *c = jabs_cause_of(e); c; c = jabs_cause_of(c)) printf("  caused by %s\n", c->cls->name);
}
/* Native entry point: what the JVM's launcher does, minus JVM startup (System.initPhase1 etc.). */
int main(void) {
    jabs_object *args = jabs_new_array(0, sizeof(jabs_object *), JABS_COMPONENT_UNCHECKED, NULL, "[Ljava/lang/String;", NULL);
    if (jc_net_2fminecraft_2fserver_2fMinecraftServer__init() != 0) { report("initializing MinecraftServer", jabs_pending); return 1; }
    jm_net_2fminecraft_2fserver_2fMinecraftServer__main___28_5bLjava_2flang_2fString_3b_29V(args);
    if (jabs_pending) { report("main", jabs_pending); return 1; }
    printf("main returned normally\n");
    return 0;
}
