//! java.nio.file natives: Windows's sun.nio.fs.WindowsNativeDispatcher and
//! sun.nio.ch.FileDispatcherImpl, Linux's sun.nio.fs.UnixNativeDispatcher and
//! sun.nio.ch.UnixFileDispatcherImpl (runtime/jabs_nio_fs.c). They are thin over the OS, as the
//! JDK's are: Java passes paths and result buffers as native memory addresses and reads the OS's
//! structs itself, so each native makes the same OS call on the same memory and fails with the
//! same exception (WindowsException or UnixException with the OS's error code). java.security
//! reads its properties file through them, so every InetAddress lookup and SecureRandom needs
//! them. A native left out stops the program with a diagnostic when it is called.

use super::*;

/// Java methods a file native calls from C: the exceptions it raises.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if body(key).is_none() {
        return Vec::new();
    }
    if key.0 == PERF {
        return vec![(DIRECT_BYTE_BUFFER, "<init>", "(JJ)V")];
    }
    vec![
        (WINDOWS_EXCEPTION, "<init>", "(I)V"),
        (UNIX_EXCEPTION, "<init>", "(I)V"),
    ]
}

/// The body of a file native, if JABS implements it: (C body, needs the class registry).
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    let found = body(key)?;
    classes_available.then(|| (found.to_string(), true))
}

const WINDOWS_DISPATCHER: &str = "sun/nio/fs/WindowsNativeDispatcher";
const UNIX_DISPATCHER: &str = "sun/nio/fs/UnixNativeDispatcher";
const WINDOWS_EXCEPTION: &str = "sun/nio/fs/WindowsException";
const UNIX_EXCEPTION: &str = "sun/nio/fs/UnixException";
const FIRST_FILE: &str = "sun/nio/fs/WindowsNativeDispatcher$FirstFile";
const UNIX_ATTRIBUTES: &str = "sun/nio/fs/UnixFileAttributes";
const PERF: &str = "jdk/internal/perf/Perf";
const DIRECT_BYTE_BUFFER: &str = "java/nio/DirectByteBuffer";

