/* Host facts the JVM reads from the operating system at startup, and raw standard-stream I/O.
 * Kept apart from generated code so OS headers (windows.h) never meet it. Strings are UTF-8. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#include "jabs_runtime.h"

#ifdef _WIN32
#include <windows.h>
/* GetSystemMetrics (hasDisplays0): MinGW links user32 by default, the MSVC target does not. */
#ifdef _MSC_VER
#pragma comment(lib, "user32.lib")
#endif
#include <io.h>
#include <fcntl.h>

/* A malloc'd UTF-8 copy of a wide string. */
static char *utf8_from_wide(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    char *s = n > 0 ? (char *)malloc((size_t)n) : NULL;
    if (s) WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL);
    return s;
}

/* An environment variable as UTF-8, or NULL. The buffer is per call: threads may ask at once. */
static char *env_utf8(const wchar_t *name) {
    wchar_t *buffer = (wchar_t *)malloc(32768 * sizeof(wchar_t));
    if (!buffer) return NULL;
    DWORD n = GetEnvironmentVariableW(name, buffer, 32768);
    char *value = n == 0 || n >= 32768 ? NULL : utf8_from_wide(buffer);
    free(buffer);
    return value;
}

/* Windows version numbers without the compatibility shims GetVersionEx applies. */
static void windows_version(DWORD *major, DWORD *minor, DWORD *build) {
    typedef LONG(WINAPI * get_version)(OSVERSIONINFOW *);
    OSVERSIONINFOW info;
    memset(&info, 0, sizeof info);
    info.dwOSVersionInfoSize = sizeof info;
    get_version f = (get_version)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "RtlGetVersion");
    *major = *minor = *build = 0;
    if (f && f(&info) == 0) { *major = info.dwMajorVersion; *minor = info.dwMinorVersion; *build = info.dwBuildNumber; }
}

/* "en" or "US" from the user's default locale name ("en-US"). */
static char *locale_part(int country) {
    wchar_t name[LOCALE_NAME_MAX_LENGTH];
    if (!GetUserDefaultLocaleName(name, LOCALE_NAME_MAX_LENGTH)) return NULL;
    wchar_t *dash = wcsrchr(name, L'-');
    if (country) return dash && wcslen(dash + 1) == 2 ? utf8_from_wide(dash + 1) : NULL;
    if (dash) *wcschr(name, L'-') = 0;
    return utf8_from_wide(name);
}

/* The JDK's name for an ANSI code page: "UTF-8" for 65001, else "Cp<n>". */
static char *code_page_name(UINT cp) {
    char *s = (char *)malloc(16);
    if (s) { if (cp == 65001) strcpy(s, "UTF-8"); else snprintf(s, 16, "Cp%u", cp); }
    return s;
}

const char *jabs_platform_value(int which) {
    wchar_t buffer[MAX_PATH + 1];
    DWORD major, minor, build;
    switch (which) {
    case JABS_PLATFORM_FILE_SEPARATOR: return "\\";
    case JABS_PLATFORM_LINE_SEPARATOR: return "\r\n";
    case JABS_PLATFORM_PATH_SEPARATOR: return ";";
    case JABS_PLATFORM_TMPDIR: return GetTempPathW(MAX_PATH + 1, buffer) ? utf8_from_wide(buffer) : NULL;
    case JABS_PLATFORM_OS_NAME:
        windows_version(&major, &minor, &build);
        if (major == 10 && build >= 22000) return "Windows 11";
        if (major == 10) return "Windows 10";
        return "Windows";
    case JABS_PLATFORM_OS_VERSION: {
        char *s = (char *)malloc(24);
        windows_version(&major, &minor, &build);
        if (s) snprintf(s, 24, "%lu.%lu", (unsigned long)major, (unsigned long)minor);
        return s;
    }
    case JABS_PLATFORM_NATIVE_ENCODING:
    case JABS_PLATFORM_JNU_ENCODING: return code_page_name(GetACP());
    case JABS_PLATFORM_USER_DIR: return GetCurrentDirectoryW(MAX_PATH + 1, buffer) ? utf8_from_wide(buffer) : NULL;
    case JABS_PLATFORM_USER_HOME: return env_utf8(L"USERPROFILE");
    case JABS_PLATFORM_USER_NAME: return env_utf8(L"USERNAME");
    case JABS_PLATFORM_LANGUAGE: return locale_part(0);
    case JABS_PLATFORM_COUNTRY: return locale_part(1);
    case JABS_PLATFORM_LIBRARY_PREFIX: return "";
    case JABS_PLATFORM_LIBRARY_SUFFIX: return ".dll";
    case JABS_PLATFORM_EXE_DIR: {
        wchar_t *path = (wchar_t *)malloc(32768 * sizeof(wchar_t));
        if (!path) return NULL;
        DWORD n = GetModuleFileNameW(NULL, path, 32768);
        if (n == 0 || n >= 32768) { free(path); return NULL; }
        wchar_t *slash = wcsrchr(path, L'\\');
        if (slash) *slash = 0;
        char *dir = utf8_from_wide(path);
        free(path);
        return dir;
    }
    default: break;
    }
    return jabs_platform_common(which);
}

