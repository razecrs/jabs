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
#include "jabs_runtime.h"

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

/* ---- The .jab's virtual tree (jabs_jab.c): read-only, as a write-protected volume ---- */

static FILETIME vfs_filetime(void) {
    int64_t v = jabs_vfs_mtime_ms() * 10000 + 116444736000000000LL;
    FILETIME t = { (DWORD)v, (DWORD)(v >> 32) };
    return t;
}

static DWORD vfs_attributes(int32_t kind) { return kind == JABS_VFS_DIRECTORY ? FILE_ATTRIBUTE_DIRECTORY : FILE_ATTRIBUTE_READONLY; }

static void vfs_missing(const jabs_vfs_node *node) {
    jabs_nio_windows_exception(node->parent_exists ? ERROR_FILE_NOT_FOUND : ERROR_PATH_NOT_FOUND);
}

/* CreateFile0 on a path of the tree: a virtual descriptor for reading, else the error a
 * write-protected volume gives. */
static int64_t vfs_create_file(const jabs_vfs_node *node, DWORD access, DWORD disposition, DWORD flags) {
    const DWORD writing = GENERIC_WRITE | GENERIC_ALL | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES | DELETE | WRITE_DAC | WRITE_OWNER;
    int creates = disposition == CREATE_NEW || disposition == CREATE_ALWAYS || disposition == OPEN_ALWAYS;
    int writes = (access & writing) || disposition == CREATE_NEW || disposition == CREATE_ALWAYS || disposition == TRUNCATE_EXISTING || (flags & FILE_FLAG_DELETE_ON_CLOSE);
    if (node->kind == JABS_VFS_MISSING) {
        if (creates && node->parent_exists) jabs_nio_windows_exception(ERROR_WRITE_PROTECT);
        else vfs_missing(node);
        return 0;
    }
    if (writes) { jabs_nio_windows_exception(ERROR_WRITE_PROTECT); return 0; }
    /* As NTFS: a directory opens only with backup semantics. */
    if (node->kind == JABS_VFS_DIRECTORY && !(flags & FILE_FLAG_BACKUP_SEMANTICS)) { jabs_nio_windows_exception(ERROR_ACCESS_DENIED); return 0; }
    int64_t d = jabs_vfs_open(node, 0);
    if (d < 0) { jabs_nio_windows_exception(ERROR_TOO_MANY_OPEN_FILES); return 0; }
    return d;
}

/* The last component of a UTF-8 path. */
static const char *vfs_last_component(const char *path) {
    const char *name = path;
    for (const char *p = path; *p; p++) if ((*p == '\\' || *p == '/') && p[1]) name = p + 1;
    return name;
}

static jabs_object *vfs_utf16_name(const char *utf8, WIN32_FIND_DATAW *out) {
    wchar_t name[MAX_PATH];
    int n = MultiByteToWideChar(CP_UTF8, 0, utf8, -1, name, MAX_PATH);
    if (n <= 0) jabs_not_compiled("a name in the .jab longer than MAX_PATH (jabs_nio_fs.c)");
    if (out) memcpy(out->cFileName, name, (size_t)n * sizeof(wchar_t));
    return jabs_string_from_utf16((const uint16_t *)name, (size_t)n - 1);
}

