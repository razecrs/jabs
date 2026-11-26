/* java.nio.file natives (the bodies are in src/cgen/net.rs): sun.nio.fs.WindowsNativeDispatcher
 * and sun.nio.ch.FileDispatcherImpl on Windows, sun.nio.fs.UnixNativeDispatcher and
 * sun.nio.ch.UnixFileDispatcherImpl on Linux. They are thin over the OS, as the JDK's are: Java
 * passes native buffers (UTF-16 or UTF-8 paths, the OS's own structs) by address and reads the
 * results itself, so each native here makes the same OS call on the same memory and fails with
 * the same exception (WindowsException or UnixException carrying the OS error code). Java
 * objects are written through generated glue (jabs_nio_set_first_file, ...). */
#ifndef _WIN32
/* statx (birth times, which the JDK reports on Linux) and makedev are GNU extensions. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#endif
#ifdef _WIN32
#include <windows.h>
#include <winioctl.h>
#include <io.h>
#else
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <dirent.h>
#include <unistd.h>
#endif
#include "jabs_unit.h"

/* IOStatus values the dispatchers return. */
#define JABS_NIO_EOF (-1)
#define JABS_NIO_UNAVAILABLE (-2)
#define JABS_NIO_INTERRUPTED (-3)
#define JABS_NIO_THROWN (-5)

#ifdef _WIN32
/* ---- WindowsNativeDispatcher ---- */

/* The JDK's natives take and give HANDLEs as longs. */
static HANDLE as_handle(int64_t h) { return (HANDLE)(intptr_t)h; }
static int64_t handle_value(HANDLE h) { return (int64_t)(intptr_t)h; }

/* CreateFile0: the new HANDLE, or 0 with WindowsException(GetLastError()) pending. */
int64_t jabs_nio_create_file(int64_t path, int32_t access, int32_t share, int64_t descriptor, int32_t disposition, int32_t flags) {
    SECURITY_ATTRIBUTES attributes;
    LPSECURITY_ATTRIBUTES pointer = NULL;
    if (descriptor != 0) {
        attributes.nLength = sizeof attributes;
        attributes.lpSecurityDescriptor = (LPVOID)(intptr_t)descriptor;
        attributes.bInheritHandle = FALSE;
        pointer = &attributes;
    }
    HANDLE h = CreateFileW((LPCWSTR)(intptr_t)path, (DWORD)access, (DWORD)share, pointer, (DWORD)disposition, (DWORD)flags, NULL);
    if (h == INVALID_HANDLE_VALUE) { jabs_nio_windows_exception((int32_t)GetLastError()); return 0; }
    return handle_value(h);
}

/* CloseHandle: the JDK ignores its result. */
void jabs_nio_close_handle(int64_t handle) { CloseHandle(as_handle(handle)); }

void jabs_nio_delete_file(int64_t path) {
    if (!DeleteFileW((LPCWSTR)(intptr_t)path)) jabs_nio_windows_exception((int32_t)GetLastError());
}

/* FindFirstFile0: the search handle, the first entry's name and attributes into FirstFile. */
void jabs_nio_find_first_file(int64_t path, jabs_object *first) {
    if (!first) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    WIN32_FIND_DATAW data;
    HANDLE h = FindFirstFileW((LPCWSTR)(intptr_t)path, &data);
    if (h == INVALID_HANDLE_VALUE) { jabs_nio_windows_exception((int32_t)GetLastError()); return; }
    data.cFileName[MAX_PATH - 1] = 0;
    jabs_object *name = jabs_string_from_utf16((const uint16_t *)data.cFileName, wcslen(data.cFileName));
    if (!name) { FindClose(h); return; }
    jabs_nio_set_first_file(first, handle_value(h), name, (int32_t)data.dwFileAttributes);
}

/* FindNextFile0: the next entry into the WIN32_FIND_DATAW at data, its name, or null at the
 * end (ERROR_NO_MORE_FILES). */
jabs_object *jabs_nio_find_next_file(int64_t handle, int64_t data) {
    WIN32_FIND_DATAW *found = (WIN32_FIND_DATAW *)(intptr_t)data;
    if (!found) jabs_not_compiled("FindNextFile0 without a data buffer (jabs_nio_find_next_file, jabs_nio_fs.c)");
    if (FindNextFileW(as_handle(handle), found)) {
        found->cFileName[MAX_PATH - 1] = 0;
        return jabs_string_from_utf16((const uint16_t *)found->cFileName, wcslen(found->cFileName));
    }
    DWORD error = GetLastError();
    if (error != ERROR_NO_MORE_FILES) jabs_nio_windows_exception((int32_t)error);
    return NULL;
}

