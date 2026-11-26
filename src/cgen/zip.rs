//! java.util.zip natives: Adler32 (inline in jabs_rt.h), plus Inflater and Deflater
//! (implemented in runtime/jabs_zip.c, with C-to-Java glue generated here for exceptions and
//! Inflater's consumed-byte fields).

use super::*;

const ADLER32: &str = "java/util/zip/Adler32";
const INFLATER: &str = "java/util/zip/Inflater";
const DEFLATER: &str = "java/util/zip/Deflater";
const DATA_FORMAT_EXCEPTION: &str = "java/util/zip/DataFormatException";
const ILLEGAL_ARGUMENT_EXCEPTION: &str = "java/lang/IllegalArgumentException";
const STRING_INIT: &str = "(Ljava/lang/String;)V";

/// Java methods a zip native calls from C: the exceptions Inflater and Deflater raise.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if (key.0 == INFLATER || key.0 == DEFLATER) && body(key).is_some() {
        vec![
            (DATA_FORMAT_EXCEPTION, "<init>", STRING_INIT),
            (ILLEGAL_ARGUMENT_EXCEPTION, "<init>", STRING_INIT),
        ]
    } else {
        Vec::new()
    }
}

/// The body of a zip native, if JABS implements it: (C body, needs the class registry).
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if key.0 == ADLER32 {
        return adler_body(key).map(|code| (code, false));
    }
    let code = body(key)?;
    classes_available.then_some((code, true))
}

enum Slice<'a> {
    Bytes {
        var: &'a str,
        arr: &'a str,
        off: &'a str,
        len: &'a str,
    },
    Buffer {
        addr: &'a str,
        len: &'a str,
    },
}

fn check_array_slice(
    var: &str,
    arr: &str,
    off: &str,
    len: &str,
    null_ret: &str,
    label: &str,
) -> String {
    format!(
        "if (!{arr}) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); {null_ret} }} \
         jabs_array *{var} = (jabs_array *){arr}; \
         if ({off} < 0 || {len} < 0 || {off} > {var}->length - {len}) \
         jabs_not_compiled(\"{label} outside the array (the JDK checks the range first)\");"
    )
}

fn slice_arg(slice: Slice<'_>, c_ptr: &str, label: &str) -> (String, String) {
    match slice {
        Slice::Bytes { var, arr, off, len } => (
            check_array_slice(var, arr, off, len, "return 0;", label),
            format!("({c_ptr})jabs_array_data({var}) + {off}"),
        ),
        Slice::Buffer { addr, len } => (
            format!("if ({len} < 0) jabs_not_compiled(\"{label} with a negative length\");"),
            // Avoid `NULL + 0` UB when a zero-length direct buffer passes address 0.
            format!("({len} ? ({c_ptr})(intptr_t){addr} : NULL)"),
        ),
    }
}

fn stream_body(
    prefix: &str,
    input: Slice<'_>,
    in_len: &str,
    output: Slice<'_>,
    out_len: &str,
    tail: &str,
    label: &str,
) -> String {
    let (in_check, in_ptr) = slice_arg(input, "const uint8_t *", label);
    let (out_check, out_ptr) = slice_arg(output, "uint8_t *", label);
    format!(
        "{in_check} {out_check} return {prefix}{in_ptr}, {in_len}, {out_ptr}, {out_len}{tail});"
    )
}

fn set_dict_bytes(func: &str, label: &str) -> String {
    let check = check_array_slice("b", "a1", "a2", "a3", "return;", label);
    format!("{check} {func}(a0, (const uint8_t *)jabs_array_data(b) + a2, a3);")
}

fn set_dict_buffer(func: &str, label: &str) -> String {
    format!(
        "if (a2 < 0) jabs_not_compiled(\"{label} with a negative length\"); \
         {func}(a0, (a2 ? (const uint8_t *)(intptr_t)a1 : NULL), a2);"
    )
}

