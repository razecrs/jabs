/* The program's .jab (src/jab.rs writes it): its class path and its java.home, one uncompressed
 * zip. Found at startup, in this order:
 *   1. portable: an archive appended to the executable itself (its zip end record is the
 *      executable's last 22 bytes; `jabs pack`);
 *   2. standard: <canonical exe directory>/<exe name without extension>.jab.
 * Neither, or a damaged archive, stops the program before any Java code runs: every entry is
 * checked (headers, names, CRC, parent directories) and the id must be the one this program was
 * compiled with.
 *
 * The file layer then serves a virtual tree (jabs_vfs_*), read-only:
 *   <jab path>             the archive as one file (portable mode only; a real file otherwise);
 *   <jab path>/jabs-home/  java.home: the archive's jabs-home/ entries as files and directories.
 * Any other path is left to the operating system. A path under java.home that the archive does
 * not hold is missing, exactly as an absent file is: the build machine's JDK is never consulted.
 * In a recording build (--record-java-home) java.home is instead the build JDK and every path
 * touched under it is appended to the recording log; that log is what --java-home-files reads.
 *
 * Virtual descriptors (open files, directory listings) are numbers from JABS_VFS_DESCRIPTOR_BASE
 * up, which no C runtime descriptor, POSIX descriptor or Windows HANDLE takes; java.io, the
 * Windows nio handles and the Unix nio descriptors share them. The tree itself is built once at
 * startup and only read afterwards; the descriptor table and the recording log are guarded by
 * JABS_LOCK_JAB. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#ifndef _XOPEN_SOURCE
#define _XOPEN_SOURCE 700
#endif
#endif
#include "jabs_unit.h"
#ifdef _WIN32
#include <windows.h>
#else
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/stat.h>
#include <unistd.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif
#endif

#define JAB_END_HEADER 22
#define JAB_CENTRAL_HEADER 46
#define JAB_LOCAL_HEADER 30
#define JAB_MAX_ENTRIES 65535u
/* A central directory of 65535 entries with 64 KiB names would be 4 GiB; real ones are a few MiB. */
#define JAB_MAX_CENTRAL_BYTES (256u * 1024u * 1024u)
#define JAB_MAX_PATH 32768
#define JAB_CRC_CHUNK (1u << 20)
#define JAB_HOME_NAME "jabs-home"
#define JAB_MARKER "META-INF/jabs/jab.properties"
#define JAB_MAX_RECORDED 65536

typedef struct {
    const char *name;   /* NUL-terminated, in jab.names */
    uint64_t start;     /* offset of the contents in the backing file */
    uint64_t size;
    uint32_t crc;
} jab_entry;

static struct {
    int active;          /* a .jab was found and checked */
    int serve_home;      /* java.home is the archive's jabs-home/ (not a recording build) */
    int portable;        /* the archive is appended to the executable */
    char *path;          /* the .jab's path (virtual in portable mode) */
    size_t path_length;
    char *directory;     /* the directory the .jab is in */
    char *home;          /* java.home */
    uint64_t base;       /* where the archive starts in the backing file */
    uint64_t size;       /* the archive's length */
    int64_t mtime_ms;    /* the backing file's last-modified time: every virtual file's */
    jab_entry *entries;  /* sorted by name */
    uint32_t count;
    char *names;
#ifdef _WIN32
    HANDLE backing;
#else
    int backing;
#endif
    const char *record_home;  /* a recording build: the build JDK's java.home */
    size_t record_home_length;
    FILE *record_log;
    char **recorded;
    size_t recorded_count;
} jab;

/* ---- Small helpers ---- */

static _Noreturn void jab_fail(const char *where, const char *problem, const char *detail) {
    fprintf(stderr, "jabs: %s: %s%s%s\n", where, problem, detail ? " " : "", detail ? detail : "");
    fprintf(stderr, "jabs: the program cannot start without its .jab (runtime/jabs_jab.c, written by `jabs compile --jab`)\n");
    fflush(stderr);
    exit(1);
}

static uint16_t jab_get16(const unsigned char *b) { return (uint16_t)(b[0] | (b[1] << 8)); }
static uint32_t jab_get32(const unsigned char *b) { return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24); }

static int jab_is_separator(char c) {
#ifdef _WIN32
    return c == '\\' || c == '/';
#else
    return c == '/';
#endif
}

static char *jab_copy(const char *s, size_t n) {
    char *out = (char *)malloc(n + 1);
    if (!out) return NULL;
    memcpy(out, s, n);
    out[n] = 0;
    return out;
}

