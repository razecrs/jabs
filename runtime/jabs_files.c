/* Files and file descriptors for the native runtime: the operating-system half of java.io.
 * Paths are UTF-8. Windows and POSIX (Linux, macOS) implement the same functions; the Java
 * glue (jabs_io.c) and the generated code never call an OS API themselves.
 * On failure a function returns its error value and jabs_files_error() describes why. */
/* POSIX 2008 (st_mtim, O_CLOEXEC, utimensat, strdup) and XSI (realpath) are hidden by -std=c11
 * unless asked for. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#ifndef _XOPEN_SOURCE
#define _XOPEN_SOURCE 700
#endif
#endif
#include "jabs_unit.h"

static char error_text[512];

const char *jabs_files_error(void) { return error_text; }

#ifdef _WIN32
#include <windows.h>
#include <io.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <share.h>
#include <direct.h>
#include <wctype.h>

/* A malloc'd wide copy of a UTF-8 string. */
static wchar_t *wide(const char *s) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    wchar_t *w = n > 0 ? (wchar_t *)malloc((size_t)n * sizeof(wchar_t)) : NULL;
    if (w) MultiByteToWideChar(CP_UTF8, 0, s, -1, w, n);
    return w;
}

/* A malloc'd UTF-8 copy of a wide string. */
static char *narrow(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    char *s = n > 0 ? (char *)malloc((size_t)n) : NULL;
    if (s) WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL);
    return s;
}

/* Record a Win32 error code's system message, as the JDK's own messages use. */
static void win32_error(DWORD code) {
    wchar_t text[256];
    DWORD n = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, NULL, code, 0, text, 256, NULL);
    while (n > 0 && (text[n - 1] == L'\n' || text[n - 1] == L'\r' || text[n - 1] == L'.')) text[--n] = 0;
    if (n == 0) { snprintf(error_text, sizeof error_text, "error %lu", (unsigned long)code); return; }
    WideCharToMultiByte(CP_UTF8, 0, text, -1, error_text, sizeof error_text, NULL, NULL);
}

/* 100 ns intervals since 1601 <-> milliseconds since 1970. */
static int64_t filetime_ms(FILETIME t) {
    int64_t v = ((int64_t)t.dwHighDateTime << 32) | t.dwLowDateTime;
    return (v - 116444736000000000LL) / 10000;
}

static FILETIME ms_filetime(int64_t ms) {
    int64_t v = ms * 10000 + 116444736000000000LL;
    FILETIME t = { (DWORD)v, (DWORD)(v >> 32) };
    return t;
}

/* Attributes of a path; 0 when it does not exist. */
static int attributes(const char *path, WIN32_FILE_ATTRIBUTE_DATA *data) {
    wchar_t *w = wide(path);
    int ok = w && GetFileAttributesExW(w, GetFileExInfoStandard, data);
    free(w);
    return ok;
}

int32_t jabs_fs_attributes(const char *path, int32_t *hidden) {
    WIN32_FILE_ATTRIBUTE_DATA d;
    *hidden = 0;
    if (!attributes(path, &d)) return 0;
    *hidden = (d.dwFileAttributes & FILE_ATTRIBUTE_HIDDEN) != 0;
    return JABS_FS_EXISTS | ((d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) ? JABS_FS_DIRECTORY
                            : (d.dwFileAttributes & FILE_ATTRIBUTE_DEVICE) ? 0 : JABS_FS_REGULAR);
}