/// java.nio.file natives (runtime/jabs_nio_fs.c): Windows's WindowsNativeDispatcher and
/// FileDispatcherImpl, Linux's UnixNativeDispatcher and UnixFileDispatcherImpl. Paths and
/// result buffers are native memory addresses Java passes as longs.
fn body(key: &MethodKey) -> Option<&'static str> {
    Some(match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (WINDOWS_DISPATCHER, "initIDs", "()V") => "/* field offsets are known at compile time */",
        (WINDOWS_DISPATCHER, "CreateFile0", "(JIIJII)J") => {
            "return jabs_nio_create_file(a0, a1, a2, a3, a4, a5);"
        }
        (WINDOWS_DISPATCHER, "CloseHandle", "(J)V") => "jabs_nio_close_handle(a0);",
        (WINDOWS_DISPATCHER, "DeleteFile0", "(J)V") => "jabs_nio_delete_file(a0);",
        (
            WINDOWS_DISPATCHER,
            "FindFirstFile0",
            "(JLsun/nio/fs/WindowsNativeDispatcher$FirstFile;)V",
        ) => "jabs_nio_find_first_file(a0, a1);",
        (WINDOWS_DISPATCHER, "FindNextFile0", "(JJ)Ljava/lang/String;") => {
            "return jabs_nio_find_next_file(a0, a1);"
        }
        (WINDOWS_DISPATCHER, "FindClose", "(J)V") => "jabs_nio_find_close(a0);",
        (WINDOWS_DISPATCHER, "GetFileAttributesEx0", "(JJ)V") => {
            "jabs_nio_get_file_attributes_ex(a0, a1);"
        }
        (WINDOWS_DISPATCHER, "GetFileInformationByHandle0", "(JJ)V") => {
            "jabs_nio_get_file_information(a0, a1);"
        }
        (WINDOWS_DISPATCHER, "GetFullPathName0", "(J)Ljava/lang/String;") => {
            "return jabs_nio_get_full_path_name(a0);"
        }
        (WINDOWS_DISPATCHER, "DeviceIoControlGetReparsePoint", "(JJI)V") => {
            "jabs_nio_get_reparse_point(a0, a1, a2);"
        }
        ("sun/nio/ch/FileDispatcherImpl", "read0", "(Ljava/io/FileDescriptor;JI)I") => {
            "return jabs_nio_file_read(a0, a1, a2);"
        }
        ("sun/nio/ch/FileDispatcherImpl", "size0", "(Ljava/io/FileDescriptor;)J") => {
            "return jabs_nio_file_size(a0);"
        }
        ("sun/nio/ch/FileDispatcherImpl", "seek0", "(Ljava/io/FileDescriptor;J)J") => {
            "return jabs_nio_file_seek(a0, a1);"
        }
        ("sun/nio/ch/UnixFileDispatcherImpl", "size0", "(Ljava/io/FileDescriptor;)J") => {
            "return jabs_nio_unix_size(a0);"
        }
        ("sun/nio/ch/UnixFileDispatcherImpl", "seek0", "(Ljava/io/FileDescriptor;J)J") => {
            "return jabs_nio_unix_seek(a0, a1);"
        }
        ("sun/nio/ch/FileDispatcherImpl", "allocationGranularity0", "()J")
        | ("sun/nio/ch/UnixFileDispatcherImpl", "allocationGranularity0", "()J") => {
            "return jabs_nio_allocation_granularity();"
        }
        ("sun/nio/ch/FileDispatcherImpl", "maxDirectTransferSize0", "()I") => {
            "return jabs_nio_max_direct_transfer_size();"
        }
        // Linux's init0 looks up copy_file_range and sendfile for transferTo/transferFrom, whose
        // natives JABS does not implement (they stop when called).
        ("sun/nio/ch/FileDispatcherImpl", "init0", "()V") => {
            "/* nothing: transferTo0/transferFrom0 are not implemented */"
        }
        (UNIX_DISPATCHER, "init", "()I") => "return jabs_nio_unix_init();",
        (UNIX_DISPATCHER, "getcwd", "()[B") => "return jabs_nio_getcwd();",
        (UNIX_DISPATCHER, "open0", "(JII)I") => "return jabs_nio_open(a0, a1, a2);",
        (UNIX_DISPATCHER, "stat0", "(JLsun/nio/fs/UnixFileAttributes;)I") => {
            "return jabs_nio_stat(a0, a1, 1);"
        }
        (UNIX_DISPATCHER, "lstat0", "(JLsun/nio/fs/UnixFileAttributes;)V") => {
            "jabs_nio_stat(a0, a1, 0);"
        }
        (UNIX_DISPATCHER, "unlink0", "(J)V") => "jabs_nio_unlink(a0);",
        (UNIX_DISPATCHER, "access0", "(JI)I") => "return jabs_nio_access(a0, a1);",
        (UNIX_DISPATCHER, "close0", "(I)V") => "jabs_nio_close(a0);",
        (UNIX_DISPATCHER, "dup", "(I)I") => "return jabs_nio_dup(a0);",
        (UNIX_DISPATCHER, "fdopendir", "(I)J") => "return jabs_nio_fdopendir(a0);",
        (UNIX_DISPATCHER, "closedir", "(J)V") => "jabs_nio_closedir(a0);",
        (UNIX_DISPATCHER, "readdir0", "(J)[B") => "return jabs_nio_readdir(a0);",
        (UNIX_DISPATCHER, "realpath0", "(J)[B") => "return jabs_nio_realpath(a0);",
        ("sun/nio/ch/UnixFileDispatcherImpl", "read0", "(Ljava/io/FileDescriptor;JI)I") => {
            "return jabs_nio_unix_read(a0, a1, a2);"
        }
        ("sun/nio/ch/UnixFileDispatcherImpl", "closeIntFD", "(I)V") => "jabs_nio_close_int_fd(a0);",
        // A performance counter (ZipFile counts its opens): HotSpot hands out a direct buffer over
        // the counter's 8 bytes of native memory, which hold the initial value. JABS keeps no
        // perf data file, so nothing outside the program ever reads them.
        (PERF, "createLong", "(Ljava/lang/String;IIJ)Ljava/nio/ByteBuffer;") => {
            "(void)self; (void)a1; (void)a2; return jabs_perf_create_long(a0, a3);"
        }
        // System.getenv's source (the Windows selector's Unix domain sockets ask it for TEMP).
        ("java/lang/ProcessEnvironment", "environmentBlock", "()Ljava/lang/String;") => {
            "return jabs_process_environment_block();"
        }
        ("java/lang/ProcessEnvironment", "environ", "()[[B") => "return jabs_process_environ();",
        _ => return None,
    })
}