/* CRC-32 with a table (the check reads the whole archive at startup). */
static uint32_t jab_crc_table[256];
static void jab_crc_init(void) {
    for (uint32_t i = 0; i < 256; i++) {
        uint32_t c = i;
        for (int k = 0; k < 8; k++) c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        jab_crc_table[i] = c;
    }
}
static uint32_t jab_crc_update(uint32_t crc, const unsigned char *data, size_t length) {
    for (size_t i = 0; i < length; i++) crc = jab_crc_table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
    return crc;
}

/* ---- The backing file: positioned reads that never move a shared file pointer ---- */

#ifdef _WIN32
static wchar_t *jab_wide(const char *s) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    wchar_t *w = n > 0 ? (wchar_t *)malloc((size_t)n * sizeof(wchar_t)) : NULL;
    if (w && MultiByteToWideChar(CP_UTF8, 0, s, -1, w, n) != n) { free(w); return NULL; }
    return w;
}

static char *jab_narrow(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    char *s = n > 0 ? (char *)malloc((size_t)n) : NULL;
    if (s && WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL) != n) { free(s); return NULL; }
    return s;
}

/* 1 opened, 0 the file does not exist, -1 it exists but cannot be opened. */
static int jab_open_backing(const char *path, uint64_t *size) {
    wchar_t *w = jab_wide(path);
    if (!w) return -1;
    HANDLE h = CreateFileW(w, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_DELETE, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    DWORD code = GetLastError();
    free(w);
    if (h == INVALID_HANDLE_VALUE) return (code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND) ? 0 : -1;
    LARGE_INTEGER length;
    FILETIME written;
    if (!GetFileSizeEx(h, &length) || length.QuadPart < 0 || !GetFileTime(h, NULL, NULL, &written)) { CloseHandle(h); return -1; }
    int64_t t = ((int64_t)written.dwHighDateTime << 32) | written.dwLowDateTime;
    jab.mtime_ms = (t - 116444736000000000LL) / 10000;
    jab.backing = h;
    *size = (uint64_t)length.QuadPart;
    return 1;
}

static void jab_close_backing(void) {
    if (jab.backing) CloseHandle(jab.backing);
    jab.backing = NULL;
}

/* Read exactly `length` bytes at `offset`: 1, or 0 on a short read or an error. */
static int jab_read_at(uint64_t offset, void *buffer, size_t length) {
    unsigned char *at = (unsigned char *)buffer;
    while (length > 0) {
        DWORD chunk = length > (1u << 30) ? (1u << 30) : (DWORD)length, got = 0;
        OVERLAPPED where;
        memset(&where, 0, sizeof where);
        where.Offset = (DWORD)offset;
        where.OffsetHigh = (DWORD)(offset >> 32);
        if (!ReadFile(jab.backing, at, chunk, &got, &where) || got == 0) return 0;
        at += got;
        offset += got;
        length -= got;
    }
    return 1;
}

static char *jab_executable_path(void) {
    wchar_t *path = (wchar_t *)malloc(JAB_MAX_PATH * sizeof(wchar_t));
    if (!path) return NULL;
    DWORD n = GetModuleFileNameW(NULL, path, JAB_MAX_PATH);
    if (n == 0 || n >= JAB_MAX_PATH) { free(path); return NULL; }
    char *utf8 = jab_narrow(path);
    free(path);
    return utf8;
}
#else
static int jab_open_backing(const char *path, uint64_t *size) {
    int fd;
    do fd = open(path, O_RDONLY | O_CLOEXEC);
    while (fd < 0 && errno == EINTR);
    if (fd < 0) return errno == ENOENT ? 0 : -1;
    struct stat s;
    if (fstat(fd, &s) != 0 || !S_ISREG(s.st_mode) || s.st_size < 0) { close(fd); return -1; }
#ifdef __APPLE__
    jab.mtime_ms = (int64_t)s.st_mtimespec.tv_sec * 1000 + s.st_mtimespec.tv_nsec / 1000000;
#else
    jab.mtime_ms = (int64_t)s.st_mtim.tv_sec * 1000 + s.st_mtim.tv_nsec / 1000000;
#endif
    jab.backing = fd;
    *size = (uint64_t)s.st_size;
    return 1;
}

static void jab_close_backing(void) {
    if (jab.backing >= 0) close(jab.backing);
    jab.backing = -1;
}

static int jab_read_at(uint64_t offset, void *buffer, size_t length) {
    unsigned char *at = (unsigned char *)buffer;
    while (length > 0) {
        if (offset > (uint64_t)INT64_MAX) return 0;
        ssize_t got = pread(jab.backing, at, length, (off_t)offset);
        if (got < 0 && errno == EINTR) continue;
        if (got <= 0) return 0;
        at += got;
        offset += (uint64_t)got;
        length -= (size_t)got;
    }
    return 1;
}

static char *jab_executable_path(void) {
#ifdef __APPLE__
    char raw[PATH_MAX];
    uint32_t n = sizeof raw;
    if (_NSGetExecutablePath(raw, &n) != 0) return NULL;
    char resolved[PATH_MAX];
    if (!realpath(raw, resolved)) return NULL;
    return jab_copy(resolved, strlen(resolved));
#else
    char buffer[PATH_MAX + 1];
    ssize_t n = readlink("/proc/self/exe", buffer, PATH_MAX);
    if (n <= 0 || n >= PATH_MAX) return NULL;
    return jab_copy(buffer, (size_t)n);
#endif
}
#endif

/* ---- Checking the archive ---- */

/* The same rule as src/jab.rs name_problem: a clean relative jar name, directories ending in /. */
static const char *jab_name_problem(const char *name, size_t length) {
    if (length == 0) return "is empty";
    if (name[0] == '/') return "is absolute";
    size_t body = name[length - 1] == '/' ? length - 1 : length;
    if (body == 0) return "is only a slash";
    size_t segment = 0;
    for (size_t i = 0; i <= body; i++) {
        unsigned char c = i < body ? (unsigned char)name[i] : '/';
        if (i < body && (c < 0x20 || c == 0x7F)) return "contains a control character";
        if (c == '\\') return "contains a backslash";
        if (c != '/') continue;
        size_t n = i - segment;
        if (n == 0) return "has an empty path segment";
        if ((n == 1 && name[segment] == '.') || (n == 2 && name[segment] == '.' && name[segment + 1] == '.')) return "has a `.` or `..` segment";
        if (segment == 0 && name[i - 1] == ':') return "starts with a drive";
        segment = i + 1;
    }
    return NULL;
}

static int jab_compare_entries(const void *a, const void *b) {
    return strcmp(((const jab_entry *)a)->name, ((const jab_entry *)b)->name);
}

/* The index of the entry with this exact name, or -1. */
static int64_t jab_lookup(const char *name) {
    uint32_t low = 0, high = jab.count;
    while (low < high) {
        uint32_t mid = low + (high - low) / 2;
        int c = strcmp(jab.entries[mid].name, name);
        if (c == 0) return mid;
        if (c < 0) low = mid + 1;
        else high = mid;
    }
    return -1;
}

/* Read and check the whole archive whose end record ends the backing file of `file_size`
 * bytes. `where` names it in messages; a standard .jab must start at offset 0. */
static void jab_check(const char *where, uint64_t file_size, const char *expected_id) {
    unsigned char end[JAB_END_HEADER];
    if (file_size < JAB_END_HEADER || !jab_read_at(file_size - JAB_END_HEADER, end, sizeof end) || jab_get32(end) != 0x06054b50u)
        jab_fail(where, "is not a .jab: no zip end record in its last 22 bytes", NULL);
    if (jab_get16(end + 4) != 0 || jab_get16(end + 6) != 0) jab_fail(where, "is a split archive", NULL);
    uint32_t count = jab_get16(end + 10);
    if (jab_get16(end + 8) != count) jab_fail(where, "has an end record whose two entry counts differ", NULL);
    if (jab_get16(end + 20) != 0) jab_fail(where, "has an end record with a comment (a .jab has none)", NULL);
    uint64_t central_size = jab_get32(end + 12), central_offset = jab_get32(end + 16);
    uint64_t end_at = file_size - JAB_END_HEADER;
    if (central_size > end_at) jab_fail(where, "has a central directory larger than the file", NULL);
    uint64_t central_start = end_at - central_size;
    if (central_offset > central_start) jab_fail(where, "has a central directory offset before the start of the file", NULL);
    if (central_size > JAB_MAX_CENTRAL_BYTES) jab_fail(where, "has an implausibly large central directory", NULL);
    jab.base = central_start - central_offset;
    jab.size = file_size - jab.base;
    if (!jab.portable && jab.base != 0) jab_fail(where, "has bytes before its archive (a packed program renamed to .jab?)", NULL);
    unsigned char *central = (unsigned char *)malloc(central_size ? (size_t)central_size : 1);
    jab.entries = (jab_entry *)calloc(count ? count : 1, sizeof(jab_entry));
    jab.names = (char *)malloc((size_t)central_size + count + 1);
    if (!central || !jab.entries || !jab.names) jab_fail(where, "out of memory reading the central directory", NULL);
    if (central_size && !jab_read_at(central_start, central, (size_t)central_size)) jab_fail(where, "cannot read its central directory", NULL);
    size_t at = 0, names_used = 0;
    char detail[600];
    for (uint32_t i = 0; i < count; i++) {
        if (at + JAB_CENTRAL_HEADER > central_size || jab_get32(central + at) != 0x02014b50u) {
            snprintf(detail, sizeof detail, "(entry %u)", (unsigned)i);
            jab_fail(where, "has a missing or damaged central directory entry", detail);
        }
        const unsigned char *h = central + at;
        uint16_t flags = jab_get16(h + 8), method = jab_get16(h + 10);
        uint32_t crc = jab_get32(h + 16), compressed = jab_get32(h + 20), size = jab_get32(h + 24);
        size_t name_length = jab_get16(h + 28), extra = jab_get16(h + 30), comment = jab_get16(h + 32);
        uint64_t local = jab_get32(h + 42);
        if (at + JAB_CENTRAL_HEADER + name_length + extra + comment > central_size) jab_fail(where, "has a central directory entry that runs past the directory", NULL);
        const char *name = (const char *)h + JAB_CENTRAL_HEADER;
        snprintf(detail, sizeof detail, "`%.*s`", (int)(name_length > 400 ? 400 : name_length), name);
        if (memchr(name, 0, name_length)) jab_fail(where, "has an entry name with a NUL", detail);
        const char *problem = jab_name_problem(name, name_length);
        if (problem) { strncat(detail, " ", sizeof detail - strlen(detail) - 1); strncat(detail, problem, sizeof detail - strlen(detail) - 1); jab_fail(where, "has a bad entry name:", detail); }
        if (method != 0 || compressed != size) jab_fail(where, "has a compressed entry (a .jab stores every entry uncompressed):", detail);
        if (flags & 0x0009) jab_fail(where, "has an encrypted entry or one with a data descriptor:", detail);
        if (name[name_length - 1] == '/' && size != 0) jab_fail(where, "has a directory entry with contents:", detail);
        unsigned char lh[JAB_LOCAL_HEADER];
        uint64_t local_at = jab.base + local;
        if (local_at + JAB_LOCAL_HEADER + name_length > central_start || !jab_read_at(local_at, lh, sizeof lh) || jab_get32(lh) != 0x04034b50u)
            jab_fail(where, "has no local header where the directory says for", detail);
        size_t local_name = jab_get16(lh + 26), local_extra = jab_get16(lh + 28);
        if (jab_get16(lh + 8) != method || jab_get32(lh + 14) != crc || jab_get32(lh + 18) != size || jab_get32(lh + 22) != size
            || (jab_get16(lh + 6) & 0x0009) || local_name != name_length)
            jab_fail(where, "has a local header that disagrees with the central directory for", detail);
        char *copy = jab.names + names_used;
        if (!jab_read_at(local_at + JAB_LOCAL_HEADER, copy, name_length) || memcmp(copy, name, name_length) != 0)
            jab_fail(where, "has a local header that disagrees with the central directory for", detail);
        copy[name_length] = 0;
        names_used += name_length + 1;
        uint64_t start = local_at + JAB_LOCAL_HEADER + local_name + local_extra;
        if (start + size > central_start) jab_fail(where, "has an entry that runs into the central directory:", detail);
        jab.entries[i].name = copy;
        jab.entries[i].start = start;
        jab.entries[i].size = size;
        jab.entries[i].crc = crc;
        at += JAB_CENTRAL_HEADER + name_length + extra + comment;
    }
    if (at != central_size) jab_fail(where, "has a central directory of another size than its end record says", NULL);
    free(central);
    jab.count = count;
    qsort(jab.entries, count, sizeof(jab_entry), jab_compare_entries);
    for (uint32_t i = 1; i < count; i++)
        if (strcmp(jab.entries[i - 1].name, jab.entries[i].name) == 0) jab_fail(where, "has a duplicate entry:", jab.entries[i].name);
    /* Every parent directory has its own entry, so directory lookups and listings are complete. */
    for (uint32_t i = 0; i < count; i++) {
        const char *name = jab.entries[i].name;
        size_t n = strlen(name), body = name[n - 1] == '/' ? n - 1 : n, slash = body;
        while (slash > 0 && name[slash - 1] != '/') slash--;
        if (slash == 0) continue;
        char *parent = jab_copy(name, slash);
        if (!parent) jab_fail(where, "out of memory", NULL);
        int64_t found = jab_lookup(parent);
        free(parent);
        if (found < 0) jab_fail(where, "has no directory entry for the parent of", name);
    }
    /* The marker: this program's own archive. */
    int64_t marker = jab_lookup(JAB_MARKER);
    if (marker < 0) jab_fail(where, "is not a .jab: it has no " JAB_MARKER, NULL);
    char expected[160], found[160];
    snprintf(expected, sizeof expected, "format=1\nid=%s\n", expected_id);
    const jab_entry *m = &jab.entries[marker];
    if (m->size >= sizeof found || !jab_read_at(m->start, found, (size_t)m->size)) jab_fail(where, "has a damaged " JAB_MARKER, NULL);
    found[m->size] = 0;
    if (strcmp(found, expected) != 0) jab_fail(where, "belongs to another build of this program (its id differs; rebuild the .jab with the program)", NULL);
    /* Every entry's contents against its CRC. */
    jab_crc_init();
    unsigned char *chunk = (unsigned char *)malloc(JAB_CRC_CHUNK);
    if (!chunk) jab_fail(where, "out of memory", NULL);
    for (uint32_t i = 0; i < count; i++) {
        uint32_t crc = 0xFFFFFFFFu;
        for (uint64_t done = 0; done < jab.entries[i].size;) {
            uint64_t left = jab.entries[i].size - done;
            size_t n = left > JAB_CRC_CHUNK ? JAB_CRC_CHUNK : (size_t)left;
            if (!jab_read_at(jab.entries[i].start + done, chunk, n)) jab_fail(where, "is truncated: cannot read", jab.entries[i].name);
            crc = jab_crc_update(crc, chunk, n);
            done += n;
        }
        if (~crc != jab.entries[i].crc) jab_fail(where, "is damaged: bad CRC for", jab.entries[i].name);
    }
    free(chunk);
}

/* ---- Startup ---- */

int jabs_jab_start(void) {
    if (jabs_jab_mode == 0 || jab.active) return 0;
#ifndef _WIN32
    jab.backing = -1;
#endif
    char *exe = jab_executable_path();
    if (!exe) jab_fail("this program", "cannot find its own executable's path", NULL);
    size_t n = strlen(exe), slash = n;
    while (slash > 0 && !jab_is_separator(exe[slash - 1])) slash--;
    if (slash == 0) jab_fail(exe, "has no directory in its path", NULL);
    const char *file = exe + slash;
    const char *dot = strrchr(file, '.');
    size_t stem = dot && dot != file ? (size_t)(dot - file) : strlen(file);
    /* The canonical directory: what File.getCanonicalPath (URLClassPath) makes of the path, so
     * the class path's canonical form still names the virtual archive. Kept with its trailing
     * separator for the canonicalization, so a root directory stays a root. */
    char *raw_directory = jab_copy(exe, slash);
    if (!raw_directory) jab_fail(exe, "out of memory", NULL);
#ifdef _WIN32
    jab.directory = jabs_fs_canonicalize(raw_directory);
#else
    char resolved[PATH_MAX];
    jab.directory = realpath(raw_directory, resolved) ? jab_copy(resolved, strlen(resolved)) : NULL;
#endif
    free(raw_directory);
    if (!jab.directory) jab_fail(exe, "cannot canonicalize its directory", NULL);
    size_t d = strlen(jab.directory);
    if (d > 0 && jab_is_separator(jab.directory[d - 1])) jab.directory[--d] = 0;
    jab.path_length = d + 1 + stem + 4;
    jab.path = (char *)malloc(jab.path_length + 1);
    jab.home = (char *)malloc(jab.path_length + 1 + sizeof JAB_HOME_NAME);
    if (!jab.path || !jab.home) jab_fail(exe, "out of memory", NULL);
    snprintf(jab.path, jab.path_length + 1, "%s%s%.*s.jab", jab.directory, jabs_platform_value(JABS_PLATFORM_FILE_SEPARATOR), (int)stem, file);
    snprintf(jab.home, jab.path_length + 1 + sizeof JAB_HOME_NAME, "%s%s" JAB_HOME_NAME, jab.path, jabs_platform_value(JABS_PLATFORM_FILE_SEPARATOR));
    /* 1. Appended to the executable. An end record there is taken as this program's archive,
     * and any problem with it stops the program: never fall back to a stray file beside it. */
    uint64_t size = 0;
    if (jab_open_backing(exe, &size) != 1) jab_fail(exe, "cannot open its own executable to look for an appended .jab", NULL);
    unsigned char tail[4];
    int appended = size >= JAB_END_HEADER && jab_read_at(size - JAB_END_HEADER, tail, 4) && jab_get32(tail) == 0x06054b50u;
    if (appended) {
        jab.portable = 1;
        jab_check(exe, size, jabs_jab_id);
    } else {
        jab_close_backing();
        /* 2. Beside it. */
        int opened = jab_open_backing(jab.path, &size);
        if (opened == 0) {
            char detail[JAB_MAX_PATH];
            snprintf(detail, sizeof detail, "and %s does not exist", jab.path);
            jab_fail(exe, "has no .jab appended (jabs pack)", detail);
        }
        if (opened < 0) jab_fail(jab.path, "exists but cannot be opened for reading", NULL);
        jab_check(jab.path, size, jabs_jab_id);
    }
    free(exe);
    jab.active = 1;
    if (jabs_jab_mode == 2) {
        jab.record_home = jabs_jab_record_home;
        jab.record_home_length = strlen(jab.record_home);
        jab.record_log = fopen(jabs_jab_record_log, "ab");
        if (!jab.record_log) jab_fail(jabs_jab_record_log, "cannot be opened to record the java.home files this run reads", NULL);
    } else {
        jab.serve_home = 1;
    }
    return 0;
}

const char *jabs_jab_class_path(void) { return jab.active ? jab.path : ""; }

const char *jabs_jab_java_home(void) {
    if (jab.active && jab.serve_home) return jab.home;
    if (jab.active && jab.record_home) return jab.record_home;
    return NULL;
}

const char *jabs_jab_path(void) { return jab.active ? jab.path : NULL; }

/* ---- Paths into the tree ---- */

/* Whether `path` starts with `prefix` as a path: the same characters (separators equal to
 * each other, ASCII case ignored on Windows), followed by a separator or the end. Returns the
 * length matched, or 0. A Windows \\?\ prefix is skipped. */
static size_t jab_prefix(const char *path, const char *prefix, size_t length) {
    size_t skip = 0;
#ifdef _WIN32
    if (strncmp(path, "\\\\?\\", 4) == 0 && strncmp(path, "\\\\?\\UNC\\", 8) != 0) skip = 4;
#endif
    const char *p = path + skip;
    for (size_t i = 0; i < length; i++) {
        char a = p[i], b = prefix[i];
        if (a == 0) return 0;
        if (jab_is_separator(a) && jab_is_separator(b)) continue;
#ifdef _WIN32
        if (a >= 'A' && a <= 'Z') a = (char)(a - 'A' + 'a');
        if (b >= 'A' && b <= 'Z') b = (char)(b - 'A' + 'a');
#endif
        if (a != b) return 0;
    }
    if (p[length] != 0 && !jab_is_separator(p[length])) return 0;
    return skip + length;
}

/* `rest` (what follows a matched prefix) as a clean `/`-separated relative name in `out`:
 * empty, `.` segments dropped, `..` popped. 0 when it climbs above the prefix or is too long. */
static int jab_relative(const char *rest, char *out, size_t cap) {
    size_t used = 0;
    const char *p = rest;
    for (int guard = 0; guard < JAB_MAX_PATH; guard++) {
        while (*p && jab_is_separator(*p)) p++;
        if (!*p) { out[used] = 0; return 1; }
        const char *start = p;
        while (*p && !jab_is_separator(*p)) p++;
        size_t n = (size_t)(p - start);
        if (n == 1 && start[0] == '.') continue;
        if (n == 2 && start[0] == '.' && start[1] == '.') {
            if (used == 0) return 0;
            while (used > 0 && out[used - 1] != '/') used--;
            if (used > 0) used--;
            continue;
        }
        if (used + n + 2 > cap) return 0;
        if (used) out[used++] = '/';
        memcpy(out + used, start, n);
        used += n;
    }
    return 0;
}

/* A recording build: note a path touched under the build JDK's java.home, once. */
static void jab_record(const char *path, int listed) {
    size_t matched = jab_prefix(path, jab.record_home, jab.record_home_length);
    if (!matched) return;
    char relative[JAB_MAX_PATH];
    if (!jab_relative(path + matched, relative, sizeof relative - 3) || !relative[0]) return;
    if (listed) strcat(relative, "/*");
    jabs_lock(JABS_LOCK_JAB);
    int seen = 0;
    for (size_t i = 0; i < jab.recorded_count && !seen; i++) seen = strcmp(jab.recorded[i], relative) == 0;
    if (!seen && jab.recorded_count < JAB_MAX_RECORDED) {
        if (jab.recorded_count % 256 == 0) {
            char **more = (char **)realloc(jab.recorded, (jab.recorded_count + 256) * sizeof(char *));
            if (more) jab.recorded = more;
            else seen = 1;
        }
        char *copy = seen ? NULL : jab_copy(relative, strlen(relative));
        if (copy) {
            jab.recorded[jab.recorded_count++] = copy;
            fprintf(jab.record_log, "%s\n", relative);
            fflush(jab.record_log);
        }
    }
    jabs_unlock(JABS_LOCK_JAB);
}

int32_t jabs_vfs_find(const char *path, jabs_vfs_node *node) {
    memset(node, 0, sizeof *node);
    node->kind = JABS_VFS_OUTSIDE;
    if (!jab.active || !path) return node->kind;
    if (!jab.serve_home) {
        if (jab.record_home) jab_record(path, 0);
        if (!jab.portable) return node->kind;
    }
    size_t matched = jab_prefix(path, jab.path, jab.path_length);
    if (!matched) return node->kind;
    char relative[JAB_MAX_PATH];
    if (!jab_relative(path + matched, relative, sizeof relative - 2)) return node->kind;
    if (!relative[0]) {
        /* The archive itself: the operating system's file, unless it is appended to the exe. */
        if (!jab.portable) return node->kind;
        node->kind = JABS_VFS_FILE;
        node->start = jab.base;
        node->size = jab.size;
        node->id = 1;
        return node->kind;
    }
    /* Inside the archive file: only jabs-home is there, and only when java.home is served. */
    size_t home = sizeof JAB_HOME_NAME - 1;
    int in_home = jab.serve_home && strncmp(relative, JAB_HOME_NAME, home) == 0 && (relative[home] == 0 || relative[home] == '/');
    node->kind = JABS_VFS_MISSING;
    if (!in_home) return node->kind;
    int64_t found = jab_lookup(relative);
    if (found >= 0) {
        node->kind = JABS_VFS_FILE;
    } else {
        strcat(relative, "/");
        found = jab_lookup(relative);
        if (found >= 0) node->kind = JABS_VFS_DIRECTORY;
    }
    if (found >= 0) {
        node->index = (uint32_t)found;
        node->start = jab.entries[found].start;
        node->size = jab.entries[found].size;
        node->id = (uint64_t)found + 2;
        return node->kind;
    }
    /* Missing: is its directory there (a missing file) or not (a missing path)? */
    size_t n = strlen(relative) - 1;
    relative[n] = 0;
    while (n > 0 && relative[n - 1] != '/') n--;
    relative[n] = 0;
    node->parent_exists = n > 0 && jab_lookup(relative) >= 0;
    return node->kind;
}

#ifdef _WIN32
int32_t jabs_vfs_find_wide(const uint16_t *path, jabs_vfs_node *node) {
    memset(node, 0, sizeof *node);
    node->kind = JABS_VFS_OUTSIDE;
    if (!jab.active || !path) return node->kind;
    char *utf8 = jab_narrow((const wchar_t *)path);
    if (!utf8) return node->kind;
    int32_t kind = jabs_vfs_find(utf8, node);
    free(utf8);
    return kind;
}
#endif

int64_t jabs_vfs_mtime_ms(void) { return jab.mtime_ms; }

const char *jabs_vfs_directory(void) { return jab.directory; }

void jabs_vfs_note_listing(const char *path) {
    if (jab.active && jab.record_home) jab_record(path, 1);
}

/* The names directly inside a directory entry, malloc'd, with whether each is a directory and
 * its size (both optional). */
static char **jab_children(const jabs_vfs_node *directory, int32_t *count, unsigned char **kinds, uint64_t **sizes) {
    const char *prefix = jab.entries[directory->index].name;
    size_t n = strlen(prefix), used = 0;
    char **names = (char **)malloc(((size_t)jab.count + 1) * sizeof(char *));
    unsigned char *k = (unsigned char *)malloc((size_t)jab.count + 1);
    uint64_t *z = (uint64_t *)malloc(((size_t)jab.count + 1) * sizeof(uint64_t));
    if (!names || !k || !z) { free(names); free(k); free(z); return NULL; }
    for (uint32_t i = directory->index + 1; i < jab.count && strncmp(jab.entries[i].name, prefix, n) == 0; i++) {
        const char *rest = jab.entries[i].name + n;
        const char *slash = strchr(rest, '/');
        if (slash && slash[1] != 0) continue; /* deeper down */
        size_t length = slash ? (size_t)(slash - rest) : strlen(rest);
        names[used] = jab_copy(rest, length);
        if (!names[used]) break;
        z[used] = jab.entries[i].size;
        k[used++] = slash != NULL;
    }
    *count = (int32_t)used;
    if (kinds) *kinds = k;
    else free(k);
    if (sizes) *sizes = z;
    else free(z);
    return names;
}

char **jabs_vfs_list(const jabs_vfs_node *directory, int32_t *count) {
    return jab_children(directory, count, NULL, NULL);
}

/* ---- Virtual descriptors ---- */

typedef struct {
    int used;
    int is_directory;
    uint64_t start, size, position, id;
    /* A listing: "." and ".." first, as a real directory's (Windows FindFirstFile). */
    char **names;
    unsigned char *kinds;
    uint64_t *sizes;
    int32_t name_count, next; /* next < 0: ".." is still to come (a listing with dots) */
} jab_slot;

static jab_slot jab_slots[JABS_VFS_DESCRIPTOR_CAP];

int jabs_vfs_is_descriptor(int64_t d) { return d >= JABS_VFS_DESCRIPTOR_BASE && d < JABS_VFS_DESCRIPTOR_BASE + JABS_VFS_DESCRIPTOR_CAP; }

/* The slot of an open descriptor; the caller holds JABS_LOCK_JAB. */
static jab_slot *jab_slot_of(int64_t d) {
    if (!jabs_vfs_is_descriptor(d)) return NULL;
    jab_slot *s = &jab_slots[d - JABS_VFS_DESCRIPTOR_BASE];
    return s->used ? s : NULL;
}

int64_t jabs_vfs_open(const jabs_vfs_node *node, int32_t listing) {
    if (node->kind != JABS_VFS_FILE && node->kind != JABS_VFS_DIRECTORY) return -1;
    char **names = NULL;
    unsigned char *kinds = NULL;
    uint64_t *sizes = NULL;
    int32_t count = 0;
    if (listing) {
        if (node->kind != JABS_VFS_DIRECTORY) return -1;
        names = jab_children(node, &count, &kinds, &sizes);
        if (!names) return -1;
    }
    int64_t result = -1;
    jabs_lock(JABS_LOCK_JAB);
    for (int i = 0; i < JABS_VFS_DESCRIPTOR_CAP; i++) {
        if (jab_slots[i].used) continue;
        jab_slot *s = &jab_slots[i];
        memset(s, 0, sizeof *s);
        s->used = 1;
        s->is_directory = node->kind == JABS_VFS_DIRECTORY;
        s->start = node->start;
        s->size = node->size;
        s->id = node->id;
        s->names = names;
        s->kinds = kinds;
        s->sizes = sizes;
        s->name_count = count;
        s->next = listing == 2 ? -1 : 0;
        result = JABS_VFS_DESCRIPTOR_BASE + i;
        break;
    }
    jabs_unlock(JABS_LOCK_JAB);
    if (result < 0) {
        for (int32_t i = 0; i < count; i++) free(names[i]);
        free(names);
        free(kinds);
        free(sizes);
    }
    return result;
}

int64_t jabs_vfs_read(int64_t d, void *buffer, size_t length) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int64_t result = -1;
    if (s && !s->is_directory) {
        uint64_t left = s->position < s->size ? s->size - s->position : 0;
        size_t n = length > left ? (size_t)left : length;
        if (n == 0 || jab_read_at(s->start + s->position, buffer, n)) {
            s->position += n;
            result = (int64_t)n;
        }
    }
    jabs_unlock(JABS_LOCK_JAB);
    return result;
}

