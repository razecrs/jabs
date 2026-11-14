/* The Java side of java.io's natives: File objects to paths, streams to descriptors, OS errors to
 * IOExceptions with the JDK's messages. Operating-system work is in jabs_files.c; object layouts
 * come from accessors the compiler generates (jabs_file_path_of, jabs_stream_fd_slot, ...).
 * Shared by WinNTFileSystem and UnixFileSystem, so a new OS needs no change here. */
#include "jabs_unit.h"

/* A File's path as malloc'd UTF-8; NULL with NullPointerException pending for a null File. */
static char *path_of(jabs_object *file) {
    if (!file) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    char *p = jabs_string_to_utf8(jabs_file_path_of(file));
    if (!p) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
    return p;
}

/* A String argument as malloc'd UTF-8 (NullPointerException for null). */
static char *utf8_arg(jabs_object *s) {
    if (!s) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    char *p = jabs_string_to_utf8(s);
    if (!p) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
    return p;
}

/* A malloc'd UTF-8 result as a String (NULL stays NULL). */
static jabs_object *string_result(char *text) {
    if (!text) return NULL;
    jabs_object *s = jabs_string_from_utf8(text);
    free(text);
    return s;
}

/* Raise IOException with the OS's reason. */
static void raise_io(void) { jabs_raise_message(JABS_VM_IO, jabs_files_error()); }

int32_t jabs_io_attributes(jabs_object *file, int32_t with_hidden) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t hidden, bits = jabs_fs_attributes(p, &hidden);
    free(p);
    return bits | (with_hidden && hidden ? JABS_FS_HIDDEN : 0);
}

int32_t jabs_io_check_access(jabs_object *file, int32_t access) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_access(p, access);
    free(p);
    return ok;
}

int64_t jabs_io_last_modified(jabs_object *file) {
    char *p = path_of(file);
    if (!p) return 0;
    int64_t t = jabs_fs_mtime(p);
    free(p);
    return t;
}

int64_t jabs_io_length(jabs_object *file) {
    char *p = path_of(file);
    if (!p) return 0;
    int64_t n = jabs_fs_length(p);
    free(p);
    return n;
}

int32_t jabs_io_set_permission(jabs_object *file, int32_t access, int32_t enable, int32_t owner_only) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_set_permission(p, access, enable, owner_only);
    free(p);
    return ok;
}

int32_t jabs_io_create_exclusively(jabs_object *path) {
    char *p = utf8_arg(path);
    if (!p) return 0;
    int32_t r = jabs_fs_create_exclusive(p);
    free(p);
    if (r < 0) { raise_io(); return 0; }
    return r;
}

jabs_object *jabs_io_list(jabs_object *file) {
    char *p = path_of(file);
    if (!p) return NULL;
    int32_t count = 0;
    char **names = jabs_fs_list(p, &count);
    free(p);
    if (!names) return NULL;
    jabs_object *array = jabs_new_string_array(count);
    for (int32_t i = 0; i < count; i++) {
        if (array && !jabs_pending) {
            jabs_object *s = jabs_string_from_utf8(names[i] ? names[i] : "");
            if (s) ((jabs_object **)jabs_array_data((jabs_array *)array))[i] = s;
        }
        free(names[i]);
    }
    free(names);
    return jabs_pending ? NULL : array;
}

int32_t jabs_io_create_directory(jabs_object *file) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_mkdir(p);
    free(p);
    return ok;
}

int32_t jabs_io_set_last_modified(jabs_object *file, int64_t ms) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_set_mtime(p, ms);
    free(p);
    return ok;
}

int32_t jabs_io_set_read_only(jabs_object *file) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_set_read_only(p);
    free(p);
    return ok;
}

int32_t jabs_io_delete(jabs_object *file, int32_t allow_read_only) {
    char *p = path_of(file);
    if (!p) return 0;
    int32_t ok = jabs_fs_delete(p, allow_read_only);
    free(p);
    return ok;
}

int32_t jabs_io_rename(jabs_object *from, jabs_object *to) {
    char *a = path_of(from);
    if (!a) return 0;
    char *b = path_of(to);
    if (!b) { free(a); return 0; }
    int32_t ok = jabs_fs_rename(a, b);
    free(a);
    free(b);
    return ok;
}