fn adler_body(key: &MethodKey) -> Option<String> {
    Some(match (key.1.as_str(), key.2.as_str()) {
        ("update", "(II)I") => {
            "uint8_t b = (uint8_t)a1; return (int32_t)jabs_adler32((uint32_t)a0, &b, 1);".into()
        }
        ("updateBytes", "(I[BII)I") => format!(
            "{} return (int32_t)jabs_adler32((uint32_t)a0, (const uint8_t *)jabs_array_data(b) + a2, (size_t)a3);",
            check_array_slice("b", "a1", "a2", "a3", "return 0;", "Adler32.updateBytes")
        ),
        ("updateByteBuffer", "(IJII)I") => {
            "if (a2 < 0 || a3 < 0) jabs_not_compiled(\"Adler32.updateByteBuffer with a negative offset or length\"); \
             return (int32_t)jabs_adler32((uint32_t)a0, (a3 ? (const uint8_t *)(intptr_t)a1 + a2 : NULL), (size_t)a3);"
                .into()
        }
        _ => return None,
    })
}

fn body(key: &MethodKey) -> Option<String> {
    Some(match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (INFLATER, "initIDs", "()V") => "/* field offsets are known at compile time */".into(),
        (INFLATER, "init", "(Z)J") => "return jabs_zip_inflate_init(a0);".into(),
        (INFLATER, "setDictionary", "(J[BII)V") => {
            set_dict_bytes("jabs_zip_inflate_set_dict", "Inflater.setDictionary")
        }
        (INFLATER, "setDictionaryBuffer", "(JJI)V") => {
            set_dict_buffer("jabs_zip_inflate_set_dict", "Inflater.setDictionaryBuffer")
        }
        (INFLATER, "inflateBytesBytes", "(J[BII[BII)J") => stream_body(
            "jabs_zip_inflate(self, a0, ",
            Slice::Bytes {
                var: "in_b",
                arr: "a1",
                off: "a2",
                len: "a3",
            },
            "a3",
            Slice::Bytes {
                var: "out_b",
                arr: "a4",
                off: "a5",
                len: "a6",
            },
            "a6",
            "",
            "Inflater.inflateBytesBytes",
        ),
        (INFLATER, "inflateBytesBuffer", "(J[BIIJI)J") => stream_body(
            "jabs_zip_inflate(self, a0, ",
            Slice::Bytes {
                var: "in_b",
                arr: "a1",
                off: "a2",
                len: "a3",
            },
            "a3",
            Slice::Buffer {
                addr: "a4",
                len: "a5",
            },
            "a5",
            "",
            "Inflater.inflateBytesBuffer",
        ),
        (INFLATER, "inflateBufferBytes", "(JJI[BII)J") => stream_body(
            "jabs_zip_inflate(self, a0, ",
            Slice::Buffer {
                addr: "a1",
                len: "a2",
            },
            "a2",
            Slice::Bytes {
                var: "out_b",
                arr: "a3",
                off: "a4",
                len: "a5",
            },
            "a5",
            "",
            "Inflater.inflateBufferBytes",
        ),
        (INFLATER, "inflateBufferBuffer", "(JJIJI)J") => stream_body(
            "jabs_zip_inflate(self, a0, ",
            Slice::Buffer {
                addr: "a1",
                len: "a2",
            },
            "a2",
            Slice::Buffer {
                addr: "a3",
                len: "a4",
            },
            "a4",
            "",
            "Inflater.inflateBufferBuffer",
        ),
        (INFLATER, "getAdler", "(J)I") => "return jabs_zip_inflate_adler(a0);".into(),
        (INFLATER, "reset", "(J)V") => "jabs_zip_inflate_reset(a0);".into(),
        (INFLATER, "end", "(J)V") => "jabs_zip_inflate_end(a0);".into(),

        (DEFLATER, "init", "(IIZ)J") => "return jabs_zip_deflate_init(a0, a1, a2);".into(),
        (DEFLATER, "setDictionary", "(J[BII)V") => {
            set_dict_bytes("jabs_zip_deflate_set_dict", "Deflater.setDictionary")
        }
        (DEFLATER, "setDictionaryBuffer", "(JJI)V") => {
            set_dict_buffer("jabs_zip_deflate_set_dict", "Deflater.setDictionaryBuffer")
        }
        (DEFLATER, "deflateBytesBytes", "(J[BII[BIIII)J") => stream_body(
            "jabs_zip_deflate(a0, ",
            Slice::Bytes {
                var: "in_b",
                arr: "a1",
                off: "a2",
                len: "a3",
            },
            "a3",
            Slice::Bytes {
                var: "out_b",
                arr: "a4",
                off: "a5",
                len: "a6",
            },
            "a6",
            ", a7, a8",
            "Deflater.deflateBytesBytes",
        ),
        (DEFLATER, "deflateBytesBuffer", "(J[BIIJIII)J") => stream_body(
            "jabs_zip_deflate(a0, ",
            Slice::Bytes {
                var: "in_b",
                arr: "a1",
                off: "a2",
                len: "a3",
            },
            "a3",
            Slice::Buffer {
                addr: "a4",
                len: "a5",
            },
            "a5",
            ", a6, a7",
            "Deflater.deflateBytesBuffer",
        ),
        (DEFLATER, "deflateBufferBytes", "(JJI[BIIII)J") => stream_body(
            "jabs_zip_deflate(a0, ",
            Slice::Buffer {
                addr: "a1",
                len: "a2",
            },
            "a2",
            Slice::Bytes {
                var: "out_b",
                arr: "a3",
                off: "a4",
                len: "a5",
            },
            "a5",
            ", a6, a7",
            "Deflater.deflateBufferBytes",
        ),
        (DEFLATER, "deflateBufferBuffer", "(JJIJIII)J") => stream_body(
            "jabs_zip_deflate(a0, ",
            Slice::Buffer {
                addr: "a1",
                len: "a2",
            },
            "a2",
            Slice::Buffer {
                addr: "a3",
                len: "a4",
            },
            "a4",
            ", a5, a6",
            "Deflater.deflateBufferBuffer",
        ),
        (DEFLATER, "getAdler", "(J)I") => "return jabs_zip_deflate_adler(a0);".into(),
        (DEFLATER, "reset", "(J)V") => "jabs_zip_deflate_reset(a0);".into(),
        (DEFLATER, "end", "(J)V") => "jabs_zip_deflate_end(a0);".into(),
        _ => return None,
    })
}

