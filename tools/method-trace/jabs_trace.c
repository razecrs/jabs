/* A JVMTI agent that records every method a JVM runs, once each, natives included: evidence of
 * what a real run executed, for `jabs coverage --executed-methods`. It is a development tool; it
 * never ships in a JABS program.
 *
 * Load:   java -agentpath:<path to this library>=<output file> ...
 * Build:  clang -shared -O2 -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/win32" jabs_trace.c -o jabs_trace.dll
 *         (include/linux and a .so elsewhere)
 *
 * Output: the line "# jabs method trace 1", then one "class<TAB>name<TAB>descriptor" line per
 * method in the order each first ran (class as an internal name; hidden classes keep the JVM's
 * "a/B$$Lambda.0x..." form), then "# complete <count>" when the VM dies. A file without that
 * last line is incomplete: the run was killed, or the agent stopped because a lookup failed or
 * its table filled, and readers must refuse it. */
#include <jvmti.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define TRACE_HEADER "# jabs method trace 1\n"
#define TABLE_BITS 23
#define TABLE_SIZE ((size_t)1 << TABLE_BITS)
/* At most half the slots are used, so a probe always ends quickly at an empty slot. */
#define MAX_METHODS (TABLE_SIZE / 2)

static jvmtiEnv *jvmti;
static jrawMonitorID lock;
static jmethodID *table;
static size_t count;
static FILE *out;
/* Set when anything goes wrong; from then on nothing is recorded and no trailer is written. */
static int failed;

static size_t slot_of(jmethodID m) {
    uint64_t h = (uint64_t)(uintptr_t)m;
    h ^= h >> 33;
    h *= UINT64_C(0xff51afd7ed558ccd);
    h ^= h >> 33;
    return (size_t)(h & (TABLE_SIZE - 1));
}

/* 1 if m is new (and now recorded in the table), 0 if already seen, -1 if the table is full.
 * The caller holds the lock. */
static int insert(jmethodID m) {
    size_t i = slot_of(m);
    for (size_t probe = 0; probe < TABLE_SIZE; probe++) {
        if (table[i] == m) return 0;
        if (table[i] == NULL) {
            if (count >= MAX_METHODS) return -1;
            table[i] = m;
            count++;
            return 1;
        }
        i = (i + 1) & (TABLE_SIZE - 1);
    }
    return -1;
}

static void release(void *p) {
    if (p) (*jvmti)->Deallocate(jvmti, (unsigned char *)p);
}

/* Write one method's line; 0 on any failure. The caller holds the lock. */
static int write_method(JNIEnv *jni, jmethodID m) {
    jclass cls = NULL;
    char *class_sig = NULL, *name = NULL, *sig = NULL;
    int ok = 0;
    if ((*jvmti)->GetMethodDeclaringClass(jvmti, m, &cls) != JVMTI_ERROR_NONE) goto done;
    if ((*jvmti)->GetClassSignature(jvmti, cls, &class_sig, NULL) != JVMTI_ERROR_NONE) goto done;
    if ((*jvmti)->GetMethodName(jvmti, m, &name, &sig, NULL) != JVMTI_ERROR_NONE) goto done;
    size_t n = strlen(class_sig);
    /* A declaring class is never an array or a primitive, so its signature is "Lname;". */
    if (n < 3 || class_sig[0] != 'L' || class_sig[n - 1] != ';') goto done;
    if (n - 2 > INT32_MAX) goto done;
    ok = fprintf(out, "%.*s\t%s\t%s\n", (int)(n - 2), class_sig + 1, name, sig) > 0;
done:
    release(class_sig);
    release(name);
    release(sig);
    if (cls && jni) (*jni)->DeleteLocalRef(jni, cls);
    return ok;
}

static void JNICALL on_method_entry(jvmtiEnv *env, JNIEnv *jni, jthread thread, jmethodID m) {
    (void)env;
    (void)thread;
    if ((*jvmti)->RawMonitorEnter(jvmti, lock) != JVMTI_ERROR_NONE) return;
    if (out && !failed) {
        int added = insert(m);
        if (added < 0 || (added == 1 && !write_method(jni, m))) failed = 1;
    }
    (*jvmti)->RawMonitorExit(jvmti, lock);
}

static void JNICALL on_vm_death(jvmtiEnv *env, JNIEnv *jni) {
    (void)env;
    (void)jni;
    if ((*jvmti)->RawMonitorEnter(jvmti, lock) != JVMTI_ERROR_NONE) return;
    if (out) {
        int ok = !failed && fprintf(out, "# complete %llu\n", (unsigned long long)count) > 0;
        if (fclose(out) != 0 || !ok) fprintf(stderr, "jabs_trace: trace is incomplete\n");
        out = NULL;
    }
    (*jvmti)->RawMonitorExit(jvmti, lock);
}

JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM *vm, char *options, void *reserved) {
    (void)reserved;
    if (!options || !options[0]) {
        fprintf(stderr, "jabs_trace: give the output file: -agentpath:<library>=<file>\n");
        return JNI_ERR;
    }
    if ((*vm)->GetEnv(vm, (void **)&jvmti, JVMTI_VERSION_1_2) != JNI_OK) {
        fprintf(stderr, "jabs_trace: this JVM has no JVMTI 1.2\n");
        return JNI_ERR;
    }
    jvmtiCapabilities caps;
    memset(&caps, 0, sizeof caps);
    caps.can_generate_method_entry_events = 1;
    if ((*jvmti)->AddCapabilities(jvmti, &caps) != JVMTI_ERROR_NONE) {
        fprintf(stderr, "jabs_trace: method entry events are not available\n");
        return JNI_ERR;
    }
    if ((*jvmti)->CreateRawMonitor(jvmti, "jabs_trace", &lock) != JVMTI_ERROR_NONE) return JNI_ERR;
    table = (jmethodID *)calloc(TABLE_SIZE, sizeof *table);
    if (!table) {
        fprintf(stderr, "jabs_trace: out of memory\n");
        return JNI_ERR;
    }
    /* Binary, so the file's bytes are the same on every OS. */
    out = fopen(options, "wb");
    if (!out || fputs(TRACE_HEADER, out) < 0) {
        fprintf(stderr, "jabs_trace: cannot write %s\n", options);
        return JNI_ERR;
    }
    jvmtiEventCallbacks callbacks;
    memset(&callbacks, 0, sizeof callbacks);
    callbacks.MethodEntry = on_method_entry;
    callbacks.VMDeath = on_vm_death;
    if ((*jvmti)->SetEventCallbacks(jvmti, &callbacks, (jint)sizeof callbacks) != JVMTI_ERROR_NONE
        || (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_METHOD_ENTRY, NULL) != JVMTI_ERROR_NONE
        || (*jvmti)->SetEventNotificationMode(jvmti, JVMTI_ENABLE, JVMTI_EVENT_VM_DEATH, NULL) != JVMTI_ERROR_NONE) {
        fprintf(stderr, "jabs_trace: cannot enable events\n");
        return JNI_ERR;
    }
    return JNI_OK;
}