int64_t jabs_io_space(jabs_object *file, int32_t which) {
    char *p = path_of(file);
    if (!p) return 0;
    int64_t n = jabs_fs_space(p, which);
    free(p);
    return n;
}

jabs_object *jabs_io_canonicalize(jabs_object *path) {
    char *p = utf8_arg(path);
    if (!p) return NULL;
    char *c = jabs_fs_canonicalize(p);
    free(p);
    if (!c) { raise_io(); return NULL; }
    return string_result(c);
}

jabs_object *jabs_io_final_path(jabs_object *path) {
    char *p = utf8_arg(path);
    if (!p) return NULL;
    char *c = jabs_fs_final_path(p);
    free(p);
    if (!c) { raise_io(); return NULL; }
    return string_result(c);
}

jabs_object *jabs_io_drive_directory(int32_t drive) { return string_result(jabs_fs_drive_directory(drive)); }

/* ---- Streams: FileInputStream, FileOutputStream, RandomAccessFile and their FileDescriptor ---- */

/* The int descriptor behind a stream; -1 with IOException("Stream Closed") when closed. */
static int32_t stream_fd(jabs_object *stream) {
    jabs_object *fdo = *jabs_stream_fd_slot(stream);
    int32_t fd = fdo ? *jabs_descriptor_fd_slot(fdo) : -1;
    if (fd < 0) jabs_raise_message(JABS_VM_IO, "Stream Closed");
    return fd;
}

/* Bounds check for (bytes, off, len), raising as the JDK's natives do. */
static int checked_range(jabs_object *bytes, int32_t off, int32_t len) {
    if (!bytes) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }
    jabs_array *b = (jabs_array *)bytes;
    if (off < 0 || len < 0 || len > b->length - off) { jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return 0; }
    return 1;
}

int32_t jabs_io_raf_flags(int32_t mode) {
    /* RandomAccessFile: O_RDONLY 1, O_RDWR 2, O_SYNC 4, O_DSYNC 8, O_TEMPORARY 16. */
    int32_t flags = (mode & 2) ? (JABS_OPEN_READ | JABS_OPEN_WRITE | JABS_OPEN_CREATE) : JABS_OPEN_READ;
    if (mode & 4) flags |= JABS_OPEN_SYNC;
    if (mode & 8) flags |= JABS_OPEN_DSYNC;
    if (mode & 16) flags |= JABS_OPEN_DELETE_ON_CLOSE;
    return flags;
}

void jabs_io_open(jabs_object *stream, jabs_object *name, int32_t flags) {
    char *p = utf8_arg(name);
    if (!p) return;
    int32_t fd = jabs_fd_open(p, flags);
    if (fd < 0) {
        /* The JDK's message: "<path> (<reason>)". */
        const char *why = jabs_files_error();
        size_t n = strlen(p) + strlen(why) + 4;
        char *message = (char *)malloc(n);
        if (message) snprintf(message, n, "%s (%s)", p, why);
        jabs_raise_message(JABS_VM_FILE_NOT_FOUND, message ? message : p);
        free(message);
        free(p);
        return;
    }
    free(p);
    jabs_object *fdo = *jabs_stream_fd_slot(stream);
    if (!fdo) { jabs_fd_close(fd); jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    *jabs_descriptor_fd_slot(fdo) = fd;
    jabs_descriptor_set_handle(fdo, -1);
    jabs_descriptor_set_append(fdo, (flags & JABS_OPEN_APPEND) != 0);
}

int32_t jabs_io_read_byte(jabs_object *stream) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return -1;
    unsigned char byte;
    int64_t n = jabs_fd_read(fd, &byte, 1);
    if (n < 0) { raise_io(); return -1; }
    return n == 0 ? -1 : byte;
}

int32_t jabs_io_read_bytes(jabs_object *stream, jabs_object *bytes, int32_t off, int32_t len) {
    if (!checked_range(bytes, off, len)) return -1;
    if (len == 0) return 0;
    int32_t fd = stream_fd(stream);
    if (fd < 0) return -1;
    int64_t n = jabs_fd_read(fd, (char *)jabs_array_data((jabs_array *)bytes) + off, (size_t)len);
    if (n < 0) { raise_io(); return -1; }
    return n == 0 ? -1 : (int32_t)n;
}