void jabs_nio_find_close(int64_t handle) {
    if (!FindClose(as_handle(handle))) jabs_nio_windows_exception((int32_t)GetLastError());
}

void jabs_nio_get_file_attributes_ex(int64_t path, int64_t data) {
    if (!GetFileAttributesExW((LPCWSTR)(intptr_t)path, GetFileExInfoStandard, (LPVOID)(intptr_t)data)) jabs_nio_windows_exception((int32_t)GetLastError());
}

void jabs_nio_get_file_information(int64_t handle, int64_t data) {
    if (!GetFileInformationByHandle(as_handle(handle), (LPBY_HANDLE_FILE_INFORMATION)(intptr_t)data)) jabs_nio_windows_exception((int32_t)GetLastError());
}

/* GetFullPathName0: tried with MAX_PATH first, then with the length it asks for. */
jabs_object *jabs_nio_get_full_path_name(int64_t path) {
    LPCWSTR name = (LPCWSTR)(intptr_t)path;
    WCHAR buffer[MAX_PATH];
    DWORD n = GetFullPathNameW(name, MAX_PATH, buffer, NULL);
    if (n == 0) { jabs_nio_windows_exception((int32_t)GetLastError()); return NULL; }
    if (n < MAX_PATH) return jabs_string_from_utf16((const uint16_t *)buffer, n);
    if (n > (DWORD)INT32_MAX - 1) jabs_not_compiled("a full path name longer than an int (jabs_nio_get_full_path_name, jabs_nio_fs.c)");
    DWORD size = n + 1;
    WCHAR *big = (WCHAR *)malloc((size_t)size * sizeof(WCHAR));
    if (!big) { jabs_raise_message(JABS_VM_OUT_OF_MEMORY, "native memory allocation failure"); return NULL; }
    n = GetFullPathNameW(name, size, big, NULL);
    /* The path changed length between the two calls (the current directory moved): the JDK's
     * answer is InternalError("GetFullPathNameW failed"), which JABS does not raise. */
    if (n == 0 || n >= size) { free(big); jabs_not_compiled("GetFullPathNameW failed on its second call (jabs_nio_get_full_path_name, jabs_nio_fs.c)"); }
    jabs_object *s = jabs_string_from_utf16((const uint16_t *)big, n);
    free(big);
    return s;
}

void jabs_nio_get_reparse_point(int64_t handle, int64_t buffer, int32_t size) {
    DWORD returned = 0;
    if (size < 0) jabs_not_compiled("DeviceIoControlGetReparsePoint with a negative size (jabs_nio_fs.c)");
    if (!DeviceIoControl(as_handle(handle), FSCTL_GET_REPARSE_POINT, NULL, 0, (LPVOID)(intptr_t)buffer, (DWORD)size, &returned, NULL)) jabs_nio_windows_exception((int32_t)GetLastError());
}

/* ---- FileDispatcherImpl (Windows) ---- */

/* The HANDLE behind a FileDescriptor: its handle field (CreateFile0's), or the C runtime's
 * handle of its fd (JABS's java.io streams keep the descriptor instead; see jabs_io.c). */
static HANDLE descriptor_handle(jabs_object *fdo) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return INVALID_HANDLE_VALUE; }
    int64_t h = jabs_descriptor_handle(fdo);
    if (h != -1) return as_handle(h);
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    if (fd < 0) return INVALID_HANDLE_VALUE;
    return (HANDLE)_get_osfhandle(fd);
}

/* JNU_ThrowIOExceptionWithLastError: the system's text for the error, else the default. */
static void raise_last_error(DWORD code, const char *fallback) {
    wchar_t text[512];
    DWORD n = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, NULL, code, 0, text, 512, NULL);
    if (n == 0 || n >= 512) { jabs_raise_message(JABS_VM_IO, fallback); return; }
    if (n > 3) {
        if (text[n - 1] == L'\n') n--;
        if (text[n - 1] == L'\r') n--;
        if (text[n - 1] == L'.') n--;
    }
    text[n] = 0;
    char utf8[1536];
    if (WideCharToMultiByte(CP_UTF8, 0, text, -1, utf8, (int)sizeof utf8, NULL, NULL) <= 0) { jabs_raise_message(JABS_VM_IO, fallback); return; }
    jabs_raise_message(JABS_VM_IO, utf8);
}

