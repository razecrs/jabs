/* java.lang.management natives' glue (sun.management.VMManagementImpl and friends; the bodies
 * are in src/cgen/management.rs). Handwritten, OS-neutral: OS facts come from jabs_platform.c,
 * thread counts from jabs_thread.c. Needs the class registry (Strings and String[]), so it is
 * part of a program exactly when jabs_io.c is. */
#include "jabs_runtime.h"

/* VMManagementImpl.getVmArguments0: HotSpot's input arguments are the options the VM was
 * created with, one per element as given ("-Dkey=value", "-Dkey"); the launcher's own
 * -Djava.class.path, -Dsun.java.command and -Dsun.java.launcher are left out. A JABS program
 * takes only -D properties (jabs_defines, stored without "-D"), so its input arguments are
 * exactly those, in order. */
jabs_object *jabs_management_vm_arguments(void) {
    if (jabs_define_count < 0 || (jabs_define_count > 0 && !jabs_defines)) {
        jabs_not_compiled("VMManagementImpl.getVmArguments0 with a broken define table (jabs_set_command_line_properties)");
    }
    jabs_object *array = jabs_new_string_array(jabs_define_count);
    if (!array) return NULL;
    for (int i = 0; i < jabs_define_count; i++) {
        const char *define = jabs_defines[i];
        if (!define) jabs_not_compiled("VMManagementImpl.getVmArguments0: a NULL define");
        size_t length = strlen(define);
        if (length > SIZE_MAX - 3) jabs_not_compiled("VMManagementImpl.getVmArguments0: a define too long to copy");
        char *argument = (char *)malloc(length + 3);
        if (!argument) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
        memcpy(argument, "-D", 2);
        memcpy(argument + 2, define, length + 1);
        jabs_object *text = jabs_string_from_utf8(argument);
        free(argument);
        if (!text) return NULL;
        ((jabs_object **)jabs_array_data((jabs_array *)array))[i] = text;
    }
    return array;
}