/// C glue the zip natives call: DataFormatException(String), IllegalArgumentException(String),
/// and updating Inflater.inputConsumed / outputConsumed.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let mut out = String::from("\n/* ---- java.util.zip glue (src/cgen/zip.rs) ---- */\n");
    for (class, function) in [
        (DATA_FORMAT_EXCEPTION, "jabs_zip_data_format_exception"),
        (ILLEGAL_ARGUMENT_EXCEPTION, "jabs_zip_illegal_argument"),
    ] {
        let ctor = (
            class.to_string(),
            "<init>".to_string(),
            STRING_INIT.to_string(),
        );
        let signature = format!("void {function}(const char *message)");
        if unit.get(class).is_some()
            && records.contains(class)
            && inits.contains(class)
            && methods.contains(&ctor)
        {
            let _ = writeln!(
                out,
                "{signature} {{\n    jabs_object *msg = NULL;\n    if (message) {{ msg = jabs_string_from_utf8(message); if (!msg) return; }}\n    if ({init}() != 0) return;\n    jabs_object *e = jabs_new(&{record});\n    if (!e) return;\n    {call}(e, msg);\n    if (!jabs_pending) jabs_pending = e;\n}}",
                init = init_symbol(class),
                record = class_record(class),
                call = symbol(class, "<init>", STRING_INIT),
            );
        } else {
            let _ = writeln!(
                out,
                "{signature} {{ (void)message; jabs_not_compiled(\"{class}(String) is not in the program ({function}, src/cgen/zip.rs)\"); }}"
            );
        }
    }
    let has_field = |name: &str| {
        records.contains(INFLATER)
            && unit.get(INFLATER).is_some_and(|c| {
                c.fields
                    .iter()
                    .any(|f| f.name == name && f.descriptor == "I" && f.access & ACC_STATIC == 0)
            })
    };
    let consumed = "void jabs_zip_set_inflater_consumed(jabs_object *inflater, int32_t in_consumed, int32_t out_consumed)";
    if has_field("inputConsumed") && has_field("outputConsumed") {
        let struct_name = object_struct(INFLATER);
        let _ = writeln!(
            out,
            "{consumed} {{\n    (({struct_name} *)inflater)->{in_f} = in_consumed;\n    (({struct_name} *)inflater)->{out_f} = out_consumed;\n}}",
            in_f = instance_field("inputConsumed"),
            out_f = instance_field("outputConsumed"),
        );
    } else {
        let _ = writeln!(
            out,
            "{consumed} {{ (void)inflater; (void)in_consumed; (void)out_consumed; jabs_not_compiled(\"Inflater without inputConsumed and outputConsumed (src/cgen/zip.rs)\"); }}"
        );
    }
    out
}