/// C glue the file natives call: the OS-error exceptions (WindowsException(int),
/// UnixException(int)) and the fields of FirstFile and UnixFileAttributes they fill.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let field = |class: &str, name: &str, descriptor: &str| {
        records.contains(class)
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    let at = |class: &str, object: &str, name: &str| {
        format!(
            "(({} *){object})->{}",
            object_struct(class),
            instance_field(name)
        )
    };
    let mut out = String::from("\n/* ---- java.nio.file glue (src/cgen/net.rs) ---- */\n");
    // JNI's NewDirectByteBuffer, for Perf.createLong: DirectByteBuffer(long address, long capacity).
    let direct = "jabs_object *jabs_perf_create_long(jabs_object *name, int64_t value)";
    let direct_init = (
        DIRECT_BYTE_BUFFER.to_string(),
        "<init>".to_string(),
        "(JJ)V".to_string(),
    );
    if records.contains(DIRECT_BYTE_BUFFER)
        && inits.contains(DIRECT_BYTE_BUFFER)
        && methods.contains(&direct_init)
    {
        let _ = writeln!(
            out,
            "{direct} {{\n    if (!name) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}\n    int64_t *counter = (int64_t *)malloc(sizeof(int64_t));\n    if (!counter) {{ jabs_raise_message(JABS_VM_OUT_OF_MEMORY, \"Perf.createLong\"); return NULL; }}\n    *counter = value;\n    if ({init}() != 0) return NULL;\n    jabs_object *b = jabs_new(&{record});\n    if (!b) return NULL;\n    {call}(b, (int64_t)(intptr_t)counter, (int64_t)sizeof(int64_t));\n    return jabs_pending ? NULL : b;\n}}",
            init = init_symbol(DIRECT_BYTE_BUFFER),
            record = class_record(DIRECT_BYTE_BUFFER),
            call = symbol(DIRECT_BYTE_BUFFER, "<init>", "(JJ)V"),
        );
    } else {
        let _ = writeln!(
            out,
            "{direct} {{ (void)name; (void)value; jabs_not_compiled(\"Perf.createLong without DirectByteBuffer(long, long) in the program (src/cgen/nio_fs.rs)\"); }}"
        );
    }
    for (class, function) in [
        (WINDOWS_EXCEPTION, "jabs_nio_windows_exception"),
        (UNIX_EXCEPTION, "jabs_nio_unix_exception"),
    ] {
        let ctor = (class.to_string(), "<init>".to_string(), "(I)V".to_string());
        let signature = format!("void {function}(int32_t error)");
        if unit.get(class).is_some()
            && records.contains(class)
            && inits.contains(class)
            && methods.contains(&ctor)
        {
            let _ = writeln!(
                out,
                "{signature} {{\n    if ({init}() != 0) return;\n    jabs_object *e = jabs_new(&{record});\n    if (!e) return;\n    {call}(e, error);\n    if (!jabs_pending) jabs_pending = e;\n}}",
                init = init_symbol(class),
                record = class_record(class),
                call = symbol(class, "<init>", "(I)V"),
            );
        } else {
            let _ = writeln!(
                out,
                "{signature} {{ (void)error; jabs_not_compiled(\"{class}(int) is not in the program ({function}, src/cgen/net.rs)\"); }}"
            );
        }
    }
    let first = "void jabs_nio_set_first_file(jabs_object *first, int64_t handle, jabs_object *name, int32_t attributes)";
    if field(FIRST_FILE, "handle", "J")
        && field(FIRST_FILE, "name", "Ljava/lang/String;")
        && field(FIRST_FILE, "attributes", "I")
    {
        let _ = writeln!(
            out,
            "{first} {{ {} = handle; {} = name; {} = attributes; }}",
            at(FIRST_FILE, "first", "handle"),
            at(FIRST_FILE, "first", "name"),
            at(FIRST_FILE, "first", "attributes"),
        );
    } else {
        let _ = writeln!(
            out,
            "{first} {{ (void)first; (void)handle; (void)name; (void)attributes; jabs_not_compiled(\"WindowsNativeDispatcher$FirstFile without handle, name and attributes (src/cgen/net.rs)\"); }}"
        );
    }
    // UnixFileAttributes, field by field as the JDK's prepAttributes/copy_statx_attributes.
    const STAT_FIELDS: [(&str, &str, &str); 17] = [
        ("st_mode", "I", "mode"),
        ("st_ino", "J", "ino"),
        ("st_dev", "J", "dev"),
        ("st_rdev", "J", "rdev"),
        ("st_nlink", "I", "nlink"),
        ("st_uid", "I", "uid"),
        ("st_gid", "I", "gid"),
        ("st_size", "J", "size"),
        ("st_atime_sec", "J", "atime_sec"),
        ("st_atime_nsec", "J", "atime_nsec"),
        ("st_mtime_sec", "J", "mtime_sec"),
        ("st_mtime_nsec", "J", "mtime_nsec"),
        ("st_ctime_sec", "J", "ctime_sec"),
        ("st_ctime_nsec", "J", "ctime_nsec"),
        ("st_birthtime_sec", "J", "birthtime_sec"),
        ("st_birthtime_nsec", "J", "birthtime_nsec"),
        ("birthtime_available", "Z", "birthtime_available"),
    ];
    let unix =
        "void jabs_nio_set_unix_attributes(jabs_object *attributes, const jabs_nio_stat_result *s)";
    if STAT_FIELDS
        .iter()
        .all(|(name, descriptor, _)| field(UNIX_ATTRIBUTES, name, descriptor))
    {
        let mut body = String::new();
        for (name, descriptor, from) in STAT_FIELDS {
            let value = if descriptor == "Z" {
                format!("(uint8_t)(s->{from} != 0)")
            } else {
                format!("s->{from}")
            };
            let _ = write!(
                body,
                " {} = {value};",
                at(UNIX_ATTRIBUTES, "attributes", name)
            );
        }
        let _ = writeln!(out, "{unix} {{{body} }}");
    } else {
        let _ = writeln!(
            out,
            "{unix} {{ (void)attributes; (void)s; jabs_not_compiled(\"UnixFileAttributes without the stat fields (src/cgen/net.rs)\"); }}"
        );
    }
    out
}