int64_t jabs_platform_std_handle(int32_t fd) {
    DWORD which = fd == 0 ? STD_INPUT_HANDLE : fd == 1 ? STD_OUTPUT_HANDLE : fd == 2 ? STD_ERROR_HANDLE : 0;
    if (!which) return -1;
    HANDLE h = GetStdHandle(which);
    return h == INVALID_HANDLE_VALUE ? -1 : (int64_t)(intptr_t)h;
}

int64_t jabs_platform_set_error_mode(int64_t mode) {
    return (int64_t)SetErrorMode((UINT)mode);
}

/* sun.awt.PlatformGraphicsInfo.hasDisplays0 (Windows): whether the desktop has a display, which
 * GraphicsEnvironment.isHeadless reports the opposite of when java.awt.headless is unset. */
int32_t jabs_platform_has_displays(void) {
    return GetSystemMetrics(SM_CMONITORS) > 0;
}

int64_t jabs_platform_process_id(void) {
    return (int64_t)GetCurrentProcessId(); /* cannot fail */
}

/* TimeZone.getSystemTimeZoneID on Windows, as the JDK does it: the registry's zone key name
 * (GetDynamicTimeZoneInformation), mapped to a Java zone ID through the JDK's own
 * <java.home>/lib/tzmappings, whose lines are "WindowsKey:region:JavaID:". An entry for the
 * user's two-letter region wins over the "001" default entry. NULL when there is no name, no
 * mapping file or no matching line; Java then falls back to the GMT offset. */