int64_t jabs_vfs_seek(int64_t d, int64_t offset, int32_t whence) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int64_t result = -1;
    if (s && !s->is_directory) {
        int64_t from = whence == 0 ? 0 : whence == 1 ? (int64_t)s->position : (int64_t)s->size;
        if ((offset >= 0 && from <= INT64_MAX - offset) || (offset < 0 && from + offset >= 0)) {
            s->position = (uint64_t)(from + offset);
            result = (int64_t)s->position;
        }
    }
    jabs_unlock(JABS_LOCK_JAB);
    return result;
}

int64_t jabs_vfs_size(int64_t d) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int64_t result = s ? (int64_t)s->size : -1;
    jabs_unlock(JABS_LOCK_JAB);
    return result;
}

int64_t jabs_vfs_position(int64_t d) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int64_t result = s ? (int64_t)s->position : -1;
    jabs_unlock(JABS_LOCK_JAB);
    return result;
}

int32_t jabs_vfs_descriptor_kind(int64_t d, uint64_t *id) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int32_t kind = !s ? JABS_VFS_MISSING : s->is_directory ? JABS_VFS_DIRECTORY : JABS_VFS_FILE;
    if (s && id) *id = s->id;
    jabs_unlock(JABS_LOCK_JAB);
    return kind;
}