void jabs_io_write_byte(jabs_object *stream, int32_t b) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return;
    unsigned char byte = (unsigned char)b;
    if (jabs_platform_write(fd, &byte, 1) < 0) raise_io();
}

void jabs_io_write_bytes(jabs_object *stream, jabs_object *bytes, int32_t off, int32_t len) {
    if (!checked_range(bytes, off, len)) return;
    int32_t fd = stream_fd(stream);
    if (fd < 0) return;
    if (jabs_platform_write(fd, (const char *)jabs_array_data((jabs_array *)bytes) + off, (size_t)len) < 0)
        jabs_raise_message(JABS_VM_IO, "write failed");
}

int64_t jabs_io_position(jabs_object *stream) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return -1;
    int64_t r = jabs_fd_seek(fd, 0, 1);
    if (r < 0) raise_io();
    return r;
}

void jabs_io_seek(jabs_object *stream, int64_t position) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return;
    if (position < 0) { jabs_raise_message(JABS_VM_IO, "Negative seek offset"); return; }
    if (jabs_fd_seek(fd, position, 0) < 0) raise_io();
}

int64_t jabs_io_skip(jabs_object *stream, int64_t n) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return 0;
    int64_t here = jabs_fd_seek(fd, 0, 1);
    if (here < 0) { raise_io(); return 0; }
    int64_t there = jabs_fd_seek(fd, n, 1);
    if (there < 0) { raise_io(); return 0; }
    return there - here;
}

int64_t jabs_io_stream_length(jabs_object *stream) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return -1;
    int64_t n = jabs_fd_length(fd);
    if (n < 0) raise_io();
    return n;
}

void jabs_io_set_length(jabs_object *stream, int64_t length) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return;
    int64_t here = jabs_fd_seek(fd, 0, 1);
    if (!jabs_fd_set_length(fd, length)) { raise_io(); return; }
    /* As RandomAccessFile.setLength specifies: the pointer moves to the new end if it was past it. */
    if (here > length) jabs_fd_seek(fd, length, 0);
}

int32_t jabs_io_available(jabs_object *stream) {
    int32_t fd = stream_fd(stream);
    if (fd < 0) return 0;
    int64_t n = jabs_fd_available(fd);
    if (n < 0) { raise_io(); return 0; }
    return n > 0x7fffffff ? 0x7fffffff : (int32_t)n;
}

int32_t jabs_io_is_regular(jabs_object *fdo) {
    if (!fdo) return 0;
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    return fd >= 0 && jabs_fd_is_regular(fd);
}

void jabs_io_close_descriptor(jabs_object *fdo) {
    int32_t *slot = jabs_descriptor_fd_slot(fdo);
    int32_t fd = *slot;
    *slot = -1;
    jabs_descriptor_set_handle(fdo, -1);
    /* The standard streams stay open, as in the JDK. */
    if (fd > 2 && !jabs_fd_close(fd)) raise_io();
}

void jabs_io_cleanup_close(int32_t fd) {
    if (fd > 2 && !jabs_fd_close(fd)) raise_io();
}

void jabs_io_sync(jabs_object *fdo) {
    int32_t fd = *jabs_descriptor_fd_slot(fdo);
    if (fd < 0 || !jabs_fd_sync(fd)) jabs_raise_message(JABS_VM_SYNC_FAILED, "sync failed");
}

jabs_object *jabs_map_library_name(jabs_object *name) {
    char *n = utf8_arg(name);
    if (!n) return NULL;
    const char *prefix = jabs_platform_value(JABS_PLATFORM_LIBRARY_PREFIX), *suffix = jabs_platform_value(JABS_PLATFORM_LIBRARY_SUFFIX);
    size_t size = strlen(prefix) + strlen(n) + strlen(suffix) + 1;
    char *mapped = (char *)malloc(size);
    if (mapped) snprintf(mapped, size, "%s%s%s", prefix, n, suffix);
    free(n);
    if (!mapped) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    return string_result(mapped);
}
