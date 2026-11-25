/* Raw OS threads for the monitor tests (tests/sync.rs). Kept apart from harness.c so that
 * windows.h never meets jabs_unit.h, as in the runtime itself. Every failure aborts loudly. */
#ifndef OS_THREADS_H
#define OS_THREADS_H
#include <stdint.h>

typedef struct harness_thread harness_thread;
/* Start fn(arg) on a new OS thread. */
harness_thread *harness_spawn(void (*fn)(void *), void *arg);
/* Wait for the thread to end and free it. */
void harness_join(harness_thread *t);
void harness_sleep_ms(int ms);
/* Milliseconds on a monotonic clock. */
int64_t harness_now_ms(void);
/* End the process with status 3 and a message naming `test` if it still runs after `seconds`:
 * a deadlock or a lost wakeup must fail, never hang. */
void harness_start_watchdog(const char *test, int seconds);
#endif