/* FileDispatcherImpl.read0: ReadFile; end of a pipe is IOS_EOF, an empty non-blocking pipe
 * IOS_UNAVAILABLE, zero bytes the end of the file. */
int32_t jabs_nio_file_read(jabs_object *fdo, int64_t address, int32_t length) {
    HANDLE h = descriptor_handle(fdo);
    if (jabs_pending) return JABS_NIO_THROWN;
    /* Java never reads zero bytes; the JDK would report a stale error for it. */
    if (length <= 0) jabs_not_compiled("FileDispatcherImpl.read0 of no bytes (jabs_nio_file_read, jabs_nio_fs.c)");
    DWORD got = 0;
    if (!ReadFile(h, (LPVOID)(intptr_t)address, (DWORD)length, &got, NULL)) {
        DWORD error = GetLastError();
        if (error == ERROR_BROKEN_PIPE) return JABS_NIO_EOF;
        if (error == ERROR_NO_DATA) return JABS_NIO_UNAVAILABLE;
        raise_last_error(error, "Read failed");
        return JABS_NIO_THROWN;
    }
    if (got > (DWORD)length) jabs_not_compiled("ReadFile read more than asked (jabs_nio_file_read, jabs_nio_fs.c)");
    return got > 0 ? (int32_t)got : JABS_NIO_EOF;
}

/* FileDispatcherImpl.allocationGranularity0: the granularity file mappings are aligned to. */
int64_t jabs_nio_allocation_granularity(void) {
    SYSTEM_INFO info;
    GetSystemInfo(&info);
    return info.dwAllocationGranularity;
}

/* FileDispatcherImpl.maxDirectTransferSize0: the JDK's Windows limit for one TransmitFile
 * (INT_MAX - 1, as the reference JVM reports). */
int32_t jabs_nio_max_direct_transfer_size(void) { return INT32_MAX - 1; }

/* Linux natives: never reached on Windows. */
int32_t jabs_nio_unix_init(void) { jabs_not_compiled("UnixNativeDispatcher.init on Windows (jabs_nio_fs.c)"); }
jabs_object *jabs_nio_getcwd(void) { jabs_not_compiled("UnixNativeDispatcher.getcwd on Windows (jabs_nio_fs.c)"); }
int32_t jabs_nio_open(int64_t path, int32_t flags, int32_t mode) { (void)path; (void)flags; (void)mode; jabs_not_compiled("UnixNativeDispatcher.open0 on Windows (jabs_nio_fs.c)"); }
int32_t jabs_nio_stat(int64_t path, jabs_object *attributes, int32_t follow) { (void)path; (void)attributes; (void)follow; jabs_not_compiled("UnixNativeDispatcher.stat0 on Windows (jabs_nio_fs.c)"); }
void jabs_nio_unlink(int64_t path) { (void)path; jabs_not_compiled("UnixNativeDispatcher.unlink0 on Windows (jabs_nio_fs.c)"); }
void jabs_nio_close_int_fd(int32_t fd) { (void)fd; jabs_not_compiled("UnixFileDispatcherImpl.closeIntFD on Windows (jabs_nio_fs.c)"); }
int32_t jabs_nio_unix_read(jabs_object *fdo, int64_t address, int32_t length) { (void)fdo; (void)address; (void)length; jabs_not_compiled("UnixFileDispatcherImpl.read0 on Windows (jabs_nio_fs.c)"); }
int32_t jabs_nio_access(int64_t path, int32_t mode) { (void)path; (void)mode; jabs_not_compiled("UnixNativeDispatcher.access0 on Windows (jabs_nio_fs.c)"); }
void jabs_nio_close(int32_t fd) { (void)fd; jabs_not_compiled("UnixNativeDispatcher.close0 on Windows (jabs_nio_fs.c)"); }
int32_t jabs_nio_dup(int32_t fd) { (void)fd; jabs_not_compiled("UnixNativeDispatcher.dup on Windows (jabs_nio_fs.c)"); }
int64_t jabs_nio_fdopendir(int32_t fd) { (void)fd; jabs_not_compiled("UnixNativeDispatcher.fdopendir on Windows (jabs_nio_fs.c)"); }
void jabs_nio_closedir(int64_t dir) { (void)dir; jabs_not_compiled("UnixNativeDispatcher.closedir on Windows (jabs_nio_fs.c)"); }
jabs_object *jabs_nio_readdir(int64_t dir) { (void)dir; jabs_not_compiled("UnixNativeDispatcher.readdir0 on Windows (jabs_nio_fs.c)"); }
jabs_object *jabs_nio_realpath(int64_t path) { (void)path; jabs_not_compiled("UnixNativeDispatcher.realpath0 on Windows (jabs_nio_fs.c)"); }
#else
/* ---- UnixNativeDispatcher (Linux) ---- */