int32_t jabs_fs_access(const char *path, int32_t access) {
    WIN32_FILE_ATTRIBUTE_DATA d;
    if (!attributes(path, &d)) return 0;
    /* Windows has no read or execute bits; writing needs a directory or a non-read-only file. */
    if (access & JABS_FS_ACCESS_WRITE)
        return (d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || !(d.dwFileAttributes & FILE_ATTRIBUTE_READONLY);
    return 1;
}

int64_t jabs_fs_mtime(const char *path) {
    WIN32_FILE_ATTRIBUTE_DATA d;
    return attributes(path, &d) ? filetime_ms(d.ftLastWriteTime) : 0;
}

int64_t jabs_fs_length(const char *path) {
    WIN32_FILE_ATTRIBUTE_DATA d;
    if (!attributes(path, &d) || (d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) return 0;
    return ((int64_t)d.nFileSizeHigh << 32) | d.nFileSizeLow;
}

int32_t jabs_fs_create_exclusive(const char *path) {
    wchar_t *w = wide(path);
    if (!w) { strcpy(error_text, "out of memory"); return -1; }
    HANDLE h = CreateFileW(w, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, NULL);
    DWORD code = GetLastError();
    free(w);
    if (h != INVALID_HANDLE_VALUE) { CloseHandle(h); return 1; }
    if (code == ERROR_FILE_EXISTS || code == ERROR_ALREADY_EXISTS) return 0;
    if (code == ERROR_ACCESS_DENIED) {
        int32_t hidden;
        if (jabs_fs_attributes(path, &hidden) & JABS_FS_EXISTS) return 0; /* e.g. an existing directory */
    }
    win32_error(code);
    return -1;
}

int32_t jabs_fs_delete(const char *path, int32_t allow_read_only) {
    wchar_t *w = wide(path);
    if (!w) return 0;
    DWORD a = GetFileAttributesW(w);
    int ok = 0;
    if (a != INVALID_FILE_ATTRIBUTES) {
        if ((a & FILE_ATTRIBUTE_READONLY) && allow_read_only) SetFileAttributesW(w, a & ~(DWORD)FILE_ATTRIBUTE_READONLY);
        ok = (a & FILE_ATTRIBUTE_DIRECTORY) ? RemoveDirectoryW(w) : DeleteFileW(w);
    }
    free(w);
    return ok != 0;
}

int32_t jabs_fs_mkdir(const char *path) {
    wchar_t *w = wide(path);
    int ok = w && CreateDirectoryW(w, NULL);
    free(w);
    return ok;
}

int32_t jabs_fs_rename(const char *from, const char *to) {
    wchar_t *a = wide(from), *b = wide(to);
    int ok = a && b && _wrename(a, b) == 0;
    free(a);
    free(b);
    return ok;
}

int32_t jabs_fs_set_mtime(const char *path, int64_t ms) {
    wchar_t *w = wide(path);
    if (!w) return 0;
    HANDLE h = CreateFileW(w, FILE_WRITE_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
    free(w);
    if (h == INVALID_HANDLE_VALUE) return 0;
    FILETIME t = ms_filetime(ms);
    int ok = SetFileTime(h, NULL, NULL, &t) != 0;
    CloseHandle(h);
    return ok;
}

int32_t jabs_fs_set_read_only(const char *path) {
    wchar_t *w = wide(path);
    DWORD a = w ? GetFileAttributesW(w) : INVALID_FILE_ATTRIBUTES;
    int ok = a != INVALID_FILE_ATTRIBUTES && SetFileAttributesW(w, a | FILE_ATTRIBUTE_READONLY);
    free(w);
    return ok;
}

int32_t jabs_fs_set_permission(const char *path, int32_t access, int32_t enable, int32_t owner_only) {
    (void)owner_only;
    /* Only the write permission maps to something on Windows (the read-only attribute). */
    if (!(access & JABS_FS_ACCESS_WRITE)) return enable;
    wchar_t *w = wide(path);
    DWORD a = w ? GetFileAttributesW(w) : INVALID_FILE_ATTRIBUTES;
    int ok = 0;
    if (a != INVALID_FILE_ATTRIBUTES) {
        if (a & FILE_ATTRIBUTE_DIRECTORY) ok = 1;
        else ok = SetFileAttributesW(w, enable ? (a & ~(DWORD)FILE_ATTRIBUTE_READONLY) : (a | FILE_ATTRIBUTE_READONLY)) != 0;
    }
    free(w);
    return ok;
}

char **jabs_fs_list(const char *path, int32_t *count) {
    size_t n = strlen(path);
    char *pattern = (char *)malloc(n + 3);
    if (!pattern) return NULL;
    memcpy(pattern, path, n);
    if (n > 0 && path[n - 1] != '\\' && path[n - 1] != '/') pattern[n++] = '\\';
    pattern[n++] = '*';
    pattern[n] = 0;
    wchar_t *w = wide(pattern);
    free(pattern);
    if (!w) return NULL;
    WIN32_FIND_DATAW f;
    HANDLE h = FindFirstFileW(w, &f);
    free(w);
    if (h == INVALID_HANDLE_VALUE) {
        /* An empty directory still lists (as nothing); anything else is "not a directory". */
        int32_t hidden;
        if (GetLastError() == ERROR_FILE_NOT_FOUND && (jabs_fs_attributes(path, &hidden) & JABS_FS_DIRECTORY)) {
            *count = 0;
            return (char **)calloc(1, sizeof(char *));
        }
        return NULL;
    }
    size_t used = 0, capacity = 16;
    char **names = (char **)malloc(capacity * sizeof(char *));
    do {
        if (!names) break;
        if (wcscmp(f.cFileName, L".") == 0 || wcscmp(f.cFileName, L"..") == 0) continue;
        if (used == capacity) {
            char **more = (char **)realloc(names, (capacity *= 2) * sizeof(char *));
            if (!more) { free(names); names = NULL; break; }
            names = more;
        }
        names[used++] = narrow(f.cFileName);
    } while (FindNextFileW(h, &f));
    FindClose(h);
    *count = (int32_t)used;
    return names;
}

char *jabs_fs_canonicalize(const char *path) {
    wchar_t *w = wide(path);
    if (!w) return NULL;
    DWORD n = GetFullPathNameW(w, 0, NULL, NULL);
    wchar_t *full = n ? (wchar_t *)malloc((n + 1) * sizeof(wchar_t)) : NULL;
    if (!full || !GetFullPathNameW(w, n + 1, full, NULL)) { win32_error(GetLastError()); free(w); free(full); return NULL; }
    free(w);
    /* Each existing component takes its on-disk spelling, as the JDK's canonicalize does. */
    size_t len = wcslen(full);
    size_t start = (len >= 2 && full[1] == L':') ? 3 : 0;
    if (start == 3 && full[0] >= L'a' && full[0] <= L'z') full[0] = (wchar_t)(full[0] - L'a' + L'A');
    for (size_t i = start; i <= len; i++) {
        if (i < len && full[i] != L'\\') continue;
        wchar_t saved = full[i];
        full[i] = 0;
        WIN32_FIND_DATAW f;
        HANDLE h = (i > start) ? FindFirstFileW(full, &f) : INVALID_HANDLE_VALUE;
        if (h != INVALID_HANDLE_VALUE) {
            FindClose(h);
            wchar_t *component = wcsrchr(full, L'\\');
            component = component ? component + 1 : full;
            if (wcslen(f.cFileName) == wcslen(component) && wcschr(component, L'*') == NULL && wcschr(component, L'?') == NULL)
                memcpy(component, f.cFileName, wcslen(component) * sizeof(wchar_t));
        }
        full[i] = saved;
        if (h == INVALID_HANDLE_VALUE && i > start) break; /* the rest does not exist: keep it as written */
    }
    char *out = narrow(full);
    free(full);
    return out;
}

char *jabs_fs_final_path(const char *path) {
    wchar_t *w = wide(path);
    if (!w) return NULL;
    HANDLE h = CreateFileW(w, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
    free(w);
    if (h == INVALID_HANDLE_VALUE) { win32_error(GetLastError()); return NULL; }
    wchar_t buffer[32768];
    DWORD n = GetFinalPathNameByHandleW(h, buffer, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    DWORD code = GetLastError();
    CloseHandle(h);
    if (n == 0 || n >= 32768) { win32_error(code); return NULL; }
    const wchar_t *p = buffer;
    if (wcsncmp(p, L"\\\\?\\UNC\\", 8) == 0) { p += 6; buffer[6] = L'\\'; }
    else if (wcsncmp(p, L"\\\\?\\", 4) == 0) p += 4;
    return narrow(p);
}

char *jabs_fs_drive_directory(int32_t drive) {
    wchar_t buffer[MAX_PATH + 1];
    if (!_wgetdcwd(drive, buffer, MAX_PATH + 1)) return NULL;
    const wchar_t *p = buffer;
    if (iswalpha(p[0]) && p[1] == L':') p += 2;
    return narrow(p);
}

int32_t jabs_fs_roots(void) { return (int32_t)GetLogicalDrives(); }

int64_t jabs_fs_space(const char *path, int32_t which) {
    ULARGE_INTEGER usable, total, free_bytes;
    wchar_t *w = wide(path);
    int ok = w && GetDiskFreeSpaceExW(w, &usable, &total, &free_bytes);
    free(w);
    if (!ok) return 0;
    return (int64_t)(which == JABS_FS_SPACE_TOTAL ? total.QuadPart : which == JABS_FS_SPACE_FREE ? free_bytes.QuadPart : usable.QuadPart);
}

int32_t jabs_fd_open(const char *path, int32_t flags) {
    int mode = _O_BINARY | _O_NOINHERIT;
    if ((flags & JABS_OPEN_READ) && (flags & JABS_OPEN_WRITE)) mode |= _O_RDWR;
    else if (flags & JABS_OPEN_WRITE) mode |= _O_WRONLY;
    else mode |= _O_RDONLY;
    if (flags & JABS_OPEN_CREATE) mode |= _O_CREAT;
    if (flags & JABS_OPEN_TRUNCATE) mode |= _O_TRUNC;
    if (flags & JABS_OPEN_APPEND) mode |= _O_APPEND;
    if (flags & JABS_OPEN_DELETE_ON_CLOSE) mode |= _O_TEMPORARY;
    wchar_t *w = wide(path);
    if (!w) { strcpy(error_text, "out of memory"); return -1; }
    int32_t hidden;
    if (jabs_fs_attributes(path, &hidden) & JABS_FS_DIRECTORY) { free(w); strcpy(error_text, "Is a directory"); return -1; }
    if (flags & (JABS_OPEN_SYNC | JABS_OPEN_DSYNC)) {
        /* The CRT's open flags have no synchronous-write mode. Give it ownership of
         * a write-through Win32 handle instead. DSYNC may provide the stronger SYNC
         * guarantee; ordinary descriptors keep their existing buffered behavior. */
        DWORD access = (flags & JABS_OPEN_WRITE) ? GENERIC_WRITE : 0;
        if (flags & JABS_OPEN_READ || !(flags & JABS_OPEN_WRITE)) access |= GENERIC_READ;
        DWORD disposition = (flags & JABS_OPEN_CREATE)
            ? ((flags & JABS_OPEN_TRUNCATE) ? CREATE_ALWAYS : OPEN_ALWAYS)
            : ((flags & JABS_OPEN_TRUNCATE) ? TRUNCATE_EXISTING : OPEN_EXISTING);
        DWORD attributes = FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH;
        if (flags & JABS_OPEN_DELETE_ON_CLOSE) {
            access |= DELETE;
            attributes |= FILE_FLAG_DELETE_ON_CLOSE;
        }
        HANDLE h = CreateFileW(w, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                              NULL, disposition, attributes, NULL);
        DWORD code = GetLastError();
        free(w);
        if (h == INVALID_HANDLE_VALUE) { win32_error(code); return -1; }
        int fd = _open_osfhandle((intptr_t)h, mode & (_O_BINARY | _O_APPEND | _O_RDONLY | _O_WRONLY | _O_RDWR));
        if (fd < 0) {
            snprintf(error_text, sizeof error_text, "%s", strerror(errno));
            CloseHandle(h);
        }
        return fd;
    }
    int fd = -1;
    errno_t e = _wsopen_s(&fd, w, mode, _SH_DENYNO, _S_IREAD | _S_IWRITE);
    free(w);
    if (e != 0 || fd < 0) {
        unsigned long code = 0;
        _get_doserrno(&code);
        if (code) win32_error((DWORD)code);
        else snprintf(error_text, sizeof error_text, "%s", strerror(e));
        return -1;
    }
    return fd;
}

int64_t jabs_fd_read(int32_t fd, void *buffer, size_t length) {
    /* Inherited stdin starts in CRT text mode (CRLF translation and Ctrl-Z EOF).
     * Reapply on reads so a replaced standard descriptor is handled as well. */
    if (fd >= 0 && fd <= 2 && _setmode(fd, _O_BINARY) == -1) {
        snprintf(error_text, sizeof error_text, "%s", strerror(errno));
        return -1;
    }
    int n = _read(fd, buffer, length > 0x7fffffff ? 0x7fffffffu : (unsigned)length);
    if (n < 0) { snprintf(error_text, sizeof error_text, "%s", strerror(errno)); return -1; }
    return n;
}

int64_t jabs_fd_seek(int32_t fd, int64_t offset, int32_t whence) {
    int64_t r = _lseeki64(fd, offset, whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : SEEK_END);
    if (r < 0) snprintf(error_text, sizeof error_text, "%s", strerror(errno));
    return r;
}

int64_t jabs_fd_length(int32_t fd) {
    int64_t n = _filelengthi64(fd);
    if (n < 0) snprintf(error_text, sizeof error_text, "%s", strerror(errno));
    return n;
}

int32_t jabs_fd_set_length(int32_t fd, int64_t length) {
    errno_t e = _chsize_s(fd, length);
    if (e) snprintf(error_text, sizeof error_text, "%s", strerror(e));
    return e == 0;
}

int32_t jabs_fd_is_regular(int32_t fd) {
    struct _stat64 s;
    return _fstat64(fd, &s) == 0 && (s.st_mode & _S_IFMT) == _S_IFREG;
}

int64_t jabs_fd_available(int32_t fd) {
    if (jabs_fd_is_regular(fd)) {
        int64_t here = _lseeki64(fd, 0, SEEK_CUR), end = _filelengthi64(fd);
        return here < 0 || end < 0 ? -1 : (end > here ? end - here : 0);
    }
    HANDLE h = (HANDLE)_get_osfhandle(fd);
    DWORD bytes = 0;
    if (GetFileType(h) == FILE_TYPE_PIPE && PeekNamedPipe(h, NULL, 0, NULL, &bytes, NULL)) return bytes;
    return 0; /* a console: nothing is known to be available without blocking */
}

int32_t jabs_fd_close(int32_t fd) {
    if (_close(fd) != 0) { snprintf(error_text, sizeof error_text, "%s", strerror(errno)); return 0; }
    return 1;
}

int32_t jabs_fd_sync(int32_t fd) { return _commit(fd) == 0; }

#else
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <unistd.h>

/* Record errno's message. */
static void posix_error(void) { snprintf(error_text, sizeof error_text, "%s", strerror(errno)); }

int32_t jabs_fs_attributes(const char *path, int32_t *hidden) {
    struct stat s;
    *hidden = 0; /* UnixFileSystem decides hidden-ness in Java, from the name */
    if (stat(path, &s) != 0) return 0;
    return JABS_FS_EXISTS | (S_ISDIR(s.st_mode) ? JABS_FS_DIRECTORY : S_ISREG(s.st_mode) ? JABS_FS_REGULAR : 0);
}

int32_t jabs_fs_access(const char *path, int32_t wanted) {
    int mode = 0;
    if (wanted & JABS_FS_ACCESS_READ) mode |= R_OK;
    if (wanted & JABS_FS_ACCESS_WRITE) mode |= W_OK;
    if (wanted & JABS_FS_ACCESS_EXECUTE) mode |= X_OK;
    return access(path, mode ? mode : F_OK) == 0;
}

int64_t jabs_fs_mtime(const char *path) {
    struct stat s;
    if (stat(path, &s) != 0) return 0;
#ifdef __APPLE__
    return (int64_t)s.st_mtimespec.tv_sec * 1000 + s.st_mtimespec.tv_nsec / 1000000;
#else
    return (int64_t)s.st_mtim.tv_sec * 1000 + s.st_mtim.tv_nsec / 1000000;
#endif
}

int64_t jabs_fs_length(const char *path) {
    struct stat s;
    return stat(path, &s) == 0 ? (int64_t)s.st_size : 0;
}

int32_t jabs_fs_create_exclusive(const char *path) {
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0666);
    if (fd >= 0) { close(fd); return 1; }
    if (errno == EEXIST) return 0;
    posix_error();
    return -1;
}

int32_t jabs_fs_delete(const char *path, int32_t allow_read_only) {
    (void)allow_read_only;
    return remove(path) == 0;
}

int32_t jabs_fs_mkdir(const char *path) { return mkdir(path, 0777) == 0; }

int32_t jabs_fs_rename(const char *from, const char *to) { return rename(from, to) == 0; }

int32_t jabs_fs_set_mtime(const char *path, int64_t ms) {
    struct timespec t[2];
    t[0].tv_sec = 0;
    t[0].tv_nsec = UTIME_OMIT;
    t[1].tv_sec = (time_t)(ms / 1000);
    t[1].tv_nsec = (long)(ms % 1000) * 1000000;
    return utimensat(AT_FDCWD, path, t, 0) == 0;
}

int32_t jabs_fs_set_read_only(const char *path) {
    struct stat s;
    return stat(path, &s) == 0 && chmod(path, s.st_mode & ~(mode_t)(S_IWUSR | S_IWGRP | S_IWOTH)) == 0;
}

int32_t jabs_fs_set_permission(const char *path, int32_t access, int32_t enable, int32_t owner_only) {
    mode_t bits = 0;
    if (access & JABS_FS_ACCESS_READ) bits |= owner_only ? S_IRUSR : (S_IRUSR | S_IRGRP | S_IROTH);
    if (access & JABS_FS_ACCESS_WRITE) bits |= owner_only ? S_IWUSR : (S_IWUSR | S_IWGRP | S_IWOTH);
    if (access & JABS_FS_ACCESS_EXECUTE) bits |= owner_only ? S_IXUSR : (S_IXUSR | S_IXGRP | S_IXOTH);
    struct stat s;
    if (stat(path, &s) != 0) return 0;
    return chmod(path, enable ? (s.st_mode | bits) : (s.st_mode & ~bits)) == 0;
}

char **jabs_fs_list(const char *path, int32_t *count) {
    DIR *d = opendir(path);
    if (!d) return NULL;
    size_t used = 0, capacity = 16;
    char **names = (char **)malloc(capacity * sizeof(char *));
    for (struct dirent *e; names && (e = readdir(d));) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (used == capacity) {
            char **more = (char **)realloc(names, (capacity *= 2) * sizeof(char *));
            if (!more) { free(names); names = NULL; break; }
            names = more;
        }
        names[used] = strdup(e->d_name);
        if (names[used]) used++;
    }
    closedir(d);
    *count = (int32_t)used;
    return names;
}

char *jabs_fs_canonicalize(const char *path) {
    /* realpath of the longest existing prefix, then the rest as written (as the JDK does). */
    char resolved[PATH_MAX];
    if (realpath(path, resolved)) return strdup(resolved);
    size_t n = strlen(path);
    char *prefix = strdup(path);
    if (!prefix) return NULL;
    for (size_t i = n; i > 0; i--) {
        if (prefix[i - 1] != '/') continue;
        prefix[i - 1] = 0;
        if (realpath(i > 1 ? prefix : "/", resolved)) {
            size_t a = strlen(resolved), b = strlen(path + i - 1);
            char *out = (char *)malloc(a + b + 1);
            if (out) { memcpy(out, resolved, a); memcpy(out + a, path + i - 1, b + 1); }
            if (out && a == 1) memmove(out, out + 1, b + 1); /* "/" + "/rest" */
            free(prefix);
            return out;
        }
    }
    free(prefix);
    return strdup(path);
}

char *jabs_fs_final_path(const char *path) { return jabs_fs_canonicalize(path); }

char *jabs_fs_drive_directory(int32_t drive) { (void)drive; return NULL; }

int32_t jabs_fs_roots(void) { return 0; }

int64_t jabs_fs_space(const char *path, int32_t which) {
    struct statvfs v;
    if (statvfs(path, &v) != 0) return 0;
    if (which == JABS_FS_SPACE_TOTAL) return (int64_t)v.f_frsize * (int64_t)v.f_blocks;
    if (which == JABS_FS_SPACE_FREE) return (int64_t)v.f_frsize * (int64_t)v.f_bfree;
    return (int64_t)v.f_frsize * (int64_t)v.f_bavail;
}

int32_t jabs_fd_open(const char *path, int32_t flags) {
    int mode = O_CLOEXEC;
    if ((flags & JABS_OPEN_READ) && (flags & JABS_OPEN_WRITE)) mode |= O_RDWR;
    else if (flags & JABS_OPEN_WRITE) mode |= O_WRONLY;
    else mode |= O_RDONLY;
    if (flags & JABS_OPEN_CREATE) mode |= O_CREAT;
    if (flags & JABS_OPEN_TRUNCATE) mode |= O_TRUNC;
    if (flags & JABS_OPEN_APPEND) mode |= O_APPEND;
    if (flags & JABS_OPEN_SYNC) mode |= O_SYNC;
    if (flags & JABS_OPEN_DSYNC) mode |= O_DSYNC;
    int fd = open(path, mode, 0666);
    if (fd < 0) { posix_error(); return -1; }
    struct stat s;
    if (fstat(fd, &s) == 0 && S_ISDIR(s.st_mode)) { close(fd); strcpy(error_text, "Is a directory"); return -1; }
    return fd;
}

int64_t jabs_fd_read(int32_t fd, void *buffer, size_t length) {
    ssize_t n;
    do n = read(fd, buffer, length); while (n < 0 && errno == EINTR);
    if (n < 0) posix_error();
    return n;
}

int64_t jabs_fd_seek(int32_t fd, int64_t offset, int32_t whence) {
    off_t r = lseek(fd, (off_t)offset, whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : SEEK_END);
    if (r < 0) posix_error();
    return r;
}

int64_t jabs_fd_length(int32_t fd) {
    struct stat s;
    if (fstat(fd, &s) != 0) { posix_error(); return -1; }
    return s.st_size;
}

int32_t jabs_fd_set_length(int32_t fd, int64_t length) {
    if (ftruncate(fd, (off_t)length) != 0) { posix_error(); return 0; }
    return 1;
}

int32_t jabs_fd_is_regular(int32_t fd) {
    struct stat s;
    return fstat(fd, &s) == 0 && S_ISREG(s.st_mode);
}

int64_t jabs_fd_available(int32_t fd) {
    if (jabs_fd_is_regular(fd)) {
        struct stat s;
        off_t here = lseek(fd, 0, SEEK_CUR);
        if (here < 0 || fstat(fd, &s) != 0) return -1;
        return s.st_size > here ? s.st_size - here : 0;
    }
    int n = 0;
    return ioctl(fd, FIONREAD, &n) == 0 ? n : 0;
}

int32_t jabs_fd_close(int32_t fd) {
    if (close(fd) != 0) { posix_error(); return 0; }
    return 1;
}

int32_t jabs_fd_sync(int32_t fd) { return fsync(fd) == 0; }
#endif