#define JABS_TZ_LINE_CAP 512
#define JABS_TZ_LINES_CAP 4096
#define JABS_TZ_FILE_CAP (1024 * 1024)
char *jabs_platform_time_zone_id(const char *java_home) {
    if (!java_home) return NULL;
    DYNAMIC_TIME_ZONE_INFORMATION zone;
    if (GetDynamicTimeZoneInformation(&zone) == TIME_ZONE_ID_INVALID) return NULL;
    if (zone.TimeZoneKeyName[0] == 0) return NULL;
    char *key = utf8_from_wide(zone.TimeZoneKeyName);
    if (!key) return NULL;
    char region[8] = "";
    GEOID geo = GetUserGeoID(GEOCLASS_NATION);
    if (geo != GEOID_NOT_AVAILABLE) {
        wchar_t iso[8];
        if (GetGeoInfoW(geo, GEO_ISO2, iso, 8, 0) > 0 && iso[0] && iso[1] && !iso[2]) {
            region[0] = (char)iso[0];
            region[1] = (char)iso[1];
            region[2] = 0;
        }
    }
    size_t home = strlen(java_home);
    char *path = (char *)malloc(home + 32);
    if (!path) { free(key); return NULL; }
    memcpy(path, java_home, home);
    memcpy(path + home, "\\lib\\tzmappings", 16);
    /* Through the file layer, so a .jab's java.home serves it (jabs_files.c, jabs_jab.c). */
    int32_t fd = jabs_fd_open(path, JABS_OPEN_READ);
    free(path);
    if (fd < 0) { free(key); return NULL; }
    char *text = (char *)malloc(JABS_TZ_FILE_CAP + 1);
    size_t used = 0;
    for (int reads = 0; text && used < JABS_TZ_FILE_CAP && reads < JABS_TZ_LINES_CAP; reads++) {
        int64_t n = jabs_fd_read(fd, text + used, JABS_TZ_FILE_CAP - used);
        if (n <= 0) break;
        used += (size_t)n;
    }
    jabs_fd_close(fd);
    if (!text) { free(key); return NULL; }
    text[used] = 0;
    char line[JABS_TZ_LINE_CAP], fallback[JABS_TZ_LINE_CAP] = "", exact[JABS_TZ_LINE_CAP] = "";
    size_t key_length = strlen(key);
    const char *next = text;
    for (int n = 0; n < JABS_TZ_LINES_CAP && *next; n++) {
        size_t length = strcspn(next, "\n");
        size_t kept = length < sizeof line - 1 ? length : sizeof line - 1;
        memcpy(line, next, kept);
        line[kept] = 0;
        next += length + (next[length] == '\n');
        if (line[0] == '#' || strncmp(line, key, key_length) != 0 || line[key_length] != ':') continue;
        char *code = line + key_length + 1;
        char *id = strchr(code, ':');
        if (!id) continue;
        *id++ = 0;
        char *end = strchr(id, ':');
        if (!end || end == id) continue;
        *end = 0;
        if (region[0] && strcmp(code, region) == 0) { strcpy(exact, id); break; }
        if (strcmp(code, "001") == 0 && !fallback[0]) strcpy(fallback, id);
    }
    free(text);
    free(key);
    const char *chosen = exact[0] ? exact : fallback;
    if (!chosen[0]) return NULL;
    size_t length = strlen(chosen);
    char *out = (char *)malloc(length + 1);
    if (out) memcpy(out, chosen, length + 1);
    return out;
}

static bool jabs_utc_and_local(time_t now, struct tm *utc, struct tm *local) {
    return gmtime_s(utc, &now) == 0 && localtime_s(local, &now) == 0;
}

int32_t jabs_platform_load_average(double *out, int32_t count) {
    (void)out;
    (void)count;
    return -1; /* Windows keeps no load average */
}

int jabs_platform_stack_functions(const void **starts, int max) {
    if (!starts || max <= 0) return 0;
    void *pcs[1024];
    int limit = max < 1024 ? max : 1024;
    int n = (int)RtlCaptureStackBackTrace(0, (DWORD)limit, pcs, NULL);
    for (int i = 0; i < n; i++) {
        DWORD64 image = 0;
        /* The unwind tables give the start of the function containing each return address. */
        PRUNTIME_FUNCTION f = pcs[i] ? RtlLookupFunctionEntry((DWORD64)(uintptr_t)pcs[i] - 1, &image, NULL) : NULL;
        starts[i] = f ? (const void *)(uintptr_t)(image + f->BeginAddress) : NULL;
    }
    return n;
}

void jabs_sleep_nanos(int64_t nanos) {
    if (nanos > 0) {
        /* Divide before rounding up so nanos near INT64_MAX cannot overflow signed int64_t. */
        int64_t millis = nanos / 1000000 + (nanos % 1000000 != 0);
        Sleep((DWORD)(millis > 0x7fffffff ? 0x7fffffff : millis));
    }
}

int64_t jabs_nano_time(void) {
    LARGE_INTEGER counter, frequency;
    if (!QueryPerformanceFrequency(&frequency) || frequency.QuadPart <= 0 ||
        !QueryPerformanceCounter(&counter)) abort();
    uint64_t ticks = (uint64_t)counter.QuadPart;
    uint64_t hz = (uint64_t)frequency.QuadPart;
    /* Split before scaling: multiplying the full tick count would overflow after days.
     * The fractional calculation also accommodates unusually high counter frequencies. */
    uint64_t ns = (ticks / hz) * UINT64_C(1000000000)
        + (uint64_t)((long double)(ticks % hz) * 1000000000.0L / (long double)hz);
    return (int64_t)ns;
}