/* init: the capabilities the JDK reports on Linux with statx (65546 on the reference JVM):
 * openat and friends, extended attributes, birth times. */
#define JABS_UNIX_SUPPORTS_OPENAT (1 << 1)
#define JABS_UNIX_SUPPORTS_XATTR (1 << 3)
#define JABS_UNIX_SUPPORTS_BIRTHTIME (1 << 16)

int32_t jabs_nio_unix_init(void) {
    return JABS_UNIX_SUPPORTS_OPENAT | JABS_UNIX_SUPPORTS_XATTR | JABS_UNIX_SUPPORTS_BIRTHTIME;
}

jabs_object *jabs_nio_getcwd(void) {
    char buffer[PATH_MAX + 1];
    if (!getcwd(buffer, sizeof buffer)) { jabs_nio_unix_exception(errno); return NULL; }
    size_t n = strlen(buffer);
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!bytes) return NULL;
    memcpy(jabs_array_data((jabs_array *)bytes), buffer, n);
    return bytes;
}

/* open0: open, retried on EINTR (RESTARTABLE); failure is UnixException(errno). */
int32_t jabs_nio_open(int64_t path, int32_t flags, int32_t mode) {
    int fd;
    do fd = open((const char *)(intptr_t)path, flags, (mode_t)mode);
    while (fd == -1 && errno == EINTR);
    if (fd == -1) { jabs_nio_unix_exception(errno); return -1; }
    return fd;
}

/* stat0 (follow) and lstat0: statx, as the JDK prefers on Linux, into UnixFileAttributes.
 * stat0 returns the errno (0 on success); lstat0 raises UnixException. */
int32_t jabs_nio_stat(int64_t path, jabs_object *attributes, int32_t follow) {
    if (!attributes) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }
    struct statx buffer;
    int flags = AT_STATX_SYNC_AS_STAT | (follow ? 0 : AT_SYMLINK_NOFOLLOW);
    int r;
    do r = statx(AT_FDCWD, (const char *)(intptr_t)path, flags, STATX_ALL, &buffer);
    while (r == -1 && errno == EINTR);
    if (r != 0) {
        if (follow) return errno;
        jabs_nio_unix_exception(errno);
        return 0;
    }
    jabs_nio_stat_result s;
    s.mode = (int32_t)buffer.stx_mode;
    s.ino = (int64_t)buffer.stx_ino;
    s.dev = (int64_t)makedev(buffer.stx_dev_major, buffer.stx_dev_minor);
    s.rdev = (int64_t)makedev(buffer.stx_rdev_major, buffer.stx_rdev_minor);
    s.nlink = (int32_t)buffer.stx_nlink;
    s.uid = (int32_t)buffer.stx_uid;
    s.gid = (int32_t)buffer.stx_gid;
    s.size = (int64_t)buffer.stx_size;
    s.atime_sec = buffer.stx_atime.tv_sec;
    s.atime_nsec = buffer.stx_atime.tv_nsec;
    s.mtime_sec = buffer.stx_mtime.tv_sec;
    s.mtime_nsec = buffer.stx_mtime.tv_nsec;
    s.ctime_sec = buffer.stx_ctime.tv_sec;
    s.ctime_nsec = buffer.stx_ctime.tv_nsec;
    s.birthtime_available = (buffer.stx_mask & STATX_BTIME) != 0;
    s.birthtime_sec = s.birthtime_available ? buffer.stx_btime.tv_sec : 0;
    s.birthtime_nsec = s.birthtime_available ? buffer.stx_btime.tv_nsec : 0;
    jabs_nio_set_unix_attributes(attributes, &s);
    return 0;
}

void jabs_nio_unlink(int64_t path) {
    if (unlink((const char *)(intptr_t)path) == -1) jabs_nio_unix_exception(errno);
}

/* access0: 0, or the errno (no exception), retried on EINTR. */
int32_t jabs_nio_access(int64_t path, int32_t mode) {
    int r;
    do r = access((const char *)(intptr_t)path, mode);
    while (r == -1 && errno == EINTR);
    return r == -1 ? errno : 0;
}

/* close0: an interrupted close is not an error (the descriptor is gone either way). */
void jabs_nio_close(int32_t fd) {
    if (close(fd) == -1 && errno != EINTR) jabs_nio_unix_exception(errno);
}