/* CreateFile0: the new HANDLE, or 0 with WindowsException(GetLastError()) pending. */
int64_t jabs_nio_create_file(int64_t path, int32_t access, int32_t share, int64_t descriptor, int32_t disposition, int32_t flags) {
    jabs_vfs_node node;
    if (jabs_vfs_find_wide((const uint16_t *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) return vfs_create_file(&node, (DWORD)access, (DWORD)disposition, (DWORD)flags);
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
void jabs_nio_close_handle(int64_t handle) {
    if (jabs_vfs_is_descriptor(handle)) { jabs_vfs_close(handle); return; }
    CloseHandle(as_handle(handle));
}

void jabs_nio_delete_file(int64_t path) {
    jabs_vfs_node node;
    if (jabs_vfs_find_wide((const uint16_t *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        if (node.kind == JABS_VFS_MISSING) vfs_missing(&node);
        else jabs_nio_windows_exception(ERROR_WRITE_PROTECT);
        return;
    }
    if (!DeleteFileW((LPCWSTR)(intptr_t)path)) jabs_nio_windows_exception((int32_t)GetLastError());
}

/* FindFirstFile0 in the tree: 1 when the path was the tree's (answered or raised). The JDK
 * asks for "<directory>\\*" to list a directory, which starts with "." and "..", or for one
 * exact name. */
static int vfs_find_first_file(const wchar_t *path, jabs_object *first) {
    char *utf8 = NULL;
    {
        int n = WideCharToMultiByte(CP_UTF8, 0, path, -1, NULL, 0, NULL, NULL);
        utf8 = n > 0 ? (char *)malloc((size_t)n) : NULL;
        if (!utf8 || WideCharToMultiByte(CP_UTF8, 0, path, -1, utf8, n, NULL, NULL) != n) { free(utf8); return 0; }
    }
    size_t length = strlen(utf8);
    int listing = length >= 2 && utf8[length - 1] == '*' && (utf8[length - 2] == '\\' || utf8[length - 2] == '/');
    if (listing) utf8[length - 2] = 0;
    jabs_vfs_node node;
    if (jabs_vfs_find(utf8, &node) == JABS_VFS_OUTSIDE) { free(utf8); return 0; }
    if (!listing && (strchr(utf8, '*') || strchr(utf8, '?'))) jabs_not_compiled("FindFirstFile with a wildcard pattern in the .jab (vfs_find_first_file, jabs_nio_fs.c)");
    if (node.kind == JABS_VFS_MISSING) { vfs_missing(&node); free(utf8); return 1; }
    if (listing && node.kind != JABS_VFS_DIRECTORY) { jabs_nio_windows_exception(ERROR_DIRECTORY); free(utf8); return 1; }
    int64_t d = jabs_vfs_open(&node, listing ? 2 : 0);
    if (d < 0) { jabs_nio_windows_exception(ERROR_TOO_MANY_OPEN_FILES); free(utf8); return 1; }
    jabs_object *name = vfs_utf16_name(listing ? "." : vfs_last_component(utf8), NULL);
    free(utf8);
    if (!name) { jabs_vfs_close(d); return 1; }
    jabs_nio_set_first_file(first, d, name, (int32_t)vfs_attributes(node.kind));
    return 1;
}

/* FindFirstFile0: the search handle, the first entry's name and attributes into FirstFile. */
void jabs_nio_find_first_file(int64_t path, jabs_object *first) {
    if (!first) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    if (vfs_find_first_file((const wchar_t *)(intptr_t)path, first)) return;
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
    if (jabs_vfs_is_descriptor(handle)) {
        char *name = NULL;
        int32_t is_directory = 0;
        uint64_t size = 0;
        int32_t got = jabs_vfs_next_name(handle, &name, &is_directory, &size);
        if (got < 0) { jabs_nio_windows_exception(ERROR_INVALID_HANDLE); return NULL; }
        if (got == 0) return NULL;
        memset(found, 0, sizeof *found);
        found->dwFileAttributes = vfs_attributes(is_directory ? JABS_VFS_DIRECTORY : JABS_VFS_FILE);
        found->ftCreationTime = found->ftLastAccessTime = found->ftLastWriteTime = vfs_filetime();
        found->nFileSizeHigh = (DWORD)(size >> 32);
        found->nFileSizeLow = (DWORD)size;
        jabs_object *s = vfs_utf16_name(name, found);
        free(name);
        return s;
    }
    if (FindNextFileW(as_handle(handle), found)) {
        found->cFileName[MAX_PATH - 1] = 0;
        return jabs_string_from_utf16((const uint16_t *)found->cFileName, wcslen(found->cFileName));
    }
    DWORD error = GetLastError();
    if (error != ERROR_NO_MORE_FILES) jabs_nio_windows_exception((int32_t)error);
    return NULL;
}

void jabs_nio_find_close(int64_t handle) {
    if (jabs_vfs_is_descriptor(handle)) { if (!jabs_vfs_close(handle)) jabs_nio_windows_exception(ERROR_INVALID_HANDLE); return; }
    if (!FindClose(as_handle(handle))) jabs_nio_windows_exception((int32_t)GetLastError());
}

void jabs_nio_get_file_attributes_ex(int64_t path, int64_t data) {
    jabs_vfs_node node;
    if (jabs_vfs_find_wide((const uint16_t *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        if (node.kind == JABS_VFS_MISSING) { vfs_missing(&node); return; }
        WIN32_FILE_ATTRIBUTE_DATA *out = (WIN32_FILE_ATTRIBUTE_DATA *)(intptr_t)data;
        memset(out, 0, sizeof *out);
        out->dwFileAttributes = vfs_attributes(node.kind);
        out->ftCreationTime = out->ftLastAccessTime = out->ftLastWriteTime = vfs_filetime();
        out->nFileSizeHigh = (DWORD)(node.size >> 32);
        out->nFileSizeLow = (DWORD)node.size;
        return;
    }
    if (!GetFileAttributesExW((LPCWSTR)(intptr_t)path, GetFileExInfoStandard, (LPVOID)(intptr_t)data)) jabs_nio_windows_exception((int32_t)GetLastError());
}

void jabs_nio_get_file_information(int64_t handle, int64_t data) {
    if (jabs_vfs_is_descriptor(handle)) {
        uint64_t id = 0;
        int32_t kind = jabs_vfs_descriptor_kind(handle, &id);
        if (kind == JABS_VFS_MISSING) { jabs_nio_windows_exception(ERROR_INVALID_HANDLE); return; }
        int64_t size = kind == JABS_VFS_FILE ? jabs_vfs_size(handle) : 0;
        BY_HANDLE_FILE_INFORMATION *out = (BY_HANDLE_FILE_INFORMATION *)(intptr_t)data;
        memset(out, 0, sizeof *out);
        out->dwFileAttributes = vfs_attributes(kind);
        out->ftCreationTime = out->ftLastAccessTime = out->ftLastWriteTime = vfs_filetime();
        out->dwVolumeSerialNumber = 0x4A41425Fu; /* "JAB_": one volume for the whole tree */
        out->nFileSizeHigh = (DWORD)((uint64_t)size >> 32);
        out->nFileSizeLow = (DWORD)size;
        out->nNumberOfLinks = 1;
        out->nFileIndexHigh = (DWORD)(id >> 32);
        out->nFileIndexLow = (DWORD)id;
        return;
    }
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
    if (jabs_vfs_is_descriptor(handle)) { jabs_nio_windows_exception(ERROR_NOT_A_REPARSE_POINT); return; }
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
    if (jabs_vfs_is_descriptor(fd)) return as_handle(fd); /* a java.io stream on the .jab */
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
    if (jabs_vfs_is_descriptor(handle_value(h))) {
        int64_t n = jabs_vfs_read(handle_value(h), (void *)(intptr_t)address, (size_t)length);
        if (n < 0) { jabs_raise_message(JABS_VM_IO, "Read failed"); return JABS_NIO_THROWN; }
        return n > 0 ? (int32_t)n : JABS_NIO_EOF;
    }
    DWORD got = 0;
    jabs_gc_enter_native();
    BOOL ok = ReadFile(h, (LPVOID)(intptr_t)address, (DWORD)length, &got, NULL);
    DWORD error = ok ? 0 : GetLastError();
    jabs_gc_exit_native();
    if (!ok) {
        if (error == ERROR_BROKEN_PIPE) return JABS_NIO_EOF;
        if (error == ERROR_NO_DATA) return JABS_NIO_UNAVAILABLE;
        raise_last_error(error, "Read failed");
        return JABS_NIO_THROWN;
    }
    if (got > (DWORD)length) jabs_not_compiled("ReadFile read more than asked (jabs_nio_file_read, jabs_nio_fs.c)");
    return got > 0 ? (int32_t)got : JABS_NIO_EOF;
}

/* FileDispatcherImpl.size0: GetFileSizeEx, as the JDK's ("Size failed"). */
int64_t jabs_nio_file_size(jabs_object *fdo) {
    HANDLE h = descriptor_handle(fdo);
    if (jabs_pending) return -1;
    if (jabs_vfs_is_descriptor(handle_value(h))) return jabs_vfs_size(handle_value(h));
    LARGE_INTEGER size;
    if (!GetFileSizeEx(h, &size)) { raise_last_error(GetLastError(), "Size failed"); return -1; }
    return size.QuadPart;
}

/* FileDispatcherImpl.seek0: a negative offset asks for the position ("Seek failed"). */
int64_t jabs_nio_file_seek(jabs_object *fdo, int64_t offset) {
    HANDLE h = descriptor_handle(fdo);
    if (jabs_pending) return -1;
    if (jabs_vfs_is_descriptor(handle_value(h))) {
        int64_t r = offset < 0 ? jabs_vfs_position(handle_value(h)) : jabs_vfs_seek(handle_value(h), offset, 0);
        if (r < 0) jabs_raise_message(JABS_VM_IO, "Seek failed");
        return r;
    }
    LARGE_INTEGER distance, where;
    distance.QuadPart = offset < 0 ? 0 : offset;
    if (!SetFilePointerEx(h, distance, &where, offset < 0 ? FILE_CURRENT : FILE_BEGIN)) { raise_last_error(GetLastError(), "Seek failed"); return -1; }
    return where.QuadPart;
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
int64_t jabs_nio_unix_size(jabs_object *fdo) { (void)fdo; jabs_not_compiled("UnixFileDispatcherImpl.size0 on Windows (jabs_nio_fs.c)"); }
int64_t jabs_nio_unix_seek(jabs_object *fdo, int64_t offset) { (void)fdo; (void)offset; jabs_not_compiled("UnixFileDispatcherImpl.seek0 on Windows (jabs_nio_fs.c)"); }
#else
/* ---- UnixNativeDispatcher (Linux) ---- */

/* init: the capabilities the JDK reports on Linux with statx (65546 on the reference JVM):
 * openat and friends, extended attributes, birth times. */
#define JABS_UNIX_SUPPORTS_OPENAT (1 << 1)
#define JABS_UNIX_SUPPORTS_XATTR (1 << 3)
#define JABS_UNIX_SUPPORTS_BIRTHTIME (1 << 16)

/* ---- The .jab's virtual tree (jabs_jab.c): read-only, as a read-only file system ---- */

static void vfs_missing(const jabs_vfs_node *node) { (void)node; jabs_nio_unix_exception(ENOENT); }

static void vfs_stat(const jabs_vfs_node *node, jabs_nio_stat_result *s) {
    memset(s, 0, sizeof *s);
    s->mode = node->kind == JABS_VFS_DIRECTORY ? (int32_t)(S_IFDIR | 0555) : (int32_t)(S_IFREG | 0444);
    s->ino = (int64_t)node->id;
    s->dev = 0x4A4142; /* "JAB": one device for the whole tree */
    s->nlink = 1;
    s->uid = (int32_t)getuid();
    s->gid = (int32_t)getgid();
    s->size = node->kind == JABS_VFS_FILE ? (int64_t)node->size : 0;
    int64_t ms = jabs_vfs_mtime_ms();
    s->atime_sec = s->mtime_sec = s->ctime_sec = ms / 1000;
    s->atime_nsec = s->mtime_nsec = s->ctime_nsec = (ms % 1000) * 1000000;
}

static jabs_object *vfs_bytes(const char *text) {
    size_t n = strlen(text);
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), text, n);
    return bytes;
}

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
    jabs_vfs_node node;
    if (jabs_vfs_find((const char *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        int writes = (flags & O_ACCMODE) != O_RDONLY || (flags & (O_CREAT | O_TRUNC | O_APPEND));
        if (node.kind == JABS_VFS_MISSING) {
            jabs_nio_unix_exception((flags & O_CREAT) && node.parent_exists ? EROFS : ENOENT);
            return -1;
        }
        if (writes) { jabs_nio_unix_exception(node.kind == JABS_VFS_DIRECTORY ? EISDIR : EROFS); return -1; }
        int64_t d = jabs_vfs_open(&node, node.kind == JABS_VFS_DIRECTORY ? 1 : 0);
        if (d < 0) { jabs_nio_unix_exception(EMFILE); return -1; }
        return (int32_t)d;
    }
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
    jabs_vfs_node node;
    if (jabs_vfs_find((const char *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        if (node.kind == JABS_VFS_MISSING) {
            if (follow) return ENOENT;
            vfs_missing(&node);
            return 0;
        }
        jabs_nio_stat_result s;
        vfs_stat(&node, &s);
        jabs_nio_set_unix_attributes(attributes, &s);
        return 0;
    }
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
    jabs_vfs_node node;
    if (jabs_vfs_find((const char *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) { jabs_nio_unix_exception(node.kind == JABS_VFS_MISSING ? ENOENT : EROFS); return; }
    if (unlink((const char *)(intptr_t)path) == -1) jabs_nio_unix_exception(errno);
}

/* access0: 0, or the errno (no exception), retried on EINTR. */
int32_t jabs_nio_access(int64_t path, int32_t mode) {
    jabs_vfs_node node;
    if (jabs_vfs_find((const char *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        if (node.kind == JABS_VFS_MISSING) return ENOENT;
        if (mode & W_OK) return EROFS;
        if ((mode & X_OK) && node.kind == JABS_VFS_FILE) return EACCES;
        return 0;
    }
    int r;
    do r = access((const char *)(intptr_t)path, mode);
    while (r == -1 && errno == EINTR);
    return r == -1 ? errno : 0;
}

/* close0: an interrupted close is not an error (the descriptor is gone either way). */
void jabs_nio_close(int32_t fd) {
    if (jabs_vfs_is_descriptor(fd)) { if (!jabs_vfs_close(fd)) jabs_nio_unix_exception(EBADF); return; }
    if (close(fd) == -1 && errno != EINTR) jabs_nio_unix_exception(errno);
}

int32_t jabs_nio_dup(int32_t fd) {
    if (jabs_vfs_is_descriptor(fd)) jabs_not_compiled("dup of a descriptor of the .jab (jabs_nio_dup, jabs_nio_fs.c)");
    int r;
    do r = dup(fd);
    while (r == -1 && errno == EINTR);
    if (r == -1) jabs_nio_unix_exception(errno);
    return r;
}

/* fdopendir / closedir / readdir0: a DIR* travels to Java as a long. */
int64_t jabs_nio_fdopendir(int32_t fd) {
    /* A directory of the tree: its descriptor is already a listing, and stands in for the DIR*. */
    if (jabs_vfs_is_descriptor(fd)) {
        if (jabs_vfs_descriptor_kind(fd, NULL) != JABS_VFS_DIRECTORY) { jabs_nio_unix_exception(ENOTDIR); return 0; }
        return fd;
    }
    DIR *d = fdopendir(fd);
    if (!d) { jabs_nio_unix_exception(errno); return 0; }
    return (int64_t)(intptr_t)d;
}

void jabs_nio_closedir(int64_t dir) {
    if (jabs_vfs_is_descriptor(dir)) { if (!jabs_vfs_close(dir)) jabs_nio_unix_exception(EBADF); return; }
    if (closedir((DIR *)(intptr_t)dir) == -1 && errno != EINTR) jabs_nio_unix_exception(errno);
}

/* The next entry's name, or null at the end (errno still 0). */
jabs_object *jabs_nio_readdir(int64_t dir) {
    if (jabs_vfs_is_descriptor(dir)) {
        char *name = NULL;
        int32_t is_directory = 0;
        uint64_t size = 0;
        int32_t got = jabs_vfs_next_name(dir, &name, &is_directory, &size);
        if (got < 0) { jabs_nio_unix_exception(EBADF); return NULL; }
        if (got == 0) return NULL;
        jabs_object *bytes = vfs_bytes(name);
        free(name);
        return bytes;
    }
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
    jabs_vfs_node node;
    if (jabs_vfs_find((const char *)(intptr_t)path, &node) != JABS_VFS_OUTSIDE) {
        if (node.kind == JABS_VFS_MISSING) { vfs_missing(&node); return NULL; }
        /* No links inside a .jab; the JDK passes an absolute, normalized path. */
        return vfs_bytes((const char *)(intptr_t)path);
    }
    char resolved[PATH_MAX + 1];
    if (!realpath((const char *)(intptr_t)path, resolved)) { jabs_nio_unix_exception(errno); return NULL; }
    size_t n = strlen(resolved);
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), resolved, n);
    return bytes;
}

/* ---- UnixFileDispatcherImpl ---- */

static void nio_raise_errno(int code, const char *fallback) {
    /* strerror is thread-safe in glibc 2.32 and later; _GNU_SOURCE hides the XSI strerror_r. */
    jabs_raise_message(JABS_VM_IO, code != 0 ? strerror(code) : fallback);
}

void jabs_nio_close_int_fd(int32_t fd) {
    if (jabs_vfs_is_descriptor(fd)) { if (!jabs_vfs_close(fd)) nio_raise_errno(EBADF, "Close failed"); return; }
    if (fd != -1 && close(fd) < 0) nio_raise_errno(errno, "Close failed");
}

/* read0: convertReturnVal on a read. */
int32_t jabs_nio_unix_read(jabs_object *fdo, int64_t address, int32_t length) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return JABS_NIO_THROWN; }
    if (length < 0) jabs_not_compiled("UnixFileDispatcherImpl.read0 with a negative length (jabs_nio_fs.c)");
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    if (jabs_vfs_is_descriptor(fd)) {
        int64_t got = jabs_vfs_read(fd, (void *)(intptr_t)address, (size_t)length);
        if (got < 0) { nio_raise_errno(EIO, "Read failed"); return JABS_NIO_THROWN; }
        return got > 0 ? (int32_t)got : JABS_NIO_EOF;
    }
    jabs_gc_enter_native();
    ssize_t n = read(fd, (void *)(intptr_t)address, (size_t)length);
    int err = n < 0 ? errno : 0;
    jabs_gc_exit_native();
    if (n > 0) return (int32_t)n;
    if (n == 0) return JABS_NIO_EOF;
    if (err == EAGAIN || err == EWOULDBLOCK) return JABS_NIO_UNAVAILABLE;
    if (err == EINTR) return JABS_NIO_INTERRUPTED;
    nio_raise_errno(err, "Read failed");
    return JABS_NIO_THROWN;
}

/* size0: fstat's size ("fstat64 failed"). */
int64_t jabs_nio_unix_size(jabs_object *fdo) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return -1; }
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    if (jabs_vfs_is_descriptor(fd)) return jabs_vfs_size(fd);
    struct stat s;
    if (fstat(fd, &s) != 0) { nio_raise_errno(errno, "fstat64 failed"); return -1; }
    return (int64_t)s.st_size;
}

/* seek0: a negative offset asks for the position ("lseek64 failed"). */
int64_t jabs_nio_unix_seek(jabs_object *fdo, int64_t offset) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return -1; }
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    if (jabs_vfs_is_descriptor(fd)) {
        int64_t r = offset < 0 ? jabs_vfs_position(fd) : jabs_vfs_seek(fd, offset, 0);
        if (r < 0) nio_raise_errno(EINVAL, "lseek64 failed");
        return r;
    }
    off_t r = offset < 0 ? lseek(fd, 0, SEEK_CUR) : lseek(fd, (off_t)offset, SEEK_SET);
    if (r < 0) { nio_raise_errno(errno, "lseek64 failed"); return -1; }
    return (int64_t)r;
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
int64_t jabs_nio_file_size(jabs_object *fdo) { (void)fdo; jabs_not_compiled("FileDispatcherImpl.size0 (Windows) on Linux (jabs_nio_fs.c)"); }
int64_t jabs_nio_file_seek(jabs_object *fdo, int64_t offset) { (void)fdo; (void)offset; jabs_not_compiled("FileDispatcherImpl.seek0 (Windows) on Linux (jabs_nio_fs.c)"); }
int64_t jabs_nio_allocation_granularity(void) { return sysconf(_SC_PAGESIZE); }
int32_t jabs_nio_max_direct_transfer_size(void) { jabs_not_compiled("FileDispatcherImpl.maxDirectTransferSize0 on Linux (jabs_nio_fs.c)"); }
#endif

#ifdef _WIN32
int32_t jabs_nio_close_descriptor_handle(int64_t handle) {
    if (jabs_vfs_is_descriptor(handle)) return jabs_vfs_close(handle);
    return CloseHandle(as_handle(handle)) != 0;
}
#else
int32_t jabs_nio_close_descriptor_handle(int64_t handle) { if (jabs_vfs_is_descriptor(handle)) return jabs_vfs_close(handle); jabs_not_compiled("a FileDescriptor with a handle on Linux (jabs_nio_close_descriptor_handle, jabs_nio_fs.c)"); }
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