/* The CRT opens the standard streams in text mode, which turns every LF written to them into
   CR LF; Java already writes its own line separator, so the JVM writes them raw. Switch each to
   binary once, flushing C stdio's buffered text first so nothing written before changes.
   Threads may race to switch the same stream; switching it twice is harmless. */
static void jabs_std_binary(int32_t fd) {
    static _Atomic int done[3];
    if (fd < 0 || fd > 2 || atomic_load_explicit(&done[fd], memory_order_acquire)) return;
    if (fd == 1) fflush(stdout);
    if (fd == 2) fflush(stderr);
    if (_setmode(fd, _O_BINARY) != -1) atomic_store_explicit(&done[fd], 1, memory_order_release);
}

int64_t jabs_platform_write(int32_t fd, const void *data, size_t length) {
    size_t done = 0;
    jabs_std_binary(fd);
    while (done < length) {
        unsigned chunk = length - done > 0x40000000u ? 0x40000000u : (unsigned)(length - done);
        int n = _write(fd, (const char *)data + done, chunk);
        if (n < 0) return -1;
        done += (size_t)n;
    }
    return (int64_t)done;
}

#else
#include <unistd.h>
#include <sys/utsname.h>
#include <unwind.h>

/* A malloc'd copy, so no buffer is shared between threads; NULL for NULL or no memory. */
static char *copy_text(const char *text) {
    if (!text) return NULL;
    size_t n = strlen(text) + 1;
    char *s = (char *)malloc(n);
    if (s) memcpy(s, text, n);
    return s;
}

const char *jabs_platform_value(int which) {
    char cwd[4096];
    struct utsname host;
    switch (which) {
    case JABS_PLATFORM_FILE_SEPARATOR: return "/";
    case JABS_PLATFORM_LINE_SEPARATOR: return "\n";
    case JABS_PLATFORM_PATH_SEPARATOR: return ":";
    case JABS_PLATFORM_TMPDIR: return "/tmp";
    case JABS_PLATFORM_OS_NAME: return uname(&host) == 0 ? copy_text(host.sysname) : NULL;
    case JABS_PLATFORM_OS_VERSION: return uname(&host) == 0 ? copy_text(host.release) : NULL;
    case JABS_PLATFORM_NATIVE_ENCODING:
    case JABS_PLATFORM_JNU_ENCODING: return "UTF-8";
    case JABS_PLATFORM_USER_DIR: return getcwd(cwd, sizeof cwd) ? copy_text(cwd) : NULL;
    case JABS_PLATFORM_USER_HOME: return getenv("HOME");
    case JABS_PLATFORM_USER_NAME: return getenv("USER");
    case JABS_PLATFORM_LANGUAGE: return "en";
    case JABS_PLATFORM_COUNTRY: return NULL;
    case JABS_PLATFORM_EXE_DIR: return ".";
    case JABS_PLATFORM_LIBRARY_PREFIX: return "lib";
#ifdef __APPLE__
    case JABS_PLATFORM_LIBRARY_SUFFIX: return ".dylib";
#else
    case JABS_PLATFORM_LIBRARY_SUFFIX: return ".so";
#endif
    default: break;
    }
    return jabs_platform_common(which);
}

int64_t jabs_platform_set_error_mode(int64_t mode) {
    (void)mode;
    return 0;
}

/* Only Windows' PlatformGraphicsInfo declares hasDisplays0; nothing calls this elsewhere. */
int32_t jabs_platform_has_displays(void) {
    return 0;
}

int64_t jabs_platform_process_id(void) {
    return (int64_t)getpid(); /* cannot fail */
}

/* TimeZone.getSystemTimeZoneID on Linux and other POSIX hosts, in the JDK's order: the TZ
 * variable, then /etc/timezone (Debian and Ubuntu), then the zone /etc/localtime links to. The
 * JDK would also compare a copied /etc/localtime with every zoneinfo file; JABS does not, and
 * answers NULL there, so Java falls back to the GMT offset. */