int32_t jabs_nio_dup(int32_t fd) {
    int r;
    do r = dup(fd);
    while (r == -1 && errno == EINTR);
    if (r == -1) jabs_nio_unix_exception(errno);
    return r;
}

/* fdopendir / closedir / readdir0: a DIR* travels to Java as a long. */
int64_t jabs_nio_fdopendir(int32_t fd) {
    DIR *d = fdopendir(fd);
    if (!d) { jabs_nio_unix_exception(errno); return 0; }
    return (int64_t)(intptr_t)d;
}

void jabs_nio_closedir(int64_t dir) {
    if (closedir((DIR *)(intptr_t)dir) == -1 && errno != EINTR) jabs_nio_unix_exception(errno);
}

/* The next entry's name, or null at the end (errno still 0). */
jabs_object *jabs_nio_readdir(int64_t dir) {
    errno = 0;
    struct dirent *entry = readdir((DIR *)(intptr_t)dir);
    if (!entry) {
        if (errno != 0) jabs_nio_unix_exception(errno);
        return NULL;
    }
    size_t n = strlen(entry->d_name);
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), entry->d_name, n);
    return bytes;
}

jabs_object *jabs_nio_realpath(int64_t path) {
    char resolved[PATH_MAX + 1];
    if (!realpath((const char *)(intptr_t)path, resolved)) { jabs_nio_unix_exception(errno); return NULL; }
    size_t n = strlen(resolved);
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), resolved, n);
    return bytes;
}

/* ---- UnixFileDispatcherImpl ---- */

static void raise_errno(int code, const char *fallback) {
    /* strerror is thread-safe in glibc 2.32 and later; _GNU_SOURCE hides the XSI strerror_r. */
    jabs_raise_message(JABS_VM_IO, code != 0 ? strerror(code) : fallback);
}

void jabs_nio_close_int_fd(int32_t fd) {
    if (fd != -1 && close(fd) < 0) raise_errno(errno, "Close failed");
}

/* read0: convertReturnVal on a read. */
int32_t jabs_nio_unix_read(jabs_object *fdo, int64_t address, int32_t length) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return JABS_NIO_THROWN; }
    if (length < 0) jabs_not_compiled("UnixFileDispatcherImpl.read0 with a negative length (jabs_nio_fs.c)");
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    ssize_t n = read(fd, (void *)(intptr_t)address, (size_t)length);
    if (n > 0) return (int32_t)n;
    if (n == 0) return JABS_NIO_EOF;
    if (errno == EAGAIN || errno == EWOULDBLOCK) return JABS_NIO_UNAVAILABLE;
    if (errno == EINTR) return JABS_NIO_INTERRUPTED;
    raise_errno(errno, "Read failed");
    return JABS_NIO_THROWN;
}

/* Windows natives: never reached on Linux. */
int64_t jabs_nio_create_file(int64_t path, int32_t access, int32_t share, int64_t descriptor, int32_t disposition, int32_t flags) { (void)path; (void)access; (void)share; (void)descriptor; (void)disposition; (void)flags; jabs_not_compiled("WindowsNativeDispatcher.CreateFile0 on Linux (jabs_nio_fs.c)"); }
void jabs_nio_close_handle(int64_t handle) { (void)handle; jabs_not_compiled("WindowsNativeDispatcher.CloseHandle on Linux (jabs_nio_fs.c)"); }
void jabs_nio_delete_file(int64_t path) { (void)path; jabs_not_compiled("WindowsNativeDispatcher.DeleteFile0 on Linux (jabs_nio_fs.c)"); }
void jabs_nio_find_first_file(int64_t path, jabs_object *first) { (void)path; (void)first; jabs_not_compiled("WindowsNativeDispatcher.FindFirstFile0 on Linux (jabs_nio_fs.c)"); }
jabs_object *jabs_nio_find_next_file(int64_t handle, int64_t data) { (void)handle; (void)data; jabs_not_compiled("WindowsNativeDispatcher.FindNextFile0 on Linux (jabs_nio_fs.c)"); }
void jabs_nio_find_close(int64_t handle) { (void)handle; jabs_not_compiled("WindowsNativeDispatcher.FindClose on Linux (jabs_nio_fs.c)"); }
void jabs_nio_get_file_attributes_ex(int64_t path, int64_t data) { (void)path; (void)data; jabs_not_compiled("WindowsNativeDispatcher.GetFileAttributesEx0 on Linux (jabs_nio_fs.c)"); }
void jabs_nio_get_file_information(int64_t handle, int64_t data) { (void)handle; (void)data; jabs_not_compiled("WindowsNativeDispatcher.GetFileInformationByHandle0 on Linux (jabs_nio_fs.c)"); }
jabs_object *jabs_nio_get_full_path_name(int64_t path) { (void)path; jabs_not_compiled("WindowsNativeDispatcher.GetFullPathName0 on Linux (jabs_nio_fs.c)"); }
void jabs_nio_get_reparse_point(int64_t handle, int64_t buffer, int32_t size) { (void)handle; (void)buffer; (void)size; jabs_not_compiled("WindowsNativeDispatcher.DeviceIoControlGetReparsePoint on Linux (jabs_nio_fs.c)"); }
int32_t jabs_nio_file_read(jabs_object *fdo, int64_t address, int32_t length) { (void)fdo; (void)address; (void)length; jabs_not_compiled("FileDispatcherImpl.read0 (Windows) on Linux (jabs_nio_fs.c)"); }
int64_t jabs_nio_allocation_granularity(void) { return sysconf(_SC_PAGESIZE); }
int32_t jabs_nio_max_direct_transfer_size(void) { jabs_not_compiled("FileDispatcherImpl.maxDirectTransferSize0 on Linux (jabs_nio_fs.c)"); }
#endif