/* The next name of a listing (malloc'd), 0 at its end, -1 for a bad descriptor. */
int32_t jabs_vfs_next_name(int64_t d, char **name, int32_t *is_directory, uint64_t *size) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int32_t result = -1;
    if (s && s->is_directory) {
        result = 0;
        if (s->next < 0) {
            *name = jab_copy("..", 2);
            *is_directory = 1;
            *size = 0;
            s->next = 0;
            result = *name ? 1 : -1;
        } else if (s->next < s->name_count) {
            *name = jab_copy(s->names[s->next], strlen(s->names[s->next]));
            *is_directory = s->kinds[s->next];
            *size = s->sizes[s->next];
            s->next++;
            result = *name ? 1 : -1;
        }
    }
    jabs_unlock(JABS_LOCK_JAB);
    return result;
}

int32_t jabs_vfs_close(int64_t d) {
    jabs_lock(JABS_LOCK_JAB);
    jab_slot *s = jab_slot_of(d);
    int32_t ok = s != NULL;
    if (s) {
        for (int32_t i = 0; i < s->name_count; i++) free(s->names[i]);
        free(s->names);
        free(s->kinds);
        free(s->sizes);
        memset(s, 0, sizeof *s);
    }
    jabs_unlock(JABS_LOCK_JAB);
    return ok;
}