#define JABS_TZ_LINE_CAP 512
static char *jabs_copy_text(const char *text) {
    size_t length = strlen(text);
    char *out = (char *)malloc(length + 1);
    if (out) memcpy(out, text, length + 1);
    return out;
}
char *jabs_platform_time_zone_id(const char *java_home) {
    (void)java_home;
    const char *tz = getenv("TZ");
    if (tz && tz[0]) {
        if (tz[0] == ':') tz++;
        const char *zoneinfo = strstr(tz, "zoneinfo/");
        if (zoneinfo) tz = zoneinfo + 9;
        if (tz[0]) return jabs_copy_text(tz);
    }
    FILE *f = fopen("/etc/timezone", "r");
    if (f) {
        char line[JABS_TZ_LINE_CAP];
        char *got = fgets(line, sizeof line, f);
        fclose(f);
        if (got) {
            line[strcspn(line, " \t\r\n")] = 0;
            if (line[0]) return jabs_copy_text(line);
        }
    }
    char target[JABS_TZ_LINE_CAP];
    ssize_t n = readlink("/etc/localtime", target, sizeof target - 1);
    if (n > 0) {
        target[n] = 0;
        const char *zoneinfo = strstr(target, "zoneinfo/");
        if (zoneinfo && zoneinfo[9]) return jabs_copy_text(zoneinfo + 9);
    }
    return NULL;
}

static bool jabs_utc_and_local(time_t now, struct tm *utc, struct tm *local) {
    return gmtime_r(&now, utc) != NULL && localtime_r(&now, local) != NULL;
}

/* glibc and the BSDs have it in <stdlib.h>, but only outside strict POSIX mode, which this file
 * asks for (_POSIX_C_SOURCE); the signature is the same on every host that has it. */
extern int getloadavg(double loadavg[], int nelem);

int32_t jabs_platform_load_average(double *out, int32_t count) {
    if (!out || count < 1 || count > 3) {
        fprintf(stderr, "jabs: jabs_platform_load_average(%p, %d): bad arguments\n", (void *)out, (int)count);
        abort();
    }
    int got = getloadavg(out, count);
    return got < 1 ? -1 : (int32_t)got;
}

typedef struct { const void **starts; int count, max; } jabs_unwind_state;

/* One frame of _Unwind_Backtrace: record the start of its enclosing function. */
static _Unwind_Reason_Code jabs_unwind_frame(struct _Unwind_Context *context, void *data) {
    jabs_unwind_state *s = (jabs_unwind_state *)data;
    if (s->count >= s->max) return _URC_END_OF_STACK;
    uintptr_t pc = (uintptr_t)_Unwind_GetIP(context);
    s->starts[s->count++] = pc ? _Unwind_FindEnclosingFunction((void *)(pc - 1)) : NULL;
    return _URC_NO_REASON;
}

int jabs_platform_stack_functions(const void **starts, int max) {
    if (!starts || max <= 0) return 0;
    jabs_unwind_state s = { starts, 0, max };
    _Unwind_Backtrace(jabs_unwind_frame, &s);
    return s.count;
}

void jabs_sleep_nanos(int64_t nanos) {
    if (nanos > 0) {
        struct timespec t = { (time_t)(nanos / 1000000000), (long)(nanos % 1000000000) };
        nanosleep(&t, NULL);
    }
}

int64_t jabs_nano_time(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t) != 0) abort();
    return (int64_t)((uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec);
}

int64_t jabs_platform_std_handle(int32_t fd) {
    (void)fd;
    return -1; /* POSIX: FileDescriptor uses the fd itself */
}

int64_t jabs_platform_write(int32_t fd, const void *data, size_t length) {
    size_t done = 0;
    while (done < length) {
        ssize_t n = write(fd, (const char *)data + done, length - done);
        if (n < 0) return -1;
        done += (size_t)n;
    }
    return (int64_t)done;
}
#endif

/* TimeZone.getSystemGMTOffsetID: the host's current offset from UTC as "GMT+hh:mm" (or
 * "GMT-hh:mm"), and "GMT" for no offset. Java uses it when no zone ID could be found. */