#ifdef _WIN32
int32_t jabs_nio_close_descriptor_handle(int64_t handle) { return CloseHandle(as_handle(handle)) != 0; }
#else
int32_t jabs_nio_close_descriptor_handle(int64_t handle) { (void)handle; jabs_not_compiled("a FileDescriptor with a handle on Linux (jabs_nio_close_descriptor_handle, jabs_nio_fs.c)"); }
#endif

/* ---- java.lang.ProcessEnvironment: the environment System.getenv reads ---- */

#ifdef _WIN32
/* environmentBlock: GetEnvironmentStringsW up to (not including) the final terminator, as one
 * String of NUL-separated "name=value" entries. */
jabs_object *jabs_process_environment_block(void) {
    WCHAR *block = GetEnvironmentStringsW();
    if (!block) jabs_not_compiled("GetEnvironmentStringsW failed (jabs_process_environment_block, jabs_nio_fs.c)");
    size_t i = 0;
    /* Each entry ends in a NUL; the block ends at an empty entry. */
    while (block[i]) {
        while (block[i]) i++;
        i++;
    }
    jabs_object *s = jabs_string_from_utf16((const uint16_t *)block, i);
    FreeEnvironmentStringsW(block);
    return s;
}

jabs_object *jabs_process_environ(void) { jabs_not_compiled("ProcessEnvironment.environ on Windows (jabs_nio_fs.c)"); }
#else
extern char **environ;

/* environ: byte[] name, byte[] value, ... for each entry with an '='. */
jabs_object *jabs_process_environ(void) {
    int32_t count = 0;
    for (char **e = environ; e && *e; e++) {
        if (strchr(*e, '=')) {
            if (count > INT32_MAX / 2 - 1) jabs_not_compiled("more environment entries than an array holds (jabs_nio_fs.c)");
            count++;
        }
    }
    jabs_object *result = jabs_new_array(2 * count, sizeof(jabs_object *), JABS_COMPONENT_ARRAY, NULL, "[[B", "[B");
    if (!result) return NULL;
    jabs_object **slots = (jabs_object **)jabs_array_data((jabs_array *)result);
    int32_t j = 0;
    for (char **e = environ; e && *e && j < count; e++) {
        const char *end = strchr(*e, '=');
        if (!end) continue;
        size_t name_length = (size_t)(end - *e), value_length = strlen(end + 1);
        if (name_length > INT32_MAX || value_length > INT32_MAX) jabs_not_compiled("an environment entry longer than an array holds (jabs_nio_fs.c)");
        jabs_object *name = jabs_new_array((int32_t)name_length, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
        if (!name) return NULL;
        memcpy(jabs_array_data((jabs_array *)name), *e, name_length);
        jabs_object *value = jabs_new_array((int32_t)value_length, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
        if (!value) return NULL;
        memcpy(jabs_array_data((jabs_array *)value), end + 1, value_length);
        slots[2 * j] = name;
        slots[2 * j + 1] = value;
        j++;
    }
    return result;
}

jabs_object *jabs_process_environment_block(void) { jabs_not_compiled("ProcessEnvironment.environmentBlock on Linux (jabs_nio_fs.c)"); }
#endif
