/* Host facts the JVM reads from the operating system at startup, and raw standard-stream I/O.
 * Kept apart from generated code so OS headers (windows.h) never meet it. Strings are UTF-8. */
#include "jabs_unit.h"

#ifdef _WIN32
#include <windows.h>
#include <io.h>

/* A malloc'd UTF-8 copy of a wide string. */
static char *utf8_from_wide(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    char *s = n > 0 ? (char *)malloc((size_t)n) : NULL;
    if (s) WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL);
    return s;
}

/* An environment variable as UTF-8, or NULL. */
static char *env_utf8(const wchar_t *name) {
    static wchar_t buffer[32768];
    DWORD n = GetEnvironmentVariableW(name, buffer, 32768);
    return n == 0 || n >= 32768 ? NULL : utf8_from_wide(buffer);
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
        static wchar_t path[32768];
        DWORD n = GetModuleFileNameW(NULL, path, 32768);
        if (n == 0 || n >= 32768) return NULL;
        wchar_t *slash = wcsrchr(path, L'\\');
        if (slash) *slash = 0;
        return utf8_from_wide(path);
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

int jabs_platform_stack_functions(const void **starts, int max) {
    void *pcs[1024];
    int n = (int)RtlCaptureStackBackTrace(0, (DWORD)(max < 1024 ? max : 1024), pcs, NULL);
    for (int i = 0; i < n; i++) {
        DWORD64 image = 0;
        /* The unwind tables give the start of the function containing each return address. */
        PRUNTIME_FUNCTION f = RtlLookupFunctionEntry((DWORD64)(uintptr_t)pcs[i] - 1, &image, NULL);
        starts[i] = f ? (const void *)(uintptr_t)(image + f->BeginAddress) : NULL;
    }
    return n;
}

void jabs_sleep_nanos(int64_t nanos) {
    if (nanos > 0) Sleep((DWORD)((nanos + 999999) / 1000000 > 0x7fffffff ? 0x7fffffff : (nanos + 999999) / 1000000));
}

int64_t jabs_platform_write(int32_t fd, const void *data, size_t length) {
    size_t done = 0;
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

const char *jabs_platform_value(int which) {
    static char cwd[4096];
    static struct utsname host;
    switch (which) {
    case JABS_PLATFORM_FILE_SEPARATOR: return "/";
    case JABS_PLATFORM_LINE_SEPARATOR: return "\n";
    case JABS_PLATFORM_PATH_SEPARATOR: return ":";
    case JABS_PLATFORM_TMPDIR: return "/tmp";
    case JABS_PLATFORM_OS_NAME: return uname(&host) == 0 ? host.sysname : NULL;
    case JABS_PLATFORM_OS_VERSION: return uname(&host) == 0 ? host.release : NULL;
    case JABS_PLATFORM_NATIVE_ENCODING:
    case JABS_PLATFORM_JNU_ENCODING: return "UTF-8";
    case JABS_PLATFORM_USER_DIR: return getcwd(cwd, sizeof cwd);
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
    jabs_unwind_state s = { starts, 0, max };
    _Unwind_Backtrace(jabs_unwind_frame, &s);
    return s.count;
}

void jabs_sleep_nanos(int64_t nanos) {
    struct timespec t = { (time_t)(nanos / 1000000000), (long)(nanos % 1000000000) };
    if (nanos > 0) nanosleep(&t, NULL);
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

int64_t jabs_nano_time_adjustment(int64_t offset_seconds) {
    struct timespec now;
    if (timespec_get(&now, TIME_UTC) != TIME_UTC) return -1;
    int64_t seconds = (int64_t)now.tv_sec - offset_seconds;
    if (seconds > 0xFFFFFFFFLL || seconds < -0xFFFFFFFFLL) return -1;
    return seconds * 1000000000LL + now.tv_nsec;
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
