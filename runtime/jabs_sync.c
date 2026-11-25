/* Synchronization the JVM provides: object monitors (synchronized, wait, notify) and the atomic
 * accesses behind volatile fields, Unsafe and VarHandles. Handwritten, OS-specific; generated code
 * reaches it only through the declarations in jabs_rt.h. */
#include "jabs_unit.h"