char *jabs_platform_gmt_offset_id(void) {
    time_t now = time(NULL);
    struct tm utc, local;
    if (now == (time_t)-1 || !jabs_utc_and_local(now, &utc, &local)) return NULL;
    int days = local.tm_year != utc.tm_year ? (local.tm_year > utc.tm_year ? 1 : -1) : local.tm_yday - utc.tm_yday;
    int minutes = days * 1440 + (local.tm_hour - utc.tm_hour) * 60 + (local.tm_min - utc.tm_min);
    char *out = (char *)malloc(16);
    if (!out) return NULL;
    if (minutes == 0) {
        memcpy(out, "GMT", 4);
        return out;
    }
    char sign = minutes < 0 ? '-' : '+';
    if (minutes < 0) minutes = -minutes;
    snprintf(out, 16, "GMT%c%02d:%02d", sign, minutes / 60, minutes % 60);
    return out;
}

int64_t jabs_nano_time_adjustment(int64_t offset_seconds) {
    struct timespec now;
    if (timespec_get(&now, TIME_UTC) != TIME_UTC) return -1;
    int64_t sec = (int64_t)now.tv_sec;
    /* Guard against signed subtraction overflow before range-checking the delta. */
    if ((offset_seconds < 0 && sec > INT64_MAX + offset_seconds) ||
        (offset_seconds > 0 && sec < INT64_MIN + offset_seconds)) return -1;
    int64_t seconds = sec - offset_seconds;
    if (seconds > 0xFFFFFFFFLL || seconds < -0xFFFFFFFFLL) return -1;
    return seconds * 1000000000LL + now.tv_nsec;
}

/* The VM's start for java.lang.management (RuntimeMXBean start time and uptime). HotSpot takes
 * the start time when VM creation ends and the uptime from its own start, both before main;
 * JABS takes both at once when jabs_vm_startup begins, which is single-threaded. A program
 * without that startup (a bare compiled unit) starts the clock on the first query instead.
 * State: 0 not started, 1 being recorded, 2 recorded (release, after the two values). */
static _Atomic int jabs_start_state;
static int64_t jabs_start_wall_millis, jabs_start_nanos;
/* A thread that finds another recording waits this many yields at most: recording is two clock
 * reads, so running out means the recorder died. */
#define JABS_START_WAIT_LIMIT 100000000

void jabs_platform_record_start(void) {
    int expected = 0;
    if (atomic_compare_exchange_strong_explicit(&jabs_start_state, &expected, 1, memory_order_acq_rel, memory_order_acquire)) {
        jabs_start_wall_millis = jabs_current_millis();
        jabs_start_nanos = jabs_nano_time();
        atomic_store_explicit(&jabs_start_state, 2, memory_order_release);
        return;
    }
    for (long i = 0; i < JABS_START_WAIT_LIMIT; i++) {
        if (atomic_load_explicit(&jabs_start_state, memory_order_acquire) == 2) return;
        jabs_thread_yield();
    }
    fprintf(stderr, "jabs: the VM start time was never recorded (jabs_platform_record_start)\n");
    abort();
}

int64_t jabs_platform_start_millis(void) {
    jabs_platform_record_start();
    return jabs_start_wall_millis;
}

int64_t jabs_platform_uptime_millis(void) {
    jabs_platform_record_start();
    int64_t elapsed = jabs_nano_time() - jabs_start_nanos;
    /* The monotonic clock never goes back; a negative difference is a broken clock. */
    if (elapsed < 0) { fprintf(stderr, "jabs: the monotonic clock went back (jabs_platform_uptime_millis)\n"); abort(); }
    return elapsed / 1000000;
}

/* Values that do not depend on the operating system. */
const char *jabs_platform_common(int which) {
    switch (which) {
    case JABS_PLATFORM_OS_ARCH:
#if defined(__x86_64__) || defined(_M_X64)
        return "amd64";
#elif defined(__aarch64__) || defined(_M_ARM64)
        return "aarch64";
#else
        return NULL;
#endif
    case JABS_PLATFORM_DATA_MODEL: return sizeof(void *) == 8 ? "64" : "32";
    case JABS_PLATFORM_ENDIAN: {
        const uint16_t probe = 1;
        return *(const uint8_t *)&probe ? "little" : "big";
    }
    case JABS_PLATFORM_UNICODE_ENCODING: {
        const uint16_t probe = 1;
        return *(const uint8_t *)&probe ? "UnicodeLittle" : "UnicodeBig";
    }
    default: return NULL;
    }
}
