//! C emission for a compilation unit: a closed set of classes compiled together.
//!
//! A method compiles only when every instruction is understood, a bytecode
//! verifier proves every path type-correct against the unit's class hierarchy
//! (references become C pointers, so this is a memory-safety requirement), and
//! everything it depends on also compiles: the methods it may call (for a
//! virtual call, every implementation it can dispatch to) and the classes it
//! may initialize. Classes outside the unit are opaque: their references can be
//! passed around, but nothing is called or read through them. Every method
//! that does not compile is reported with a reason; nothing is guessed.
//!
//! Objects have a class-record header; a subclass's struct embeds its
//! superclass's struct first, and vtables follow Java's override rules. There
//! is no garbage collector yet: objects live until the process exits.

use crate::bytecode::{
    self, BinOp, Compare, Cond, Convert, DecodeError, Insn, Kind, MemberRef, Op, Value,
};
use crate::classfile::{
    ACC_ABSTRACT, ACC_FINAL, ACC_INTERFACE, ACC_NATIVE, ACC_PRIVATE, ACC_STATIC, ACC_SYNCHRONIZED,
    ClassFile, Constant, Member,
};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write,
    rc::Rc,
};

pub const RUNTIME_PRELUDE: &str = include_str!("../runtime/jabs_rt.h");
const OBJECT: &str = "java/lang/Object";
const STRING: &str = "java/lang/String";
const THROWABLE: &str = "java/lang/Throwable";
const CLASS: &str = "java/lang/Class";

/// Exceptions the runtime raises itself, in the order of the JABS_VM_* constants.
const VM_EXCEPTIONS: [(&str, &str); 13] = [
    ("java/lang/NullPointerException", "JABS_VM_NULL_POINTER"),
    ("java/lang/ArithmeticException", "JABS_VM_ARITHMETIC"),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "JABS_VM_ARRAY_INDEX",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "JABS_VM_NEGATIVE_ARRAY_SIZE",
    ),
    ("java/lang/ArrayStoreException", "JABS_VM_ARRAY_STORE"),
    ("java/lang/ClassCastException", "JABS_VM_CLASS_CAST"),
    (
        "java/lang/IncompatibleClassChangeError",
        "JABS_VM_INCOMPATIBLE_CLASS_CHANGE",
    ),
    ("java/lang/AbstractMethodError", "JABS_VM_ABSTRACT_METHOD"),
    (
        "java/lang/NoClassDefFoundError",
        "JABS_VM_NO_CLASS_DEF_FOUND",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "JABS_VM_INITIALIZER",
    ),
    ("java/lang/OutOfMemoryError", "JABS_VM_OUT_OF_MEMORY"),
    (
        "java/lang/IllegalMonitorStateException",
        "JABS_VM_ILLEGAL_MONITOR_STATE",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "JABS_VM_CLONE_NOT_SUPPORTED",
    ),
];

/// Native methods the runtime implements: (class, name, descriptor) -> C body. `self` and
/// `a0`.. are the receiver and arguments. Bodies marked `needs_classes` use the Class registry.
struct Intrinsic {
    key: (&'static str, &'static str, &'static str),
    body: &'static str,
    needs_classes: bool,
}

const INTRINSICS: &[Intrinsic] = &[
    Intrinsic {
        key: ("java/lang/Object", "hashCode", "()I"),
        body: "return jabs_identity_hash(self);",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
        ),
        body: "return a0 ? jabs_identity_hash(a0) : 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
        body: "return jabs_class_of(self);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
        body: "return jabs_clone(self);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "notify", "()V"),
        body: "if (self->monitor == 0) jabs_pending = jabs_vm_throwable(JABS_VM_ILLEGAL_MONITOR_STATE);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "notifyAll", "()V"),
        body: "if (self->monitor == 0) jabs_pending = jabs_vm_throwable(JABS_VM_ILLEGAL_MONITOR_STATE);",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        ),
        body: "jabs_arraycopy(a0, a1, a2, a3, a4);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/System", "currentTimeMillis", "()J"),
        body: "return jabs_current_millis();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/System", "nanoTime", "()J"),
        body: "return jabs_nano_time();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Double", "doubleToRawLongBits", "(D)J"),
        body: "int64_t r; memcpy(&r, &a0, 8); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Double", "longBitsToDouble", "(J)D"),
        body: "double r; memcpy(&r, &a0, 8); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Float", "floatToRawIntBits", "(F)I"),
        body: "int32_t r; memcpy(&r, &a0, 4); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Float", "intBitsToFloat", "(I)F"),
        body: "float r; memcpy(&r, &a0, 4); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Runtime", "availableProcessors", "()I"),
        body: "return 1; /* the native runtime is single-threaded */",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Throwable",
            "fillInStackTrace",
            "(I)Ljava/lang/Throwable;",
        ),
        body: "return self; /* stack traces are not captured yet */",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "desiredAssertionStatus0",
            "(Ljava/lang/Class;)Z",
        ),
        body: "return 0; /* assertions disabled, as without -ea */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Class", "isHidden", "()Z"),
        body: "return 0;",
        needs_classes: false,
    },
    // The VM creates the main thread before any Java code runs; JABS does it on first request.
    Intrinsic {
        key: ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
        body: "return jabs_main_thread();",
        needs_classes: true,
    },
    // The registry sets `name` when it creates a Class, so the lazy initializer just returns it.
    Intrinsic {
        key: ("java/lang/Class", "initClassName", "()Ljava/lang/String;"),
        body: "return ((struct jo_java_2flang_2fClass *)self)->f_name;",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Class", "isInstance", "(Ljava/lang/Object;)Z"),
        body: "return a0 ? jabs_class_is_instance(self, a0) : 0;",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "isAssignableFrom",
            "(Ljava/lang/Class;)Z",
        ),
        body: "if (!a0) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_class_assignable_from(self, a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Class", "getSuperclass", "()Ljava/lang/Class;"),
        body: "return jabs_class_superclass(self);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "getPrimitiveClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        ),
        body: "char name[32]; if (!a0 || !jabs_string_to_ascii(a0, name, sizeof name)) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_class_object(name);",
        needs_classes: true,
    },
];

/// The runtime's C body for a native method, if it implements one (and the unit can support it).
fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if key.1 == "registerNatives" && key.2 == "()V" {
        return Some(("/* natives are linked statically */".to_string(), false));
    }
    if key.0 == UNSAFE
        && let Some((body, needs_classes)) = unsafe_intrinsic(&key.1, &key.2)
    {
        return (classes_available || !needs_classes).then_some((body, needs_classes));
    }
    INTRINSICS
        .iter()
        .find(|i| i.key.0 == key.0 && i.key.1 == key.1 && i.key.2 == key.2)
        .filter(|i| classes_available || !i.needs_classes)
        .map(|i| (i.body.to_string(), i.needs_classes))
}

const UNSAFE: &str = "jdk/internal/misc/Unsafe";

/// Bodies for jdk.internal.misc.Unsafe natives. Offsets are byte offsets from the object pointer
/// (our struct layouts and array data start), or raw addresses when the object is null, as in
/// HotSpot. With a single thread, volatile accesses, fences and compare-and-set are plain.
fn unsafe_intrinsic(name: &str, descriptor: &str) -> Option<(String, bool)> {
    const TYPES: [(&str, &str, &str); 9] = [
        ("Int", "I", "int32_t"),
        ("Long", "J", "int64_t"),
        ("Reference", "Ljava/lang/Object;", "jabs_object *"),
        ("Boolean", "Z", "uint8_t"),
        ("Byte", "B", "int8_t"),
        ("Short", "S", "int16_t"),
        ("Char", "C", "uint16_t"),
        ("Float", "F", "float"),
        ("Double", "D", "double"),
    ];
    let at = |ty: &str| format!("(*({ty} *)((char *)a0 + a1))");
    for (word, code, ty) in TYPES {
        for suffix in ["", "Volatile"] {
            if name == format!("get{word}{suffix}")
                && descriptor == format!("(Ljava/lang/Object;J){code}")
            {
                return Some((format!("return {};", at(ty)), false));
            }
            if name == format!("put{word}{suffix}")
                && descriptor == format!("(Ljava/lang/Object;J{code})V")
            {
                let value = if code == "Z" {
                    "(uint8_t)(a2 & 1)".to_string()
                } else {
                    format!("({ty})a2")
                };
                return Some((format!("{} = {value};", at(ty)), false));
            }
        }
        if matches!(word, "Int" | "Long" | "Reference") {
            if name == format!("compareAndSet{word}")
                && descriptor == format!("(Ljava/lang/Object;J{code}{code})Z")
            {
                return Some((
                    format!(
                        "{ty} *p = &{}; if (*p != a2) return 0; *p = a3; return 1;",
                        at(ty)
                    ),
                    false,
                ));
            }
            if name == format!("compareAndExchange{word}")
                && descriptor == format!("(Ljava/lang/Object;J{code}{code}){code}")
            {
                return Some((
                    format!(
                        "{ty} *p = &{}; {ty} old = *p; if (old == a2) *p = a3; return old;",
                        at(ty)
                    ),
                    false,
                ));
            }
        }
    }
    let body = match (name, descriptor) {
        ("fullFence" | "loadFence" | "storeFence", "()V") => {
            "/* single-threaded: program order is enough */"
        }
        ("throwException", "(Ljava/lang/Throwable;)V") => {
            "jabs_pending = a0 ? a0 : jabs_vm_throwable(JABS_VM_NULL_POINTER);"
        }
        ("allocateMemory0", "(J)J") => "return (int64_t)(intptr_t)malloc((size_t)a0);",
        ("reallocateMemory0", "(JJ)J") => {
            "return (int64_t)(intptr_t)realloc((void *)(intptr_t)a0, (size_t)a1);"
        }
        ("freeMemory0", "(J)V") => "free((void *)(intptr_t)a0);",
        ("setMemory0", "(Ljava/lang/Object;JJB)V") => {
            "memset((char *)a0 + a1, (unsigned char)a3, (size_t)a2);"
        }
        ("copyMemory0", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V") => {
            "memmove((char *)a2 + a3, (char *)a0 + a1, (size_t)a4);"
        }
        ("arrayBaseOffset0", "(Ljava/lang/Class;)I") => {
            "return (int32_t)((sizeof(jabs_array) + 7) & ~(size_t)7);"
        }
        ("arrayIndexScale0", "(Ljava/lang/Class;)I") => {
            return Some(("return jabs_array_scale(a0);".into(), true));
        }
        ("objectFieldOffset1", "(Ljava/lang/Class;Ljava/lang/String;)J") => {
            return Some(("return jabs_field_offset(a0, a1);".into(), true));
        }
        _ => return None,
    };
    Some((body.to_string(), false))
}

/// Whether the unit has what Class objects need: java.lang.Class and String with the expected fields.
fn classes_available(unit: &Unit) -> bool {
    let has = |class: &str, field: &str, descriptor: &str| {
        unit.get(class).is_some_and(|c| {
            c.fields.iter().any(|f| {
                f.name == field && f.descriptor == descriptor && f.access & ACC_STATIC == 0
            })
        })
    };
    unit.chain(CLASS).is_ok()
        && unit.chain(STRING).is_ok()
        && has(CLASS, "name", "Ljava/lang/String;")
        && has(CLASS, "componentType", "Ljava/lang/Class;")
        && has(CLASS, "modifiers", "C")
        && has(CLASS, "primitive", "Z")
        && STRING_FIELDS.iter().all(|(n, d)| has(STRING, n, d))
}

/// `jabs_pending = <new VM exception>` for one of `VM_EXCEPTIONS`.
fn raise(class: &str) -> String {
    let kind = VM_EXCEPTIONS
        .iter()
        .find(|(c, _)| *c == class)
        .unwrap_or_else(|| panic!("{class} is not a VM exception"))
        .1;
    format!("jabs_pending = jabs_vm_throwable({kind});")
}

/// Fields the JDK 9+ String layout must have for compiled literals: value, coder, hash, hashIsZero.
const STRING_FIELDS: [(&str, &str); 4] = [
    ("value", "[B"),
    ("coder", "B"),
    ("hash", "I"),
    ("hashIsZero", "Z"),
];

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MethodStatus {
    /// The body is native. `self_contained`: nothing reachable from it hits a not-compiled stub.
    Compiled {
        symbol: String,
        self_contained: bool,
    },
    Unsupported {
        reason: String,
    },
    /// Abstract: no code of its own; calls dispatch to implementations.
    Abstract,
}

#[derive(Debug, Serialize)]
pub struct MethodReport {
    pub class: String,
    pub name: String,
    pub descriptor: String,
    #[serde(flatten)]
    pub status: MethodStatus,
}

#[derive(Debug, Serialize)]
pub struct CompiledUnit {
    pub schema_version: u32,
    pub classes: usize,
    pub compiled: usize,
    pub unsupported: usize,
    /// Abstract methods (counted in neither `compiled` nor `unsupported`).
    pub abstract_methods: usize,
    /// Compiled methods whose whole reachable call graph and class initialization is native.
    pub self_contained: usize,
    pub methods: Vec<MethodReport>,
    #[serde(skip)]
    pub c_source: String,
    /// The same program split for parallel compilation: (file name, contents). The first is the
    /// shared header every other file includes.
    #[serde(skip)]
    pub c_files: Vec<(String, String)>,
}

/// Definitions of the runtime's globals; exactly one file of a program contains them.
const RUNTIME_GLOBALS: &str = "_Thread_local jabs_object *jabs_pending;\n\
const jabs_class jabs_array_class_record = { \"[\", 0, NULL, NULL, NULL, 0, NULL, 0, 0, 0 };\n\
uint32_t jabs_hash_next = 0x9e3779b9u;\n\
const char *jabs_where = \"\";\n\n";

/// Extern declarations for the external definitions in `code` (records, vtables, static fields
/// and non-static functions), so other files can reference them.
fn declarations_for(code: &str) -> String {
    let mut out = String::new();
    for line in code.lines() {
        if line.starts_with("static")
            || line.starts_with(' ')
            || line.starts_with('#')
            || line.starts_with("typedef")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("const jabs_class ") {
            if let Some(name) = rest.split(' ').next() {
                let _ = writeln!(out, "extern const jabs_class {name};");
            }
        } else if let Some(rest) = line.strip_prefix("void (*const ") {
            if let Some(name) = rest.split('[').next() {
                let _ = writeln!(out, "extern void (*const {name}[])(void);");
            }
        } else if line.ends_with(';') && line.contains(" jf_") && !line.contains('(') {
            let _ = writeln!(out, "extern {line}");
        } else if let (Some(open), Some(brace)) = (line.find('('), line.find(") {")) {
            // A function definition on one line ("T name(params) { ... }") or opening a body.
            if open < brace && !line[..open].contains('=') {
                let _ = writeln!(out, "{};", &line[..brace + 1]);
            }
        }
    }
    out
}

/// Split method definitions into roughly equal groups for parallel compilation.
fn method_chunks(methods: &[String], files: usize) -> Vec<String> {
    let total: usize = methods.iter().map(String::len).sum();
    let target = (total / files.max(1)).max(1);
    let mut chunks = vec![String::new()];
    for m in methods {
        if chunks.last().unwrap().len() >= target {
            chunks.push(String::new());
        }
        chunks.last_mut().unwrap().push_str(m);
    }
    chunks
}

/// One type from a descriptor: `code` is I J F D Z B C S, `L` (with `class`) or `V` for void.
#[derive(Debug, Clone, PartialEq)]
pub struct Ty {
    pub code: char,
    pub class: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// Stack kind for a type code; boolean/byte/char/short all live as int.
pub fn kind_of(c: char) -> Option<Kind> {
    match c {
        'I' | 'Z' | 'B' | 'C' | 'S' => Some(Kind::I),
        'J' => Some(Kind::J),
        'F' => Some(Kind::F),
        'D' => Some(Kind::D),
        'L' => Some(Kind::A),
        _ => None,
    }
}

/// Parse one field or parameter descriptor (arrays are not compiled yet).
fn parse_type(descriptor: &str, context: &str) -> Result<Ty> {
    match descriptor.as_bytes() {
        [c] if b"IJFDZBCS".contains(c) => Ok(Ty {
            code: *c as char,
            class: None,
        }),
        [b'L', .., b';'] => Ok(Ty {
            code: 'L',
            class: Some(descriptor[1..descriptor.len() - 1].to_string()),
        }),
        [b'[', ..] => {
            parse_type(&descriptor[1..], context)?;
            Ok(Ty {
                code: 'L',
                class: Some(descriptor.to_string()),
            })
        }
        _ => bail!("invalid descriptor {descriptor} in {context}"),
    }
}

/// Escape a class or descriptor name for a C string literal (names may hold any Unicode).
fn c_escape(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out
}

/// C element type of an array, from its descriptor char (`L` or `[` for references).
fn element_type(element: char) -> &'static str {
    match element {
        'Z' => "uint8_t",
        'B' => "int8_t",
        'C' => "uint16_t",
        'S' => "int16_t",
        'I' => "int32_t",
        'J' => "int64_t",
        'F' => "float",
        'D' => "double",
        _ => "jabs_object *",
    }
}

/// Type name of an array element as the verifier interns it: a class name, an array
/// descriptor, or None for primitives.
fn element_class(array: &str) -> Option<String> {
    let element = &array[1..];
    match element.as_bytes().first() {
        Some(b'L') => Some(element[1..element.len() - 1].to_string()),
        Some(b'[') => Some(element.to_string()),
        _ => None,
    }
}

/// Split the parameter list of a method descriptor into single-type descriptors.
fn split_params(params: &str) -> Result<Vec<&str>> {
    let mut out = Vec::new();
    let mut rest = params;
    while !rest.is_empty() {
        let mut end = rest.bytes().take_while(|&b| b == b'[').count();
        end += match rest.as_bytes().get(end) {
            Some(b'L') => rest[end..]
                .find(';')
                .map(|i| i + 1)
                .unwrap_or(rest.len() + 1),
            Some(_) => 1,
            None => rest.len() + 1,
        };
        if end > rest.len() {
            bail!("invalid method descriptor parameters {params}");
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    Ok(out)
}

/// Parse a method descriptor.
pub fn parse_signature(descriptor: &str) -> Result<Signature> {
    let Some((params, ret)) = descriptor
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
    else {
        bail!("invalid method descriptor {descriptor}")
    };
    let params = split_params(params)?
        .into_iter()
        .map(|p| parse_type(p, descriptor))
        .collect::<Result<Vec<_>>>()?;
    let ret = if ret == "V" {
        Ty {
            code: 'V',
            class: None,
        }
    } else {
        parse_type(ret, descriptor)?
    };
    Ok(Signature { params, ret })
}

/// C type for a type code in generated prototypes (`void` for V).
pub fn c_type(c: char) -> &'static str {
    match kind_of(c) {
        Some(Kind::I) => "int32_t",
        Some(Kind::J) => "int64_t",
        Some(Kind::F) => "float",
        Some(Kind::D) => "double",
        Some(Kind::A) => "jabs_object *",
        None => "void",
    }
}

/// Member of `jabs_value` that holds a kind.
fn field(kind: Kind) -> &'static str {
    match kind {
        Kind::I => "i",
        Kind::J => "j",
        Kind::F => "f",
        Kind::D => "d",
        Kind::A => "a",
    }
}

/// Append `text` as a C identifier: alphanumerics kept, every other byte as `_xx` hex.
fn mangle(text: &str, out: &mut String) {
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            let _ = write!(out, "_{b:02x}");
        }
    }
}

/// `prefix` + mangled parts joined by `__`.
fn mangled(prefix: &str, parts: &[&str]) -> String {
    let mut out = String::from(prefix);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str("__");
        }
        mangle(part, &mut out);
    }
    out
}

/// Stable C symbol for a Java method identity (class, name, descriptor).
pub fn symbol(class: &str, name: &str, descriptor: &str) -> String {
    mangled("jm_", &[class, name, descriptor])
}

/// C symbol for a static field's storage.
pub fn field_symbol(class: &str, name: &str) -> String {
    mangled("jf_", &[class, name])
}

/// C symbol of a class's initialization entry point; returns 0 when the class is ready, -1 with `jabs_pending` set.
pub fn init_symbol(class: &str) -> String {
    let mut out = mangled("jc_", &[class]);
    out.push_str("__init");
    out
}

/// C struct tag for a class's object layout.
fn object_struct(class: &str) -> String {
    format!("struct {}", mangled("jo_", &[class]))
}

/// C name of a class's runtime record.
fn class_record(class: &str) -> String {
    mangled("jk_", &[class])
}

/// C name of an instance field inside its declaring class's struct.
fn instance_field(name: &str) -> String {
    mangled("f_", &[name])
}

/// C storage type for a field, sized like the JVM's (so byte fields really hold a byte).
fn storage_type(descriptor: &str) -> Option<&'static str> {
    Some(match descriptor.as_bytes() {
        b"Z" => "uint8_t",
        b"B" => "int8_t",
        b"C" => "uint16_t",
        b"S" => "int16_t",
        b"I" => "int32_t",
        b"J" => "int64_t",
        b"F" => "float",
        b"D" => "double",
        [b'L', .., b';'] | [b'[', ..] => "jabs_object *",
        _ => return None,
    })
}

/// Expression converting a stack value into a field's storage type (put narrowing; boolean keeps bit 0).
fn narrow_for_store(descriptor: &str, value: &str) -> String {
    match descriptor {
        "Z" => format!("(uint8_t)({value} & 1)"),
        "B" => format!("(int8_t)jabs_i2b({value})"),
        "C" => format!("(uint16_t)jabs_i2c({value})"),
        "S" => format!("(int16_t)jabs_i2s({value})"),
        _ => value.to_string(),
    }
}

/// Runtime package of a class: its internal name up to the last `/`.
fn package(class: &str) -> &str {
    class.rfind('/').map_or("", |i| &class[..i])
}

/// A method identity: (class, name, descriptor).
pub type MethodKey = (String, String, String);

/// Stable 64-bit ID of an interface method signature; independent of which classes exist.
pub fn selector(name: &str, descriptor: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{name}\0{descriptor}").as_bytes());
    u64::from_be_bytes(digest[..8].try_into().unwrap())
}

/// What an interface call selects on instances of a class (JVMS 5.4.6).
#[derive(Debug, Clone)]
enum ItableTarget {
    Impl(MethodKey),
    /// No implementation: AbstractMethodError.
    Abstract,
    /// Several equally specific default methods: IncompatibleClassChangeError.
    Ambiguous,
}

#[derive(Debug, Clone)]
struct ItableEntry {
    selector: u64,
    name: String,
    descriptor: String,
    target: ItableTarget,
}

/// One vtable slot: the method's name and descriptor, and which class's implementation fills it.
#[derive(Debug, Clone)]
struct Slot {
    name: String,
    descriptor: String,
    owner: String,
}

/// A memoized superclass chain, or why it could not be built.
type ChainResult<'a> = std::result::Result<Rc<Vec<&'a ClassFile>>, String>;

/// The classes compiled together, with hierarchy queries over them.
struct Unit<'a> {
    classes: HashMap<&'a str, &'a ClassFile>,
    /// Class-name interning so verification types stay `Copy`.
    names: RefCell<(Vec<String>, HashMap<String, u32>)>,
    vtables: RefCell<HashMap<String, Rc<Vec<Slot>>>>,
    chains: RefCell<HashMap<String, ChainResult<'a>>>,
    itables: RefCell<HashMap<String, Rc<Vec<ItableEntry>>>>,
}

impl<'a> Unit<'a> {
    fn new(classes: &'a [ClassFile]) -> Self {
        Unit {
            classes: classes.iter().map(|c| (c.name.as_str(), c)).collect(),
            names: RefCell::new((Vec::new(), HashMap::new())),
            vtables: RefCell::new(HashMap::new()),
            chains: RefCell::new(HashMap::new()),
            itables: RefCell::new(HashMap::new()),
        }
    }
    fn get(&self, name: &str) -> Option<&'a ClassFile> {
        self.classes.get(name).copied()
    }
    fn intern(&self, name: &str) -> u32 {
        let mut names = self.names.borrow_mut();
        if let Some(&id) = names.1.get(name) {
            return id;
        }
        let id = names.0.len() as u32;
        names.0.push(name.to_string());
        names.1.insert(name.to_string(), id);
        id
    }
    fn name(&self, id: u32) -> String {
        self.names.borrow().0[id as usize].clone()
    }
    /// The class and its superclasses up to (not including) java/lang/Object; fails if a link is outside the unit.
    fn chain(&self, name: &str) -> Result<Rc<Vec<&'a ClassFile>>> {
        if let Some(known) = self.chains.borrow().get(name) {
            return known.clone().map_err(|e| anyhow!(e));
        }
        let computed = self
            .compute_chain(name)
            .map(Rc::new)
            .map_err(|e| e.to_string());
        self.chains
            .borrow_mut()
            .insert(name.to_string(), computed.clone());
        computed.map_err(|e| anyhow!(e))
    }
    /// Walk superclass links without memoization.
    fn compute_chain(&self, name: &str) -> Result<Vec<&'a ClassFile>> {
        let mut out = Vec::new();
        let mut current = name.to_string();
        while current != OBJECT {
            let Some(class) = self.get(&current) else {
                bail!("{current} is not in the compilation unit");
            };
            if out.len() > 10_000 {
                bail!("superclass cycle at {current}");
            }
            out.push(class);
            current = class
                .super_name
                .clone()
                .unwrap_or_else(|| OBJECT.to_string());
        }
        Ok(out)
    }
    /// Whether `sub` is `sup` or one of its subclasses; None if the hierarchy is not known.
    fn is_subclass(&self, sub: &str, sup: &str) -> Option<bool> {
        if sub == sup || sup == OBJECT {
            return Some(true);
        }
        Some(self.chain(sub).ok()?.iter().any(|c| c.name == sup))
    }
    fn is_interface(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|c| c.access & ACC_INTERFACE != 0)
    }
    /// All superinterfaces of a class, direct and indirect, in declaration order without repeats.
    fn superinterfaces(&self, name: &str) -> Result<Vec<&'a ClassFile>> {
        let mut out: Vec<&ClassFile> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        for class in self.chain(name)?.iter() {
            pending.extend(class.interfaces.iter().cloned());
        }
        let mut i = 0;
        while i < pending.len() {
            let Some(iface) = self.get(&pending[i]) else {
                bail!("interface {} is not in the compilation unit", pending[i]);
            };
            if !out.iter().any(|c| c.name == iface.name) {
                out.push(iface);
                pending.extend(iface.interfaces.iter().cloned());
            }
            i += 1;
        }
        Ok(out)
    }
    /// JVMS 5.4.3.3: find a method in the class or its superclasses.
    fn resolve_method(
        &self,
        owner: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        for &class in self
            .chain(owner)
            .map_err(|e| anyhow!("call to {owner}.{name}: {e}"))?
            .iter()
        {
            if let Some(m) = class
                .methods
                .iter()
                .find(|m| m.name == name && m.descriptor == descriptor)
            {
                return Ok((class, m));
            }
        }
        if let Some(object) = self.get(OBJECT)
            && let Some(m) = object
                .methods
                .iter()
                .find(|m| m.name == name && m.descriptor == descriptor)
        {
            return Ok((object, m));
        }
        bail!(
            "{owner}.{name}{descriptor} is inherited from outside the unit (e.g. java/lang/Object)"
        )
    }
    /// JVMS 5.4.3.2: find a field in the class, its superinterfaces, then its superclasses.
    fn resolve_field(
        &self,
        owner: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        let find = |c: &'a ClassFile| {
            c.fields
                .iter()
                .find(|f| f.name == name && f.descriptor == descriptor)
        };
        let chain = self
            .chain(owner)
            .map_err(|e| anyhow!("field {owner}.{name}: {e}"))?;
        for &class in chain.iter() {
            if let Some(f) = find(class) {
                return Ok((class, f));
            }
            for iface in self.superinterfaces(&class.name)? {
                if let Some(f) = find(iface) {
                    return Ok((iface, f));
                }
            }
        }
        bail!("field {owner}.{name} is not declared in the unit")
    }
    /// JVMS 5.4.3.4: find an interface method in the interface or its superinterfaces.
    fn resolve_interface_method(
        &self,
        iface: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        let first = self
            .get(iface)
            .ok_or_else(|| anyhow!("{iface} is not in the compilation unit"))?;
        for class in std::iter::once(first).chain(self.superinterfaces(iface)?) {
            if let Some(m) = class.methods.iter().find(|m| {
                m.name == name && m.descriptor == descriptor && m.access & ACC_STATIC == 0
            }) {
                return Ok((class, m));
            }
        }
        bail!(
            "{iface}.{name}{descriptor} is inherited from outside the unit (e.g. java/lang/Object)"
        )
    }
    /// Interface dispatch table of a class: for every method of every superinterface, the
    /// implementation selected per JVMS 5.4.6, sorted by selector.
    fn itable(&self, name: &str) -> Rc<Vec<ItableEntry>> {
        if let Some(t) = self.itables.borrow().get(name) {
            return t.clone();
        }
        let mut entries: Vec<ItableEntry> = Vec::new();
        if let (Ok(chain), Ok(ifaces)) = (self.chain(name), self.superinterfaces(name)) {
            let mut seen = BTreeSet::new();
            for iface in &ifaces {
                for m in iface.methods.iter().filter(|m| {
                    m.access & (ACC_STATIC | ACC_PRIVATE) == 0 && !m.name.starts_with('<')
                }) {
                    if !seen.insert((m.name.clone(), m.descriptor.clone())) {
                        continue;
                    }
                    // 1. A method in the class or a superclass wins (abstract ones throw when called).
                    let in_class = chain.iter().copied().chain(self.get(OBJECT)).find_map(|c| {
                        c.methods
                            .iter()
                            .find(|x| {
                                x.name == m.name
                                    && x.descriptor == m.descriptor
                                    && x.access & (ACC_STATIC | ACC_PRIVATE) == 0
                            })
                            .map(|x| (c, x))
                    });
                    let target = match in_class {
                        Some((_, x)) if x.access & ACC_ABSTRACT != 0 => ItableTarget::Abstract,
                        Some((c, _)) => ItableTarget::Impl((
                            c.name.clone(),
                            m.name.clone(),
                            m.descriptor.clone(),
                        )),
                        None => {
                            // 2. Otherwise the maximally specific default method, if exactly one.
                            let candidates: Vec<&ClassFile> = ifaces
                                .iter()
                                .copied()
                                .filter(|i| {
                                    i.methods.iter().any(|x| {
                                        x.name == m.name
                                            && x.descriptor == m.descriptor
                                            && x.access & (ACC_STATIC | ACC_PRIVATE | ACC_ABSTRACT)
                                                == 0
                                    })
                                })
                                .collect();
                            let specific: Vec<&ClassFile> = candidates
                                .iter()
                                .copied()
                                .filter(|c| {
                                    !candidates.iter().any(|other| {
                                        other.name != c.name
                                            && self.superinterfaces(&other.name).is_ok_and(|sup| {
                                                sup.iter().any(|s| s.name == c.name)
                                            })
                                    })
                                })
                                .collect();
                            match specific.as_slice() {
                                [one] => ItableTarget::Impl((
                                    one.name.clone(),
                                    m.name.clone(),
                                    m.descriptor.clone(),
                                )),
                                [] => ItableTarget::Abstract,
                                _ => ItableTarget::Ambiguous,
                            }
                        }
                    };
                    entries.push(ItableEntry {
                        selector: selector(&m.name, &m.descriptor),
                        name: m.name.clone(),
                        descriptor: m.descriptor.clone(),
                        target,
                    });
                }
            }
        }
        entries.sort_by_key(|e| e.selector);
        for pair in entries.windows(2) {
            assert!(
                pair[0].selector != pair[1].selector,
                "interface selector collision in {name}: {} and {}",
                pair[0].name,
                pair[1].name
            );
        }
        let entries = Rc::new(entries);
        self.itables
            .borrow_mut()
            .insert(name.to_string(), entries.clone());
        entries
    }
    /// Whether a method declared in `owner` with `access` is overridden by a method in class `by` (JVMS 5.4.5).
    fn overrides(owner: &str, access: u16, by: &str) -> bool {
        const PUBLIC_OR_PROTECTED: u16 = 0x0001 | 0x0004;
        access & ACC_PRIVATE == 0
            && (access & PUBLIC_OR_PROTECTED != 0 || package(owner) == package(by))
    }
    /// Vtable of a class: the superclass's slots (overridden where applicable), then new methods.
    fn vtable(&self, name: &str) -> Result<Rc<Vec<Slot>>> {
        if let Some(v) = self.vtables.borrow().get(name) {
            return Ok(v.clone());
        }
        let class = self
            .get(name)
            .ok_or_else(|| anyhow!("{name} is not in the compilation unit"))?;
        let mut slots = match class.super_name.as_deref() {
            Some(parent) if parent != OBJECT && class.access & ACC_INTERFACE == 0 => {
                (*self.vtable(parent)?).clone()
            }
            // Directly below Object: start from Object's own slots (hashCode, equals, ...).
            Some(OBJECT) if class.access & ACC_INTERFACE == 0 && self.get(OBJECT).is_some() => {
                (*self.vtable(OBJECT)?).clone()
            }
            _ => Vec::new(),
        };
        if class.access & ACC_INTERFACE == 0 {
            for m in class
                .methods
                .iter()
                .filter(|m| m.access & (ACC_STATIC | ACC_PRIVATE) == 0 && !m.name.starts_with('<'))
            {
                let overridden = slots.iter().position(|s| {
                    s.name == m.name
                        && s.descriptor == m.descriptor
                        && self
                            .get(&s.owner)
                            .and_then(|o| {
                                o.methods
                                    .iter()
                                    .find(|x| x.name == s.name && x.descriptor == s.descriptor)
                            })
                            .is_some_and(|x| Self::overrides(&s.owner, x.access, name))
                });
                let slot = Slot {
                    name: m.name.clone(),
                    descriptor: m.descriptor.clone(),
                    owner: name.to_string(),
                };
                match overridden {
                    Some(i) => slots[i] = slot,
                    None => slots.push(slot),
                }
            }
        }
        let slots = Rc::new(slots);
        self.vtables
            .borrow_mut()
            .insert(name.to_string(), slots.clone());
        Ok(slots)
    }
}

/// Verification type of a local variable or stack slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VType {
    /// Unusable (unset, the second half of a long/double, or incompatible types merged).
    Top,
    I,
    J,
    F,
    D,
    Null,
    /// Initialized reference whose static type is the interned class name.
    Obj(u32),
    /// `this` inside a constructor before a superclass or sibling constructor has run.
    UninitThis,
    /// Result of `new` at this pc, before its constructor has run.
    Uninit(u32),
}

impl VType {
    /// Verification type of a primitive kind (references need a class; see `Cx::of_ty`).
    fn prim(kind: Kind) -> VType {
        match kind {
            Kind::I => VType::I,
            Kind::J => VType::J,
            Kind::F => VType::F,
            Kind::D => VType::D,
            Kind::A => unreachable!("references carry a class"),
        }
    }
    fn category(self) -> u16 {
        if matches!(self, VType::J | VType::D) {
            2
        } else {
            1
        }
    }
    fn is_reference(self) -> bool {
        matches!(
            self,
            VType::Null | VType::Obj(_) | VType::UninitThis | VType::Uninit(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Frame {
    stack: Vec<VType>,
    locals: Vec<VType>,
}

/// How a call instruction compiles.
#[derive(Debug, Clone)]
enum CallKind {
    /// Static or non-overridable target: call the implementation's symbol.
    Direct(MethodKey),
    /// Dispatch through vtable slot `slot` of the receiver's class.
    Virtual { slot: usize, target: MethodKey },
    /// `java/lang/Object.<init>()V`: no effect.
    ObjectInit,
    /// Select the implementation from the receiver's interface table by selector.
    Interface { selector: u64, target: MethodKey },
}

/// What verification resolved for one instruction, for emission.
#[derive(Debug, Clone)]
enum Resolved {
    Call(CallKind),
    /// Field access resolved to its declaring class.
    Field(String),
    /// Array access: the element descriptor char of the array's static type (`Z` vs `B` matters
    /// for stores), or None when the array is statically null (the access always throws).
    Element(Option<char>),
    /// Member of a class outside the unit: the instruction stops with this diagnostic if it
    /// ever executes (the JVM likewise resolves references lazily, when first executed).
    Unresolved(String),
}

/// Something a compiled method needs to be compiled too.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Dep {
    Method(MethodKey),
    Init(String),
}

/// Facts about the method being verified that instruction checks need.
struct Cx<'u, 'a> {
    unit: &'u Unit<'a>,
    class: &'a ClassFile,
    sig: &'u Signature,
    is_constructor: bool,
}

impl Cx<'_, '_> {
    fn describe(&self, v: VType) -> String {
        match v {
            VType::Obj(id) => self.unit.name(id),
            other => format!("{other:?}"),
        }
    }
    /// Verification type of a descriptor type.
    fn of_ty(&self, ty: &Ty) -> VType {
        match &ty.class {
            Some(class) => VType::Obj(self.unit.intern(class)),
            None => VType::prim(kind_of(ty.code).unwrap()),
        }
    }
    /// Whether a value may be used where `target` is expected. Interfaces accept any
    /// reference, as in the JVM verifier; unknown hierarchies are rejected.
    fn assignable(&self, v: VType, target: &str) -> bool {
        match v {
            VType::Null => true,
            VType::Obj(id) => self.name_assignable(&self.unit.name(id), target),
            _ => false,
        }
    }
    /// Whether a value of static type `name` (class name or array descriptor) fits `target`.
    fn name_assignable(&self, name: &str, target: &str) -> bool {
        if target == OBJECT || name == target {
            return true;
        }
        match (name.starts_with('['), target.starts_with('[')) {
            // Arrays are covariant in their reference element type; primitive arrays must match exactly.
            (true, true) => match (element_class(name), element_class(target)) {
                (Some(a), Some(b)) => self.name_assignable(&a, &b),
                _ => false,
            },
            (true, false) => matches!(target, "java/lang/Cloneable" | "java/io/Serializable"),
            (false, true) => false,
            (false, false) => {
                // A class outside the unit is opaque: nothing native accesses a value through it,
                // so any reference may stand in for it (as the JVM would only check at link time).
                self.unit.get(target).is_none()
                    || self.unit.is_interface(target)
                    || self.unit.is_subclass(name, target) == Some(true)
            }
        }
    }
    /// Least common supertype at a control-flow join (java/lang/Object when nothing closer is known).
    fn merge(&self, a: VType, b: VType) -> Option<VType> {
        match (a, b) {
            (a, b) if a == b => Some(a),
            (VType::Null, o @ VType::Obj(_)) | (o @ VType::Obj(_), VType::Null) => Some(o),
            (VType::Obj(x), VType::Obj(y)) => {
                let (x, y) = (self.unit.name(x), self.unit.name(y));
                let common = self
                    .unit
                    .chain(&x)
                    .ok()
                    .and_then(|chain| {
                        chain
                            .iter()
                            .find(|c| self.unit.is_subclass(&y, &c.name) == Some(true))
                            .map(|c| c.name.clone())
                    })
                    .unwrap_or_else(|| OBJECT.to_string());
                Some(VType::Obj(self.unit.intern(&common)))
            }
            _ => None,
        }
    }
    /// Merge two frames at a join: stack shapes must agree, locals that disagree become unusable.
    fn merge_frames(&self, a: &Frame, b: &Frame, pc: u32) -> Result<Frame> {
        if a.stack.len() != b.stack.len() {
            bail!("pc {pc}: stack depth differs between paths");
        }
        let stack = a
            .stack
            .iter()
            .zip(&b.stack)
            .map(|(x, y)| {
                self.merge(*x, *y).ok_or_else(|| {
                    anyhow!(
                        "pc {pc}: stack types {} and {} disagree",
                        self.describe(*x),
                        self.describe(*y)
                    )
                })
            })
            .collect::<Result<_>>()?;
        let locals = a
            .locals
            .iter()
            .zip(&b.locals)
            .map(|(x, y)| self.merge(*x, *y).unwrap_or(VType::Top))
            .collect();
        Ok(Frame { stack, locals })
    }
}

impl Frame {
    fn pop(&mut self, pc: u32) -> Result<VType> {
        self.stack
            .pop()
            .ok_or_else(|| anyhow!("pc {pc}: stack underflow"))
    }
    /// Pop one value and require it to have exactly the primitive type `want`.
    fn pop_prim(&mut self, want: Kind, pc: u32) -> Result<()> {
        match self.pop(pc)? {
            v if want != Kind::A && v == VType::prim(want) => Ok(()),
            other => bail!("pc {pc}: expected {want:?} on stack, found {other:?}"),
        }
    }
    /// Pop an initialized reference assignable to `target`.
    fn pop_ref(&mut self, cx: &Cx, target: &str, pc: u32) -> Result<VType> {
        let v = self.pop(pc)?;
        if !cx.assignable(v, target) {
            bail!(
                "pc {pc}: expected a {target} reference, found {} (or its hierarchy is outside the unit)",
                cx.describe(v)
            );
        }
        Ok(v)
    }
    /// Pop a value of a descriptor type.
    fn pop_ty(&mut self, cx: &Cx, ty: &Ty, pc: u32) -> Result<()> {
        match &ty.class {
            Some(class) => self.pop_ref(cx, class, pc).map(|_| ()),
            None => self.pop_prim(kind_of(ty.code).unwrap(), pc),
        }
    }
    /// Pop one value that is not a long/double, as pop/dup/swap require.
    fn pop_cat1(&mut self, pc: u32) -> Result<VType> {
        match self.pop(pc)? {
            v if v.category() == 1 => Ok(v),
            other => bail!("pc {pc}: expected category-1 value, found {other:?}"),
        }
    }
    fn local(&self, n: u16, pc: u32) -> Result<VType> {
        self.locals
            .get(n as usize)
            .copied()
            .ok_or_else(|| anyhow!("pc {pc}: local {n} out of range"))
    }
    /// Store into a local; long/double take two slots and break any pair they overwrite.
    fn set_local(&mut self, n: u16, v: VType, pc: u32) -> Result<()> {
        let n = n as usize;
        if n + v.category() as usize > self.locals.len() {
            bail!("pc {pc}: local {n} out of range");
        }
        if n > 0 && self.locals[n - 1].category() == 2 {
            self.locals[n - 1] = VType::Top;
        }
        self.locals[n] = v;
        if v.category() == 2 {
            self.locals[n + 1] = VType::Top;
        }
        Ok(())
    }
    /// After a constructor call, every copy of the uninitialized object becomes an initialized `class`.
    fn initialize(&mut self, uninit: VType, class: VType) {
        for v in self.stack.iter_mut().chain(self.locals.iter_mut()) {
            if *v == uninit {
                *v = class;
            }
        }
    }
}

/// What one instruction does to control flow and what it depends on.
#[derive(Default)]
struct Step {
    /// Execution can continue with the next instruction.
    falls: bool,
    targets: Vec<u32>,
    deps: Vec<Dep>,
    resolved: Option<Resolved>,
}

/// Verify one instruction against the frame and report its control flow and dependencies.
fn step(insn: &Insn, frame: &mut Frame, cx: &Cx, news: &HashMap<u32, String>) -> Result<Step> {
    let pc = insn.pc;
    let unit = cx.unit;
    let this = &cx.class.name;
    let mut out = Step {
        falls: true,
        ..Step::default()
    };
    let outside = |class: &str| !class.starts_with('[') && unit.get(class).is_none();
    let unresolved = match &insn.op {
        Op::InvokeStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((class.clone(), name.clone(), descriptor.clone(), false, None)),
        Op::InvokeVirtual(m) | Op::InvokeInterface(m) if outside(&m.class) => Some((
            m.class.clone(),
            m.name.clone(),
            m.descriptor.clone(),
            true,
            None,
        )),
        Op::GetStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((
            class.clone(),
            name.clone(),
            descriptor.clone(),
            false,
            Some(true),
        )),
        Op::PutStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((
            class.clone(),
            name.clone(),
            descriptor.clone(),
            false,
            Some(false),
        )),
        Op::GetField(m) if outside(&m.class) => Some((
            m.class.clone(),
            m.name.clone(),
            m.descriptor.clone(),
            true,
            Some(true),
        )),
        Op::PutField(m) if outside(&m.class) => Some((
            m.class.clone(),
            m.name.clone(),
            m.descriptor.clone(),
            true,
            Some(false),
        )),
        _ => None,
    };
    if let Some((class, name, descriptor, receiver, field)) = unresolved {
        match field {
            None => {
                let s = parse_signature(&descriptor)?;
                for p in s.params.iter().rev() {
                    frame.pop_ty(cx, p, pc)?;
                }
                if receiver {
                    frame.pop_ref(cx, OBJECT, pc)?;
                }
                if s.ret.code != 'V' {
                    frame.stack.push(cx.of_ty(&s.ret));
                }
            }
            Some(is_get) => {
                let ty = parse_type(&descriptor, &name)?;
                if !is_get {
                    frame.pop_ty(cx, &ty, pc)?;
                }
                if receiver {
                    frame.pop_ref(cx, OBJECT, pc)?;
                }
                if is_get {
                    frame.stack.push(cx.of_ty(&ty));
                }
            }
        }
        out.resolved = Some(Resolved::Unresolved(format!(
            "{class}.{name}{descriptor}: {class} is not in the compilation unit"
        )));
        return Ok(out);
    }
    match &insn.op {
        Op::Nop => {}
        Op::Goto(t) => {
            out.falls = false;
            out.targets.push(*t);
        }
        Op::Const(v) => frame.stack.push(VType::prim(v.kind())),
        Op::AConstNull => frame.stack.push(VType::Null),
        Op::Load(Kind::A, n) => {
            let v = frame.local(*n, pc)?;
            if !v.is_reference() {
                bail!("pc {pc}: aload of local {n}, which holds {v:?}");
            }
            frame.stack.push(v);
        }
        Op::Load(k, n) => {
            let v = frame.local(*n, pc)?;
            if v != VType::prim(*k) {
                bail!("pc {pc}: load of {k:?} from local {n}, which holds {v:?}");
            }
            frame.stack.push(v);
        }
        Op::Store(Kind::A, n) => {
            let v = frame.pop(pc)?;
            if !v.is_reference() {
                bail!("pc {pc}: astore of {v:?}");
            }
            frame.set_local(*n, v, pc)?;
        }
        Op::Store(k, n) => {
            frame.pop_prim(*k, pc)?;
            frame.set_local(*n, VType::prim(*k), pc)?;
        }
        Op::Iinc(n, _) => {
            if frame.local(*n, pc)? != VType::I {
                bail!("pc {pc}: iinc of non-int local {n}");
            }
        }
        Op::Pop => {
            frame.pop_cat1(pc)?;
        }
        Op::Pop2 => {
            if frame.stack.last().is_some_and(|v| v.category() == 2) {
                frame.pop(pc)?;
            } else {
                frame.pop_cat1(pc)?;
                frame.pop_cat1(pc)?;
            }
        }
        Op::Dup => {
            let v = frame.pop_cat1(pc)?;
            frame.stack.extend([v, v]);
        }
        Op::DupX1 => {
            let a = frame.pop_cat1(pc)?;
            let b = frame.pop_cat1(pc)?;
            frame.stack.extend([a, b, a]);
        }
        Op::Dup2 => {
            if let Some(&v) = frame.stack.last().filter(|v| v.category() == 2) {
                frame.stack.push(v);
            } else {
                let a = frame.pop_cat1(pc)?;
                let b = frame.pop_cat1(pc)?;
                frame.stack.extend([b, a, b, a]);
            }
        }
        Op::Swap => {
            let a = frame.pop_cat1(pc)?;
            let b = frame.pop_cat1(pc)?;
            frame.stack.extend([a, b]);
        }
        Op::Binary(k, op) => {
            let shift = matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr);
            frame.pop_prim(if shift { Kind::I } else { *k }, pc)?;
            frame.pop_prim(*k, pc)?;
            frame.stack.push(VType::prim(*k));
        }
        Op::Neg(k) => {
            frame.pop_prim(*k, pc)?;
            frame.stack.push(VType::prim(*k));
        }
        Op::Convert(c) => {
            let (from, to) = c.kinds();
            frame.pop_prim(from, pc)?;
            frame.stack.push(VType::prim(to));
        }
        Op::Compare(c) => {
            let k = match c {
                Compare::Lcmp => Kind::J,
                Compare::Fcmpl | Compare::Fcmpg => Kind::F,
                Compare::Dcmpl | Compare::Dcmpg => Kind::D,
            };
            frame.pop_prim(k, pc)?;
            frame.pop_prim(k, pc)?;
            frame.stack.push(VType::I);
        }
        Op::If(_, t) => {
            frame.pop_prim(Kind::I, pc)?;
            out.targets.push(*t);
        }
        Op::IfICmp(_, t) => {
            frame.pop_prim(Kind::I, pc)?;
            frame.pop_prim(Kind::I, pc)?;
            out.targets.push(*t);
        }
        Op::IfNull { target, .. } => {
            frame.pop_ref(cx, OBJECT, pc)?;
            out.targets.push(*target);
        }
        Op::IfACmp { target, .. } => {
            frame.pop_ref(cx, OBJECT, pc)?;
            frame.pop_ref(cx, OBJECT, pc)?;
            out.targets.push(*target);
        }
        Op::TableSwitch {
            default, targets, ..
        } => {
            frame.pop_prim(Kind::I, pc)?;
            out.falls = false;
            out.targets.push(*default);
            out.targets.extend(targets);
        }
        Op::LookupSwitch { default, pairs } => {
            frame.pop_prim(Kind::I, pc)?;
            out.falls = false;
            out.targets.push(*default);
            out.targets.extend(pairs.iter().map(|(_, t)| *t));
        }
        Op::Return(k) => {
            if *k != kind_of(cx.sig.ret.code) {
                bail!(
                    "pc {pc}: return kind {k:?} does not match descriptor return {}",
                    cx.sig.ret.code
                );
            }
            if k.is_some() {
                frame.pop_ty(cx, &cx.sig.ret, pc)?;
            }
            if cx.is_constructor && frame.locals.first() == Some(&VType::UninitThis) {
                bail!("pc {pc}: constructor returns before calling a superclass constructor");
            }
            out.falls = false;
        }
        Op::InvokeStatic {
            class: owner,
            name,
            descriptor,
        } => {
            let (declaring, target) = unit.resolve_method(owner, name, descriptor)?;
            if target.access & ACC_STATIC == 0 {
                bail!("pc {pc}: invokestatic of instance method {name}");
            }
            let s = parse_signature(descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), name.clone(), descriptor.clone());
            out.deps
                .extend([Dep::Method(key.clone()), Dep::Init(declaring.name.clone())]);
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::GetStatic {
            class: owner,
            name,
            descriptor,
        }
        | Op::PutStatic {
            class: owner,
            name,
            descriptor,
        } => {
            let (declaring, f) = unit.resolve_field(owner, name, descriptor)?;
            if f.access & ACC_STATIC == 0 {
                bail!("pc {pc}: static access to instance field {name}");
            }
            let ty = parse_type(descriptor, name)?;
            if matches!(insn.op, Op::GetStatic { .. }) {
                frame.stack.push(cx.of_ty(&ty));
            } else {
                frame.pop_ty(cx, &ty, pc)?;
            }
            out.deps.push(Dep::Init(declaring.name.clone()));
            out.resolved = Some(Resolved::Field(declaring.name.clone()));
        }
        Op::New { class } => {
            let Some(target) = unit.get(class) else {
                bail!("pc {pc}: new {class}: class is not in the compilation unit");
            };
            if target.access & (ACC_ABSTRACT | ACC_INTERFACE) != 0 {
                bail!("pc {pc}: new of abstract class or interface {class}");
            }
            unit.chain(class)?;
            frame.stack.push(VType::Uninit(pc));
            out.deps.push(Dep::Init(class.clone()));
        }
        Op::LdcString(_) => {
            let Some(string) = unit.get(STRING) else {
                bail!(
                    "pc {pc}: string constants need java/lang/String in the unit (add the JDK's java.base)"
                );
            };
            for (name, descriptor) in STRING_FIELDS {
                if !string.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                }) {
                    bail!(
                        "pc {pc}: this JDK's String has no {name}:{descriptor} field; only the JDK 9+ layout is supported"
                    );
                }
            }
            unit.chain(STRING)?;
            frame.stack.push(VType::Obj(unit.intern(STRING)));
            // The JVM initializes String before any literal exists; String's own code relies on it.
            out.deps.push(Dep::Init(STRING.to_string()));
        }
        Op::LdcClass(_) => {
            if !classes_available(unit) {
                bail!(
                    "pc {pc}: class constants need java/lang/Class and String in the unit (add the JDK's java.base)"
                );
            }
            frame.stack.push(VType::Obj(unit.intern(CLASS)));
        }
        Op::NewArray(element) => {
            frame.pop_prim(Kind::I, pc)?;
            frame
                .stack
                .push(VType::Obj(unit.intern(&format!("[{element}"))));
        }
        Op::ANewArray { class } => {
            frame.pop_prim(Kind::I, pc)?;
            let element = if class.starts_with('[') {
                class.clone()
            } else {
                format!("L{class};")
            };
            frame
                .stack
                .push(VType::Obj(unit.intern(&format!("[{element}"))));
        }
        Op::ArrayLength => {
            let array = frame.pop(pc)?;
            if !matches!(array, VType::Null)
                && !matches!(array, VType::Obj(id) if unit.name(id).starts_with('['))
            {
                bail!("pc {pc}: arraylength of {}", cx.describe(array));
            }
            frame.stack.push(VType::I);
        }
        Op::ArrayLoad(family) | Op::ArrayStore(family) => {
            let storing = matches!(insn.op, Op::ArrayStore(_));
            if storing {
                match *family {
                    'A' => {
                        frame.pop_ref(cx, OBJECT, pc)?;
                    }
                    'B' | 'C' | 'S' => frame.pop_prim(Kind::I, pc)?,
                    f => frame.pop_prim(kind_of(f).unwrap(), pc)?,
                }
            }
            frame.pop_prim(Kind::I, pc)?;
            let array = frame.pop(pc)?;
            let element = match array {
                VType::Null => None,
                VType::Obj(id) if unit.name(id).starts_with('[') => {
                    let name = unit.name(id);
                    let e = name.as_bytes()[1] as char;
                    let fits = match *family {
                        'A' => matches!(e, 'L' | '['),
                        'B' => matches!(e, 'B' | 'Z'),
                        f => f == e,
                    };
                    if !fits {
                        bail!("pc {pc}: element access of kind {family} on {name}");
                    }
                    Some(e)
                }
                other => bail!("pc {pc}: array access on {}", cx.describe(other)),
            };
            if !storing {
                let pushed = match (*family, &array) {
                    ('A', VType::Null) => VType::Null,
                    ('A', VType::Obj(id)) => {
                        VType::Obj(unit.intern(&element_class(&unit.name(*id)).unwrap()))
                    }
                    ('B' | 'C' | 'S', _) => VType::I,
                    (f, _) => VType::prim(kind_of(f).unwrap()),
                };
                frame.stack.push(pushed);
            }
            out.resolved = Some(Resolved::Element(element));
        }
        Op::CheckCast { class } | Op::InstanceOf { class } => {
            if class.starts_with('[') {
                // Array types: checked at runtime against the array's own descriptor.
                parse_type(class, "cast")?;
                frame.pop_ref(cx, OBJECT, pc)?;
                let result = if matches!(insn.op, Op::CheckCast { .. }) {
                    VType::Obj(unit.intern(class))
                } else {
                    VType::I
                };
                frame.stack.push(result);
                return Ok(out);
            }
            if unit.get(class).is_none() || unit.chain(class).is_err() {
                bail!(
                    "pc {pc}: type test against {class} (outside the unit or an interface) is not compiled yet"
                );
            }
            frame.pop_ref(cx, OBJECT, pc)?;
            let result = if matches!(insn.op, Op::CheckCast { .. }) {
                VType::Obj(unit.intern(class))
            } else {
                VType::I
            };
            frame.stack.push(result);
        }
        Op::GetField(m) | Op::PutField(m) => {
            let (declaring, f) = unit.resolve_field(&m.class, &m.name, &m.descriptor)?;
            if f.access & ACC_STATIC != 0 || unit.is_interface(&declaring.name) {
                bail!("pc {pc}: instance access to static field {}", m.name);
            }
            let ty = parse_type(&m.descriptor, &m.name)?;
            if matches!(insn.op, Op::GetField(_)) {
                frame.pop_ref(cx, &declaring.name, pc)?;
                frame.stack.push(cx.of_ty(&ty));
            } else {
                frame.pop_ty(cx, &ty, pc)?;
                // A constructor may assign its own class's fields before calling super().
                match frame.pop(pc)? {
                    VType::UninitThis if cx.is_constructor && declaring.name == *this => {}
                    v if cx.assignable(v, &declaring.name) => {}
                    v => bail!("pc {pc}: putfield {} on {}", m.name, cx.describe(v)),
                }
            }
            out.resolved = Some(Resolved::Field(declaring.name.clone()));
        }
        // Methods on arrays are Object's (clone is the array copy); arrays have no vtable, so call directly.
        Op::InvokeVirtual(m) if m.class.starts_with('[') => {
            let (declaring, target) = unit.resolve_method(OBJECT, &m.name, &m.descriptor)?;
            if target.access & ACC_STATIC != 0 || m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {} on an array", m.name);
            }
            let s = parse_signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::InvokeVirtual(m) => {
            if m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {}", m.name);
            }
            let (declaring, target) = match unit.resolve_method(&m.class, &m.name, &m.descriptor) {
                Ok(found) => found,
                // A default method inherited from a superinterface: dispatch like invokeinterface.
                Err(e) => {
                    let found = unit.superinterfaces(&m.class).ok().and_then(|ifaces| {
                        ifaces.into_iter().find_map(|i| {
                            i.methods
                                .iter()
                                .find(|x| {
                                    x.name == m.name
                                        && x.descriptor == m.descriptor
                                        && x.access & ACC_STATIC == 0
                                })
                                .map(|x| (i, x))
                        })
                    });
                    let Some((iface, _)) = found else {
                        return Err(e);
                    };
                    let s = parse_signature(&m.descriptor)?;
                    for p in s.params.iter().rev() {
                        frame.pop_ty(cx, p, pc)?;
                    }
                    frame.pop_ref(cx, &m.class, pc)?;
                    if s.ret.code != 'V' {
                        frame.stack.push(cx.of_ty(&s.ret));
                    }
                    let target = (iface.name.clone(), m.name.clone(), m.descriptor.clone());
                    out.resolved = Some(Resolved::Call(CallKind::Interface {
                        selector: selector(&m.name, &m.descriptor),
                        target,
                    }));
                    return Ok(out);
                }
            };
            if target.access & ACC_STATIC != 0 {
                bail!("pc {pc}: invokevirtual of static method {}", m.name);
            }
            let s = parse_signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            // Private, final and final-class targets cannot be overridden: call them directly.
            let fixed =
                target.access & (ACC_PRIVATE | ACC_FINAL) != 0 || declaring.access & ACC_FINAL != 0;
            if fixed {
                out.deps.push(Dep::Method(key.clone()));
                out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
            } else {
                let vtable = unit.vtable(&m.class)?;
                let slot = vtable
                    .iter()
                    .position(|s| s.name == m.name && s.descriptor == m.descriptor)
                    .ok_or_else(|| anyhow!("pc {pc}: {} has no vtable slot", m.name))?;
                // No dependency on implementations: a slot whose implementation is not compiled
                // holds a stub (see `emit_stubs`), so subclasses never change this decision.
                out.resolved = Some(Resolved::Call(CallKind::Virtual { slot, target: key }));
            }
        }
        Op::MonitorEnter | Op::MonitorExit => {
            frame.pop_ref(cx, OBJECT, pc)?;
        }
        Op::AThrow => {
            if unit.get(THROWABLE).is_none() {
                bail!(
                    "pc {pc}: throw needs java/lang/Throwable in the unit (add the JDK's java.base)"
                );
            }
            frame.pop_ref(cx, THROWABLE, pc)?;
            out.falls = false;
        }
        Op::InvokeInterface(m) => {
            if !unit.is_interface(&m.class) {
                bail!(
                    "pc {pc}: invokeinterface on {}, which is not an interface in the unit",
                    m.class
                );
            }
            let (declaring, _) = unit.resolve_interface_method(&m.class, &m.name, &m.descriptor)?;
            let s = parse_signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            // No dependency on implementations: uncompiled ones are stubs in the receiver's table.
            let target = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.resolved = Some(Resolved::Call(CallKind::Interface {
                selector: selector(&m.name, &m.descriptor),
                target,
            }));
        }
        Op::InvokeSpecial(m) if unit.is_interface(&m.class) && m.name != "<init>" => {
            // Interface.super.m() or a private interface method: a direct call to that body.
            let (declaring, target) =
                unit.resolve_interface_method(&m.class, &m.name, &m.descriptor)?;
            if target.access & ACC_ABSTRACT != 0 {
                bail!(
                    "pc {pc}: invokespecial of abstract interface method {}",
                    m.name
                );
            }
            let s = parse_signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, this, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::InvokeSpecial(m) => {
            let parent = cx.class.super_name.as_deref().unwrap_or(OBJECT);
            if m.name == "<init>" && m.class == OBJECT && m.descriptor == "()V" {
                // Either `super()` in a direct subclass of Object, or `new Object()`.
                match frame.pop(pc)? {
                    VType::UninitThis if parent == OBJECT => {
                        frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
                    }
                    v @ VType::Uninit(at) if news.get(&at).map(String::as_str) == Some(OBJECT) => {
                        frame.initialize(v, VType::Obj(unit.intern(OBJECT)));
                    }
                    other => bail!("pc {pc}: Object.<init> called on {}", cx.describe(other)),
                }
                out.resolved = Some(Resolved::Call(CallKind::ObjectInit));
                return Ok(out);
            }
            let s = parse_signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            let key = if m.name == "<init>" {
                let target = unit.get(&m.class).ok_or_else(|| {
                    anyhow!("pc {pc}: constructor of {} outside the unit", m.class)
                })?;
                if !target
                    .methods
                    .iter()
                    .any(|x| x.name == "<init>" && x.descriptor == m.descriptor)
                {
                    bail!("pc {pc}: no constructor {}{}", m.class, m.descriptor);
                }
                match frame.pop(pc)? {
                    // The constructor must belong to exactly the class that `new` created.
                    v @ VType::Uninit(at) if news.get(&at) == Some(&m.class) => {
                        frame.initialize(v, VType::Obj(unit.intern(&m.class)));
                    }
                    VType::UninitThis
                        if cx.is_constructor && (m.class == *this || m.class == parent) =>
                    {
                        frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
                    }
                    other => bail!(
                        "pc {pc}: constructor of {} called on {}",
                        m.class,
                        cx.describe(other)
                    ),
                }
                (m.class.clone(), m.name.clone(), m.descriptor.clone())
            } else {
                // Private methods (pre-nestmate style) and super.m() calls: no dynamic dispatch.
                let start = if m.class == *this {
                    this.as_str()
                } else {
                    parent
                };
                let (declaring, target) = unit.resolve_method(start, &m.name, &m.descriptor)?;
                if target.access & ACC_STATIC != 0 {
                    bail!("pc {pc}: invokespecial of static method {}", m.name);
                }
                frame.pop_ref(cx, this, pc)?;
                if s.ret.code != 'V' {
                    frame.stack.push(cx.of_ty(&s.ret));
                }
                (declaring.name.clone(), m.name.clone(), m.descriptor.clone())
            };
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
    }
    Ok(out)
}

/// One exception handler, verified: catch class (None catches everything) and handler pc.
#[derive(Debug, Clone, PartialEq)]
struct Catch {
    class: Option<String>,
    handler: u32,
}

impl Analyzed {
    /// Placeholder analysis for methods whose body the runtime supplies (intrinsics).
    fn empty() -> Analyzed {
        Analyzed {
            exits: HashMap::new(),
            dispatch: Vec::new(),
            insns: Vec::new(),
            stacks: Vec::new(),
            targets: BTreeSet::new(),
            deps: BTreeSet::new(),
            resolved: HashMap::new(),
        }
    }
}

struct Analyzed {
    /// pc -> index into `dispatch` for instructions inside try ranges.
    exits: HashMap<u32, usize>,
    /// Handler chains in table order; each becomes a dispatch block in the C function.
    dispatch: Vec<Vec<Catch>>,
    insns: Vec<Insn>,
    /// Operand stack types before each instruction.
    stacks: Vec<Vec<VType>>,
    targets: BTreeSet<u32>,
    deps: BTreeSet<Dep>,
    resolved: HashMap<u32, Resolved>,
}

/// Decode a method and verify every reachable path, or explain why it cannot be compiled.
fn analyze<'a>(
    unit: &Unit<'a>,
    class: &'a ClassFile,
    method: &Member,
    sig: &Signature,
) -> std::result::Result<Analyzed, String> {
    let code = method
        .code
        .as_ref()
        .ok_or("no bytecode (abstract or native)")?;

    let insns = match bytecode::decode(class, &code.bytecode) {
        Ok(insns) => insns,
        Err(DecodeError::Unsupported(u)) => return Err(u.to_string()),
        Err(DecodeError::Malformed(e)) => return Err(format!("malformed bytecode: {e:#}")),
    };
    // Which class each `new` creates, so constructor calls can be matched to it.
    let news: HashMap<u32, String> = insns
        .iter()
        .filter_map(|i| match &i.op {
            Op::New { class } => Some((i.pc, class.clone())),
            _ => None,
        })
        .collect();
    let is_constructor = method.name == "<init>";
    let cx = Cx {
        unit,
        class,
        sig,
        is_constructor,
    };
    let mut entry = Frame {
        stack: Vec::new(),
        locals: vec![VType::Top; code.max_locals as usize],
    };
    let mut slot = 0u16;
    let mut place = |v: VType, frame: &mut Frame| -> std::result::Result<(), String> {
        frame
            .set_local(slot, v, 0)
            .map_err(|_| "parameters exceed max_locals".to_string())?;
        slot += v.category();
        Ok(())
    };
    if method.access & ACC_STATIC == 0 {
        let this = if is_constructor {
            VType::UninitThis
        } else {
            VType::Obj(unit.intern(&class.name))
        };
        place(this, &mut entry)?;
    }
    for p in &sig.params {
        place(cx.of_ty(p), &mut entry)?;
    }

    let index: HashMap<u32, usize> = insns
        .iter()
        .enumerate()
        .map(|(i, insn)| (insn.pc, i))
        .collect();
    // Exception handlers: (start, end, catch). Catch classes must be Throwables in the unit.
    let mut handlers: Vec<(u32, u32, Catch)> = Vec::new();
    for h in &code.handlers {
        let class = if h.catch_type == 0 {
            None
        } else {
            let name = class
                .class_name(h.catch_type)
                .map_err(|e| format!("{e:#}"))?
                .to_string();
            if unit.get(&name).is_none() || unit.is_subclass(&name, THROWABLE) != Some(true) {
                return Err(format!(
                    "catch type {name} is outside the unit or not a Throwable"
                ));
            }
            Some(name)
        };
        if !index.contains_key(&(h.handler_pc as u32)) {
            return Err(format!("handler pc {} is not an instruction", h.handler_pc));
        }
        handlers.push((
            h.start_pc as u32,
            h.end_pc as u32,
            Catch {
                class,
                handler: h.handler_pc as u32,
            },
        ));
    }
    let mut frames: Vec<Option<Frame>> = vec![None; insns.len()];
    let mut targets = BTreeSet::new();
    let mut deps = BTreeSet::new();
    let mut resolved = HashMap::new();
    let mut work = vec![(0usize, entry)];
    while let Some((i, incoming)) = work.pop() {
        let pc = insns[i].pc;
        let frame = match &frames[i] {
            None => incoming,
            Some(existing) => {
                let merged = cx
                    .merge_frames(existing, &incoming, pc)
                    .map_err(|e| format!("{e:#}"))?;
                if merged == *existing {
                    continue;
                }
                merged
            }
        };
        let depth: u32 = frame.stack.iter().map(|v| v.category() as u32).sum();
        if depth > code.max_stack as u32 {
            return Err(format!("pc {pc}: stack exceeds max_stack"));
        }
        frames[i] = Some(frame.clone());
        // Every instruction in a try range may transfer to its handlers with its incoming locals
        // and only the exception on the stack.
        for (start, end, catch) in &handlers {
            if *start <= pc && pc < *end {
                if frame
                    .locals
                    .iter()
                    .any(|v| matches!(v, VType::UninitThis | VType::Uninit(_)))
                {
                    return Err(format!(
                        "pc {pc}: try range covers an object before its constructor ran"
                    ));
                }
                let thrown = catch.class.as_deref().unwrap_or(THROWABLE);
                let entry = Frame {
                    stack: vec![VType::Obj(unit.intern(thrown))],
                    locals: frame.locals.clone(),
                };
                targets.insert(catch.handler);
                work.push((index[&catch.handler], entry));
            }
        }
        let mut next = frame;
        let step = step(&insns[i], &mut next, &cx, &news).map_err(|e| format!("{e:#}"))?;
        deps.extend(step.deps);
        if let Some(r) = step.resolved {
            resolved.insert(pc, r);
        }
        for t in step.targets {
            let Some(&j) = index.get(&t) else {
                return Err(format!("pc {pc}: branch target {t} is not an instruction"));
            };
            targets.insert(t);
            work.push((j, next.clone()));
        }
        if step.falls {
            if i + 1 >= insns.len() {
                return Err(format!(
                    "pc {pc}: execution falls off the end of the method"
                ));
            }
            work.push((i + 1, next));
        }
    }
    // Unreachable instructions are never emitted.
    let (insns, stacks): (Vec<_>, Vec<_>) = insns
        .into_iter()
        .zip(frames)
        .filter_map(|(insn, f)| f.map(|f| (insn, f.stack)))
        .unzip();
    // Exceptional exits: instructions in try ranges jump to a dispatch block for their handler chain.
    let mut exits = HashMap::new();
    let mut dispatch: Vec<Vec<Catch>> = Vec::new();
    for insn in &insns {
        let chain: Vec<Catch> = handlers
            .iter()
            .filter(|(start, end, _)| *start <= insn.pc && insn.pc < *end)
            .map(|(_, _, c)| c.clone())
            .collect();
        if chain.is_empty() {
            continue;
        }
        let id = dispatch
            .iter()
            .position(|c| *c == chain)
            .unwrap_or_else(|| {
                dispatch.push(chain);
                dispatch.len() - 1
            });
        exits.insert(insn.pc, id);
    }
    Ok(Analyzed {
        exits,
        dispatch,
        insns,
        stacks,
        targets,
        deps,
        resolved,
    })
}

/// Exact C literal for a constant (floats via bit patterns so NaN/-0.0 survive).
fn value_literal(v: Value) -> String {
    match v {
        Value::I(x) => format!("(int32_t)0x{:08x}u", x as u32),
        Value::J(x) => format!("(int64_t)UINT64_C(0x{:016x})", x as u64),
        Value::F(bits) => format!("jabs_fbits(0x{bits:08x}u)"),
        Value::D(bits) => format!("jabs_dbits(UINT64_C(0x{bits:016x}))"),
    }
}

/// C comparison operator for a branch condition.
fn cond_op(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "==",
        Cond::Ne => "!=",
        Cond::Lt => "<",
        Cond::Ge => ">=",
        Cond::Gt => ">",
        Cond::Le => "<=",
    }
}

/// C parameter list for a method: `self` first for instance methods.
fn c_params(is_static: bool, sig: &Signature, names: bool) -> String {
    let mut params = Vec::new();
    if !is_static {
        params.push(if names {
            "jabs_object *self".to_string()
        } else {
            "jabs_object *".to_string()
        });
    }
    for (i, p) in sig.params.iter().enumerate() {
        params.push(if names {
            format!("{} a{i}", c_type(p.code))
        } else {
            c_type(p.code).to_string()
        });
    }
    if params.is_empty() {
        "void".into()
    } else {
        params.join(", ")
    }
}

/// C function prototype for a compiled method.
fn prototype(class: &str, method: &Member, sig: &Signature) -> String {
    format!(
        "{} {}({})",
        c_type(sig.ret.code),
        symbol(class, &method.name, &method.descriptor),
        c_params(method.access & ACC_STATIC != 0, sig, true)
    )
}

/// `if (init() != 0) return` guarding first use of another class; nothing when `target` is
/// necessarily initialized whenever this code runs (the current class or one of its superclasses).
fn init_guard(unit: &Unit, current: &str, target: &str, ret_default: &str) -> String {
    if !unit.is_interface(target) && unit.is_subclass(current, target) == Some(true) {
        String::new()
    } else {
        format!("if ({}() != 0) {ret_default} ", init_symbol(target))
    }
}

/// Emit one analyzed method as C: locals and stack become `jabs_value` arrays, pcs become labels.
fn emit_method(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    method: &Member,
    sig: &Signature,
    a: &Analyzed,
) {
    let code = method.code.as_ref().unwrap();
    let ret_default = if sig.ret.code == 'V' {
        "return;"
    } else {
        "return 0;"
    };
    let _ = writeln!(
        out,
        "/* {}.{}{} */",
        class.name, method.name, method.descriptor
    );
    let synchronized = method.access & ACC_SYNCHRONIZED != 0;
    if synchronized {
        // The body gets a private name; the public symbol is the locking wrapper below.
        let body = format!(
            "{}__sync",
            symbol(&class.name, &method.name, &method.descriptor)
        );
        let params = c_params(method.access & ACC_STATIC != 0, sig, true);
        let _ = writeln!(out, "static {} {body}({params}) {{", c_type(sig.ret.code));
    } else {
        let _ = writeln!(out, "{} {{", prototype(&class.name, method, sig));
    }
    let _ = writeln!(
        out,
        "    jabs_value l[{}] = {{{{0}}}};",
        code.max_locals.max(1)
    );
    let _ = writeln!(
        out,
        "    jabs_value s[{}] = {{{{0}}}};",
        code.max_stack.max(1)
    );
    let _ = writeln!(out, "    (void)l; (void)s;");
    let mut slot = 0usize;
    if method.access & ACC_STATIC == 0 {
        let _ = writeln!(out, "    l[0].a = self;");
        slot = 1;
    }
    for (i, p) in sig.params.iter().enumerate() {
        let k = kind_of(p.code).unwrap();
        let _ = writeln!(out, "    l[{slot}].{} = a{i};", field(k));
        slot += if matches!(k, Kind::J | Kind::D) { 2 } else { 1 };
    }
    // Raise a VM exception and leave through `exit` (a handler dispatch block or a return).
    // Raises record the method that raised them (stack traces are not captured yet).
    let site = c_escape(&format!(
        "{}.{}{}",
        class.name, method.name, method.descriptor
    ));
    let throw =
        |exc: &str, exit: &str| format!("{{ jabs_where = \"{site}\"; {} {exit} }}", raise(exc));
    // Emit a call: arguments (and the receiver, for instance methods) come off the stack.
    let call = |kind: &CallKind, descriptor: &str, d: usize, exit: &str| -> String {
        let npe = throw("java/lang/NullPointerException", exit);
        let callee = parse_signature(descriptor).unwrap();
        let base = d - callee.params.len();
        let (key, receiver) = match kind {
            CallKind::Direct(key) => {
                let declaring = unit.get(&key.0).unwrap();
                let m = declaring
                    .methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
                    .unwrap();
                (key, m.access & ACC_STATIC == 0)
            }
            CallKind::Virtual { target, .. } | CallKind::Interface { target, .. } => (target, true),
            CallKind::ObjectInit => unreachable!(),
        };
        let result_at = if receiver { base - 1 } else { base };
        let mut args: Vec<String> = Vec::new();
        if receiver {
            args.push(format!("s[{}].a", base - 1));
        }
        args.extend(
            callee
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| format!("s[{}].{}", base + i, field(kind_of(p.code).unwrap()))),
        );
        let function = match kind {
            CallKind::Virtual { slot, .. } => format!(
                "(({} (*)({}))s[{}].a->cls->vtable[{slot}])",
                c_type(callee.ret.code),
                c_params(false, &callee, false),
                base - 1
            ),
            CallKind::Interface { selector, .. } => format!(
                "(({} (*)({}))jabs_itable_find(s[{}].a, UINT64_C(0x{selector:016x})))",
                c_type(callee.ret.code),
                c_params(false, &callee, false),
                base - 1
            ),
            _ => symbol(&key.0, &key.1, &key.2),
        };
        let guard = if let CallKind::Interface { selector, .. } = kind {
            // The receiver's class must implement the interface (JVMS invokeinterface).
            format!(
                "if (!s[{r}].a) {npe} if (!jabs_itable_find(s[{r}].a, UINT64_C(0x{selector:016x}))) {} ",
                throw("java/lang/IncompatibleClassChangeError", exit),
                r = base - 1
            )
        } else if receiver {
            format!("if (!s[{}].a) {npe} ", base - 1)
        } else {
            init_guard(unit, &class.name, &key.0, exit)
        };
        let invoke = format!("{function}({})", args.join(", "));
        let assign = match kind_of(callee.ret.code) {
            Some(k) => format!("s[{result_at}].{} = {invoke};", field(k)),
            None => format!("{invoke};"),
        };
        format!("{guard}{assign} if (jabs_pending) {exit}")
    };
    for (insn, stack) in a.insns.iter().zip(&a.stacks) {
        if a.targets.contains(&insn.pc) {
            let _ = writeln!(out, "L{}:;", insn.pc);
        }
        let d = stack.len();
        let resolved = a.resolved.get(&insn.pc);
        let exit = match a.exits.get(&insn.pc) {
            Some(n) => format!("goto E{n};"),
            None => ret_default.to_string(),
        };
        let exit = exit.as_str();
        let npe = throw("java/lang/NullPointerException", exit);
        if let Some(Resolved::Unresolved(message)) = resolved {
            let _ = writeln!(out, "    jabs_not_compiled(\"{}\");", c_escape(message));
            continue;
        }
        let line = match &insn.op {
            Op::Nop | Op::Pop | Op::Pop2 => continue,
            Op::Const(v) => format!("s[{d}].{} = {};", field(v.kind()), value_literal(*v)),
            Op::AConstNull => format!("s[{d}].a = NULL;"),
            Op::Load(k, n) => format!("s[{d}].{f} = l[{n}].{f};", f = field(*k)),
            Op::Store(k, n) => format!("l[{n}].{f} = s[{}].{f};", d - 1, f = field(*k)),
            Op::Iinc(n, c) => format!("l[{n}].i = jabs_iadd(l[{n}].i, {c});"),
            Op::Dup => format!("s[{d}] = s[{}];", d - 1),
            Op::DupX1 => format!(
                "s[{d}] = s[{}]; s[{}] = s[{}]; s[{}] = s[{d}];",
                d - 1,
                d - 1,
                d - 2,
                d - 2
            ),
            Op::Dup2 if stack[d - 1].category() == 2 => format!("s[{d}] = s[{}];", d - 1),
            Op::Dup2 => format!("s[{d}] = s[{}]; s[{}] = s[{}];", d - 2, d + 1, d - 1),
            Op::Swap => format!(
                "{{ jabs_value t = s[{}]; s[{}] = s[{}]; s[{}] = t; }}",
                d - 1,
                d - 1,
                d - 2,
                d - 2
            ),
            Op::Binary(k, op) => {
                let (f, a, b) = (field(*k), d - 2, d - 1);
                let rhs = if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                    "i"
                } else {
                    f
                };
                let prefix = match k {
                    Kind::I => "jabs_i",
                    Kind::J => "jabs_l",
                    _ => "",
                };
                let name = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::Mul => "mul",
                    BinOp::Div => "div",
                    BinOp::Rem => "rem",
                    BinOp::Shl => "shl",
                    BinOp::Shr => "shr",
                    BinOp::Ushr => "ushr",
                    BinOp::And => "&",
                    BinOp::Or => "|",
                    BinOp::Xor => "^",
                };
                let expr = match (k, op) {
                    (_, BinOp::And | BinOp::Or | BinOp::Xor) => {
                        format!("s[{a}].{f} {name} s[{b}].{f}")
                    }
                    (Kind::F, BinOp::Rem) => format!("fmodf(s[{a}].f, s[{b}].f)"),
                    (Kind::D, BinOp::Rem) => format!("fmod(s[{a}].d, s[{b}].d)"),
                    (Kind::F | Kind::D, _) => {
                        let sym = match op {
                            BinOp::Add => "+",
                            BinOp::Sub => "-",
                            BinOp::Mul => "*",
                            _ => "/",
                        };
                        format!("s[{a}].{f} {sym} s[{b}].{f}")
                    }
                    _ => format!("{prefix}{name}(s[{a}].{f}, s[{b}].{rhs})"),
                };
                let check = match (k, op) {
                    (Kind::I | Kind::J, BinOp::Div | BinOp::Rem) => {
                        format!(
                            "if (s[{b}].{f} == 0) {} ",
                            throw("java/lang/ArithmeticException", exit)
                        )
                    }
                    _ => String::new(),
                };
                format!("{check}s[{a}].{f} = {expr};")
            }
            Op::Neg(k) => {
                let f = field(*k);
                match k {
                    Kind::I => format!("s[{0}].i = jabs_ineg(s[{0}].i);", d - 1),
                    Kind::J => format!("s[{0}].j = jabs_lneg(s[{0}].j);", d - 1),
                    _ => format!("s[{0}].{f} = -s[{0}].{f};", d - 1),
                }
            }
            Op::Convert(c) => {
                let (from, to) = c.kinds();
                let src = format!("s[{}].{}", d - 1, field(from));
                let expr = match c {
                    Convert::I2l => format!("(int64_t){src}"),
                    Convert::I2f | Convert::L2f | Convert::D2f => format!("(float){src}"),
                    Convert::I2d | Convert::L2d | Convert::F2d => format!("(double){src}"),
                    other => format!("jabs_{}({src})", format!("{other:?}").to_ascii_lowercase()),
                };
                format!("s[{}].{} = {expr};", d - 1, field(to))
            }
            Op::Compare(c) => {
                let f = match c {
                    Compare::Lcmp => "j",
                    Compare::Fcmpl | Compare::Fcmpg => "f",
                    _ => "d",
                };
                let name = format!("{c:?}").to_ascii_lowercase();
                format!(
                    "s[{a}].i = jabs_{name}(s[{a}].{f}, s[{b}].{f});",
                    a = d - 2,
                    b = d - 1
                )
            }
            Op::If(c, t) => format!("if (s[{}].i {} 0) goto L{t};", d - 1, cond_op(*c)),
            Op::IfICmp(c, t) => format!(
                "if (s[{}].i {} s[{}].i) goto L{t};",
                d - 2,
                cond_op(*c),
                d - 1
            ),
            Op::IfNull { is_null, target } => {
                format!(
                    "if (s[{}].a {} NULL) goto L{target};",
                    d - 1,
                    if *is_null { "==" } else { "!=" }
                )
            }
            Op::IfACmp { equal, target } => {
                format!(
                    "if (s[{}].a {} s[{}].a) goto L{target};",
                    d - 2,
                    if *equal { "==" } else { "!=" },
                    d - 1
                )
            }
            Op::Goto(t) => format!("goto L{t};"),
            Op::TableSwitch {
                default,
                low,
                targets,
            } => {
                let mut s = format!("switch (s[{}].i) {{", d - 1);
                for (i, t) in targets.iter().enumerate() {
                    let key = *low as i64 + i as i64;
                    let _ = write!(s, " case {}: goto L{t};", int_case(key as i32));
                }
                let _ = write!(s, " default: goto L{default}; }}");
                s
            }
            Op::LookupSwitch { default, pairs } => {
                let mut s = format!("switch (s[{}].i) {{", d - 1);
                for (key, t) in pairs {
                    let _ = write!(s, " case {}: goto L{t};", int_case(*key));
                }
                let _ = write!(s, " default: goto L{default}; }}");
                s
            }
            Op::Return(None) => "return;".into(),
            Op::Return(Some(k)) => {
                let v = format!("s[{}].{}", d - 1, field(*k));
                match sig.ret.code {
                    'Z' => format!("return {v} & 1;"),
                    'B' => format!("return jabs_i2b({v});"),
                    'C' => format!("return jabs_i2c({v});"),
                    'S' => format!("return jabs_i2s({v});"),
                    _ => format!("return {v};"),
                }
            }
            Op::MonitorEnter => format!(
                "if (!s[{r}].a) {npe} jabs_monitor_enter(s[{r}].a);",
                r = d - 1
            ),
            Op::MonitorExit => format!(
                "if (!s[{r}].a) {npe} if (!jabs_monitor_exit(s[{r}].a)) {}",
                throw("java/lang/IllegalMonitorStateException", exit),
                r = d - 1
            ),
            // Every reference is an Object: the cast always succeeds and instanceof is a null test.
            Op::CheckCast { class: target } if target == OBJECT => continue,
            Op::InstanceOf { class: target } if target == OBJECT => {
                format!("s[{r}].i = s[{r}].a != NULL;", r = d - 1)
            }
            Op::CheckCast { class: target } if target.starts_with('[') => format!(
                "if (s[{r}].a && !jabs_is_array_instance(s[{r}].a, \"{}\", {})) {}",
                c_escape(target),
                array_component_record(unit, target),
                throw("java/lang/ClassCastException", exit),
                r = d - 1
            ),
            Op::InstanceOf { class: target } if target.starts_with('[') => format!(
                "s[{r}].i = s[{r}].a ? jabs_is_array_instance(s[{r}].a, \"{}\", {}) : 0;",
                c_escape(target),
                array_component_record(unit, target),
                r = d - 1
            ),
            Op::AThrow => format!(
                "if (!s[{r}].a) {npe} jabs_pending = s[{r}].a; {exit}",
                r = d - 1
            ),
            Op::InvokeStatic { descriptor, .. }
            | Op::InvokeVirtual(MemberRef { descriptor, .. })
            | Op::InvokeSpecial(MemberRef { descriptor, .. })
            | Op::InvokeInterface(MemberRef { descriptor, .. }) => match resolved {
                Some(Resolved::Call(CallKind::ObjectInit)) => {
                    "/* java/lang/Object.<init>()V has no effect */".into()
                }
                Some(Resolved::Call(kind)) => call(kind, descriptor, d, exit),
                _ => unreachable!("calls are resolved during verification"),
            },
            Op::GetStatic {
                name, descriptor, ..
            }
            | Op::PutStatic {
                name, descriptor, ..
            } => {
                let Some(Resolved::Field(declaring)) = resolved else {
                    unreachable!()
                };
                let k = kind_of(parse_type(descriptor, name).unwrap().code).unwrap();
                let guard = init_guard(unit, &class.name, declaring, exit);
                let sym = field_symbol(declaring, name);
                if matches!(insn.op, Op::GetStatic { .. }) {
                    format!("{guard}s[{d}].{} = {sym};", field(k))
                } else {
                    let value = narrow_for_store(descriptor, &format!("s[{}].{}", d - 1, field(k)));
                    format!("{guard}{sym} = {value};")
                }
            }
            Op::New { class: target } => format!(
                "{}s[{d}].a = jabs_new(&{}); if (!s[{d}].a) {exit}",
                init_guard(unit, &class.name, target, exit),
                class_record(target)
            ),
            Op::LdcString(units) => format!(
                "s[{d}].a = {}(); if (!s[{d}].a) {exit}",
                literal_symbol(units)
            ),
            Op::LdcClass(name) => format!(
                "s[{d}].a = jabs_class_object(\"{}\"); if (!s[{d}].a) {exit}",
                c_escape(name)
            ),
            Op::NewArray(element) => {
                let r = d - 1;
                format!(
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof({}), JABS_COMPONENT_PRIMITIVE, NULL, \"[{element}\", NULL); if (!s[{r}].a) {exit}",
                    throw("java/lang/NegativeArraySizeException", exit),
                    element_type(*element)
                )
            }
            Op::ANewArray { class: component } => {
                let r = d - 1;
                // Record the runtime element class so covariant stores can be checked (ArrayStoreException).
                let element = if component.starts_with('[') {
                    component.clone()
                } else {
                    format!("L{component};")
                };
                let primitive_rows = component.len() == 2 && component.starts_with('[');
                let (kind, record) = if component == OBJECT {
                    ("JABS_COMPONENT_ANY", "NULL".to_string())
                } else if primitive_rows {
                    ("JABS_COMPONENT_ARRAY", "NULL".to_string())
                } else if !component.starts_with('[')
                    && unit.get(component).is_some()
                    && unit.chain(component).is_ok()
                {
                    (
                        "JABS_COMPONENT_CLASS",
                        format!("&{}", class_record(component)),
                    )
                } else {
                    ("JABS_COMPONENT_UNCHECKED", "NULL".to_string())
                };
                let component_descriptor = if primitive_rows {
                    format!("\"{component}\"")
                } else {
                    "NULL".to_string()
                };
                format!(
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof(jabs_object *), {kind}, {record}, \"[{}\", {component_descriptor}); if (!s[{r}].a) {exit}",
                    throw("java/lang/NegativeArraySizeException", exit),
                    c_escape(&element)
                )
            }
            Op::ArrayLength => {
                let r = d - 1;
                format!("if (!s[{r}].a) {npe} s[{r}].i = ((jabs_array *)s[{r}].a)->length;")
            }
            Op::ArrayLoad(family) | Op::ArrayStore(family) => {
                let storing = matches!(insn.op, Op::ArrayStore(_));
                let Some(Resolved::Element(element)) = resolved else {
                    unreachable!()
                };
                let (array, index) = if storing {
                    (d - 3, d - 2)
                } else {
                    (d - 2, d - 1)
                };
                let checks = format!(
                    "if (!s[{array}].a) {npe} if ((uint32_t)s[{index}].i >= (uint32_t)((jabs_array *)s[{array}].a)->length) {}",
                    throw("java/lang/ArrayIndexOutOfBoundsException", exit)
                );
                match element {
                    // Statically null: the null check always throws.
                    None => checks,
                    Some(e) => {
                        let slot = format!(
                            "(({} *)jabs_array_data((jabs_array *)s[{array}].a))[s[{index}].i]",
                            element_type(*e)
                        );
                        let k = if *family == 'A' {
                            Kind::A
                        } else if matches!(family, 'B' | 'C' | 'S') {
                            Kind::I
                        } else {
                            kind_of(*family).unwrap()
                        };
                        if storing {
                            let value = format!("s[{}].{}", d - 1, field(k));
                            let narrowed = narrow_for_store(&e.to_string(), &value);
                            let store_check = if *family == 'A' {
                                format!(
                                    "if ({value} && !jabs_array_store_ok((jabs_array *)s[{array}].a, {value})) {} ",
                                    throw("java/lang/ArrayStoreException", exit)
                                )
                            } else {
                                String::new()
                            };
                            format!("{checks} {store_check}{slot} = {narrowed};")
                        } else {
                            format!("{checks} s[{array}].{} = {slot};", field(k))
                        }
                    }
                }
            }
            Op::CheckCast { class: target } => format!(
                "if (s[{r}].a && !jabs_is_instance(s[{r}].a, &{})) {}",
                class_record(target),
                throw("java/lang/ClassCastException", exit),
                r = d - 1
            ),
            Op::InstanceOf { class: target } => {
                format!(
                    "s[{r}].i = s[{r}].a ? jabs_is_instance(s[{r}].a, &{}) : 0;",
                    class_record(target),
                    r = d - 1
                )
            }
            Op::GetField(m) | Op::PutField(m) => {
                let Some(Resolved::Field(declaring)) = resolved else {
                    unreachable!()
                };
                let k = kind_of(parse_type(&m.descriptor, &m.name).unwrap().code).unwrap();
                let obj = object_struct(declaring);
                let member = instance_field(&m.name);
                if matches!(insn.op, Op::GetField(_)) {
                    let r = d - 1;
                    format!(
                        "if (!s[{r}].a) {npe} s[{r}].{} = (({obj} *)s[{r}].a)->{member};",
                        field(k)
                    )
                } else {
                    let (r, v) = (d - 2, d - 1);
                    let value = narrow_for_store(&m.descriptor, &format!("s[{v}].{}", field(k)));
                    format!("if (!s[{r}].a) {npe} (({obj} *)s[{r}].a)->{member} = {value};")
                }
            }
        };
        let _ = writeln!(out, "    {line}");
    }
    // Handler dispatch: test catch types in table order; the matching handler starts with only
    // the exception on the stack. Unmatched exceptions propagate to the caller.
    for (n, chain) in a.dispatch.iter().enumerate() {
        let _ = writeln!(out, "E{n}:;");
        for catch in chain {
            let test = match &catch.class {
                Some(c) => format!("jabs_is_instance(jabs_pending, &{})", class_record(c)),
                None => "1".to_string(),
            };
            let _ = writeln!(
                out,
                "    if ({test}) {{ s[0].a = jabs_pending; jabs_pending = NULL; goto L{}; }}",
                catch.handler
            );
        }
        let _ = writeln!(out, "    {ret_default}");
    }
    let _ = writeln!(out, "}}\n");
    if synchronized {
        emit_synchronized_wrapper(out, class, method, sig);
    }
}

/// Lock the receiver (or, for static methods, the class's monitor), run the body, and unlock on
/// both normal and exceptional completion, as the JVM does for ACC_SYNCHRONIZED methods.
fn emit_synchronized_wrapper(
    out: &mut String,
    class: &ClassFile,
    method: &Member,
    sig: &Signature,
) {
    let is_static = method.access & ACC_STATIC != 0;
    let name = symbol(&class.name, &method.name, &method.descriptor);
    let lock = if is_static {
        // Static methods lock the class. Class objects are not modeled yet, so each class gets a
        // dedicated monitor object; nothing else can lock it until `X.class` exists.
        let monitor = mangled("jcm_", &[&class.name]);
        let _ = writeln!(out, "static jabs_object {monitor};");
        format!("&{monitor}")
    } else {
        "self".to_string()
    };
    let mut args: Vec<String> = Vec::new();
    if !is_static {
        args.push("self".into());
    }
    args.extend((0..sig.params.len()).map(|i| format!("a{i}")));
    let call = format!("{name}__sync({})", args.join(", "));
    let _ = writeln!(out, "{} {{", prototype(&class.name, method, sig));
    let _ = writeln!(out, "    jabs_object *lock = {lock};");
    let _ = writeln!(out, "    jabs_monitor_enter(lock);");
    if sig.ret.code == 'V' {
        let _ = writeln!(out, "    {call};\n    jabs_monitor_exit(lock);\n}}\n");
    } else {
        let _ = writeln!(out, "    {} result = {call};", c_type(sig.ret.code));
        let _ = writeln!(
            out,
            "    jabs_monitor_exit(lock);\n    return result;\n}}\n"
        );
    }
}

/// Switch case label; INT32_MIN needs the macro since `-2147483648` is not an int literal in C.
fn int_case(key: i32) -> String {
    if key == i32::MIN {
        "INT32_MIN".into()
    } else {
        key.to_string()
    }
}

/// Emit a class's object layout: its superclass's struct (or the header) first, then its own instance fields.
fn emit_object_layout(out: &mut String, class: &ClassFile) {
    let base = match class.super_name.as_deref() {
        Some(parent) if parent != OBJECT => format!("{} super;", object_struct(parent)),
        _ => "jabs_object header;".to_string(),
    };
    let _ = writeln!(out, "{} {{\n    {base}", object_struct(&class.name));
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC == 0) {
        match storage_type(&f.descriptor) {
            Some(ty) => {
                let _ = writeln!(out, "    {ty} {};", instance_field(&f.name));
            }
            // Present so the layout is complete; no compiled code can access it yet.
            None => {
                let _ = writeln!(
                    out,
                    "    void *{}; /* {}: not accessible yet */",
                    instance_field(&f.name),
                    f.descriptor
                );
            }
        }
    }
    let _ = writeln!(out, "}};\n");
}

/// Text of a `static final String` field's ConstantValue, if it has one.
fn constant_string(class: &ClassFile, field: &Member) -> Option<Vec<u16>> {
    match field
        .constant_value
        .and_then(|i| class.pool.get(i as usize))
    {
        Some(Constant::String(text)) if field.descriptor == "Ljava/lang/String;" => {
            class.string_units.get(text).cloned()
        }
        _ => None,
    }
}

/// C name of the function returning the (interned) String object for a literal.
fn literal_symbol(units: &[u16]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for u in units {
        hasher.update(u.to_le_bytes());
    }
    format!("jl_{:x}", hasher.finalize())[..27].to_string()
}

/// Emit one function per distinct string literal. Each builds its String once (Latin-1 when every
/// char fits in a byte, else UTF-16 in the target's byte order, as the JDK's compact strings do)
/// and returns the same object afterwards, so equal literals are the same object everywhere.
fn emit_literals(out: &mut String, literals: &BTreeSet<Vec<u16>>) {
    if literals.is_empty() {
        return;
    }
    let string = object_struct(STRING);
    let record = class_record(STRING);
    for units in literals {
        let latin1 = units.iter().all(|&u| u <= 0xFF);
        let bytes = |little: bool| -> String {
            let mut b: Vec<String> = Vec::new();
            for &u in units {
                if latin1 {
                    b.push(format!("{}", u as u8));
                } else if little {
                    b.extend([format!("{}", u & 0xFF), format!("{}", u >> 8)]);
                } else {
                    b.extend([format!("{}", u >> 8), format!("{}", u & 0xFF)]);
                }
            }
            if b.is_empty() {
                "0".to_string()
            } else {
                b.join(",")
            }
        };
        let length = if latin1 { units.len() } else { units.len() * 2 };
        let name = literal_symbol(units);
        let _ = writeln!(out, "jabs_object *{name}(void) {{");
        let _ = writeln!(out, "    static jabs_object *interned;");
        let _ = writeln!(out, "    if (interned) return interned;");
        let _ = writeln!(out, "    if ({}() != 0) return NULL;", init_symbol(STRING));
        if latin1 {
            let _ = writeln!(
                out,
                "    static const unsigned char bytes[] = {{ {} }};",
                bytes(true)
            );
        } else {
            let _ = writeln!(
                out,
                "#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__"
            );
            let _ = writeln!(
                out,
                "    static const unsigned char bytes[] = {{ {} }};",
                bytes(false)
            );
            let _ = writeln!(out, "#else");
            let _ = writeln!(
                out,
                "    static const unsigned char bytes[] = {{ {} }};",
                bytes(true)
            );
            let _ = writeln!(out, "#endif");
        }
        let _ = writeln!(
            out,
            "    jabs_object *value = jabs_new_array({length}, 1, JABS_COMPONENT_PRIMITIVE, NULL, \"[B\", NULL);"
        );
        let _ = writeln!(out, "    if (!value) return NULL;");
        let _ = writeln!(
            out,
            "    memcpy(jabs_array_data((jabs_array *)value), bytes, {length});"
        );
        let _ = writeln!(out, "    jabs_object *s = jabs_new(&{record});");
        let _ = writeln!(out, "    if (!s) return NULL;");
        let _ = writeln!(
            out,
            "    (({string} *)s)->{} = value;",
            instance_field("value")
        );
        let _ = writeln!(
            out,
            "    (({string} *)s)->{} = {};",
            instance_field("coder"),
            if latin1 { 0 } else { 1 }
        );
        let _ = writeln!(out, "    interned = s;");
        let _ = writeln!(out, "    return s;\n}}");
    }
    out.push('\n');
}

/// `&record` of a one-dimensional reference array's element class when the unit has it, else NULL.
fn array_component_record(unit: &Unit, array: &str) -> String {
    match element_class(array) {
        Some(c)
            if c != OBJECT
                && !c.starts_with('[')
                && unit.get(&c).is_some()
                && (unit.is_interface(&c) || unit.chain(&c).is_ok()) =>
        {
            format!("&{}", class_record(&c))
        }
        _ => "NULL".to_string(),
    }
}

/// C name of the stub that stands in for an implementation that is not compiled.
fn stub_symbol(key: &MethodKey) -> String {
    mangled("jx_", &[&key.0, &key.1, &key.2])
}

/// C name of the stub for an interface call with no unique implementation on `class`.
fn error_stub_symbol(class: &str, name: &str, descriptor: &str) -> String {
    mangled("jxe_", &[class, name, descriptor])
}

/// Write one typed stub function with the given body.
fn write_stub(out: &mut String, name: &str, sig: &Signature, body: &str) {
    let unused: String = (0..sig.params.len())
        .map(|i| format!(" (void)a{i};"))
        .collect();
    let _ = writeln!(
        out,
        "static {} {name}({}) {{ (void)self;{unused} {body} }}",
        c_type(sig.ret.code),
        c_params(false, sig, true)
    );
}

/// Emit typed stubs for vtable and interface-table entries without live code. Abstract methods
/// throw AbstractMethodError and ambiguous defaults IncompatibleClassChangeError, as Java does;
/// anything else stops with a diagnostic (the JVM backend must run the profile until that
/// method compiles). Stubs keep each class's code independent of which subclasses exist, so
/// per-class caching stays valid when mods add subclasses.
fn emit_stubs(
    out: &mut String,
    unit: &Unit,
    recorded: &[&ClassFile],
    live: &dyn Fn(&str, &MethodKey) -> bool,
) {
    let mut done = BTreeSet::new();
    let mut stub_for = |out: &mut String, key: &MethodKey| {
        if !done.insert(key.clone()) {
            return;
        }
        let Ok(sig) = parse_signature(&key.2) else {
            return;
        };
        let is_abstract = unit
            .get(&key.0)
            .and_then(|c| {
                c.methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
            })
            .is_some_and(|m| m.access & ACC_ABSTRACT != 0);
        let ret = if sig.ret.code == 'V' {
            "return;"
        } else {
            "return 0;"
        };
        let body = if is_abstract {
            format!("{} {ret}", raise("java/lang/AbstractMethodError"))
        } else {
            format!(
                "jabs_not_compiled(\"{}\");",
                c_escape(&format!("{}.{}{}", key.0, key.1, key.2))
            )
        };
        write_stub(out, &stub_symbol(key), &sig, &body);
    };
    for class in recorded {
        for slot in unit
            .vtable(&class.name)
            .map(|v| (*v).clone())
            .unwrap_or_default()
        {
            let key = (
                slot.owner.clone(),
                slot.name.clone(),
                slot.descriptor.clone(),
            );
            if !live(&class.name, &key) {
                stub_for(out, &key);
            }
        }
        for entry in unit.itable(&class.name).iter() {
            let Ok(sig) = parse_signature(&entry.descriptor) else {
                continue;
            };
            let ret = if sig.ret.code == 'V' {
                "return;"
            } else {
                "return 0;"
            };
            match &entry.target {
                ItableTarget::Impl(key) if !live(&class.name, key) => stub_for(out, key),
                ItableTarget::Impl(_) => {}
                ItableTarget::Abstract | ItableTarget::Ambiguous => {
                    let error = raise(if matches!(entry.target, ItableTarget::Abstract) {
                        "java/lang/AbstractMethodError"
                    } else {
                        "java/lang/IncompatibleClassChangeError"
                    });
                    let name = error_stub_symbol(&class.name, &entry.name, &entry.descriptor);
                    write_stub(out, &name, &sig, &format!("{error} {ret}"));
                }
            }
        }
    }
    out.push('\n');
}

/// Emit the runtime record of an interface (used by instanceof, checkcast and array stores).
fn emit_interface_record(out: &mut String, iface: &ClassFile) {
    let _ = writeln!(
        out,
        "const jabs_class {} = {{ \"{}\", 0, NULL, NULL, NULL, 0, NULL, 0, 1, {} }};",
        class_record(&iface.name),
        c_escape(&iface.name),
        iface.access
    );
}

/// Emit a class's vtable, interface table, interface list and runtime record; entries
/// without live code point at stubs.
fn emit_class_record(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    live: &dyn Fn(&str, &MethodKey) -> bool,
) {
    let function = |key: &MethodKey| -> Option<String> {
        if live(&class.name, key) {
            Some(format!(
                "(void (*)(void)){}",
                symbol(&key.0, &key.1, &key.2)
            ))
        } else if parse_signature(&key.2).is_ok() {
            Some(format!("(void (*)(void)){}", stub_symbol(key)))
        } else {
            None
        }
    };
    let vtable_name = mangled("jv_", &[&class.name]);
    let slots = unit
        .vtable(&class.name)
        .map(|v| (*v).clone())
        .unwrap_or_default();
    let entries: Vec<String> = slots
        .iter()
        .map(|s| {
            function(&(s.owner.clone(), s.name.clone(), s.descriptor.clone()))
                .unwrap_or_else(|| "0".into())
        })
        .collect();
    let entries = if entries.is_empty() {
        "0".to_string()
    } else {
        entries.join(", ")
    };
    // External linkage: records are referenced across the unit and must not trip unused warnings.
    let _ = writeln!(
        out,
        "void (*const {vtable_name}[])(void) = {{ {entries} }};"
    );

    let itable: Vec<String> = unit
        .itable(&class.name)
        .iter()
        .filter_map(|e| {
            let f = match &e.target {
                ItableTarget::Impl(key) => function(key)?,
                _ if parse_signature(&e.descriptor).is_ok() => {
                    format!(
                        "(void (*)(void)){}",
                        error_stub_symbol(&class.name, &e.name, &e.descriptor)
                    )
                }
                _ => return None,
            };
            Some(format!("{{ UINT64_C(0x{:016x}), {f} }}", e.selector))
        })
        .collect();
    let itable_name = mangled("ji_", &[&class.name]);
    let (itable_ref, itable_len) = if itable.is_empty() {
        ("NULL".to_string(), 0)
    } else {
        let _ = writeln!(
            out,
            "static const jabs_itable_entry {itable_name}[] = {{ {} }};",
            itable.join(", ")
        );
        (itable_name, itable.len())
    };
    let ifaces: Vec<String> = unit
        .superinterfaces(&class.name)
        .unwrap_or_default()
        .iter()
        .map(|i| format!("&{}", class_record(&i.name)))
        .collect();
    let ifaces_name = mangled("jn_", &[&class.name]);
    let (ifaces_ref, ifaces_len) = if ifaces.is_empty() {
        ("NULL".to_string(), 0)
    } else {
        let _ = writeln!(
            out,
            "static const jabs_class *const {ifaces_name}[] = {{ {} }};",
            ifaces.join(", ")
        );
        (ifaces_name, ifaces.len())
    };
    let parent = match class.super_name.as_deref() {
        Some(p) if p != OBJECT => format!("&{}", class_record(p)),
        _ => "NULL".to_string(),
    };
    let _ = writeln!(
        out,
        "const jabs_class {} = {{ \"{}\", sizeof({}), {vtable_name}, {parent}, {itable_ref}, {itable_len}, {ifaces_ref}, {ifaces_len}, 0, {} }};\n",
        class_record(&class.name),
        c_escape(&class.name),
        object_struct(&class.name),
        class.access
    );
}

/// Define the runtime's exception hooks: VM exceptions are instances of the JDK classes when
/// their records exist, else of name-only records (catchable only by name-free paths).
fn emit_vm_throwables(out: &mut String, recorded: &BTreeSet<&str>) {
    let mut records = Vec::new();
    for (name, _) in VM_EXCEPTIONS {
        if recorded.contains(name) {
            records.push(format!("&{}", class_record(name)));
        } else {
            let synthetic = mangled("jabs_vmx_", &[name]);
            let _ = writeln!(
                out,
                "const jabs_class {synthetic} = {{ \"{name}\", 0, NULL, NULL, NULL, 0, NULL, 0, 0, 0 }};"
            );
            records.push(format!("&{synthetic}"));
        }
    }
    let _ = writeln!(out, "jabs_object *jabs_vm_throwable(int kind) {{");
    let _ = writeln!(
        out,
        "    static const jabs_class *const records[] = {{ {} }};",
        records.join(", ")
    );
    let _ = writeln!(out, "    const jabs_class *c = records[kind];");
    let _ = writeln!(
        out,
        "    jabs_object *o = (jabs_object *)calloc(1, c->size > sizeof(jabs_object) ? c->size : sizeof(jabs_object));"
    );
    let _ = writeln!(
        out,
        "    if (!o) {{ fputs(\"jabs: out of memory while raising an exception\\n\", stderr); abort(); }}"
    );
    let _ = writeln!(out, "    o->cls = c;\n    return o;\n}}");
    let error_test = if recorded.contains("java/lang/Error") {
        format!("jabs_is_instance(o, &{})", class_record("java/lang/Error"))
    } else {
        "strstr(o->cls->name, \"Error\") != NULL".to_string()
    };
    let _ = writeln!(
        out,
        "int jabs_is_error(const jabs_object *o) {{ return {error_test}; }}\n"
    );
    // Throwable.cause, so ExceptionInInitializerError.getCause() returns the original exception.
    if recorded.contains(THROWABLE) {
        let field = format!(
            "(({} *)o)->{}",
            object_struct(THROWABLE),
            instance_field("cause")
        );
        let _ = writeln!(
            out,
            "void jabs_set_cause(jabs_object *o, jabs_object *cause) {{ {field} = cause; }}"
        );
        let _ = writeln!(
            out,
            "jabs_object *jabs_cause_of(jabs_object *o) {{ return {field} == o ? NULL : {field}; }}\n"
        );
    } else {
        let _ = writeln!(
            out,
            "void jabs_set_cause(jabs_object *o, jabs_object *cause) {{ (void)o; (void)cause; }}"
        );
        let _ = writeln!(
            out,
            "jabs_object *jabs_cause_of(jabs_object *o) {{ (void)o; return NULL; }}\n"
        );
    }
}

/// Emit the Class-object registry: one java.lang.Class per name, created on first use with the
/// fields the JVM itself fills in (name, componentType, modifiers, primitive). Class loaders are
/// not modeled: every class reports the boot loader (null).
fn emit_class_registry(out: &mut String, unit: &Unit, recorded: &[&ClassFile]) {
    let class_struct = object_struct(CLASS);
    let string_struct = object_struct(STRING);
    let records: Vec<String> = recorded
        .iter()
        .map(|c| format!("&{}", class_record(&c.name)))
        .collect();
    let _ = writeln!(
        out,
        "static const jabs_class *const jabs_records[] = {{ {}NULL }};",
        records.iter().map(|r| format!("{r}, ")).collect::<String>()
    );
    // Field offsets of every recorded class's own instance fields, for Unsafe.objectFieldOffset.
    let _ = writeln!(
        out,
        "typedef struct {{ const char *class_name, *field_name; size_t offset; }} jabs_field_offset_entry;"
    );
    let _ = writeln!(
        out,
        "static const jabs_field_offset_entry jabs_field_offsets[] = {{"
    );
    for class in recorded {
        for f in class.fields.iter().filter(|f| f.access & ACC_STATIC == 0) {
            let _ = writeln!(
                out,
                "    {{ \"{}\", \"{}\", offsetof({}, {}) }},",
                c_escape(&class.name),
                c_escape(&f.name),
                object_struct(&class.name),
                instance_field(&f.name)
            );
        }
    }
    let _ = writeln!(out, "    {{ NULL, NULL, 0 }}\n}};");
    out.push_str(&format!(r#"
typedef struct {{ char *name; const jabs_class *record; jabs_object *object; }} jabs_class_entry;
static jabs_class_entry *jabs_class_entries;
static size_t jabs_class_count, jabs_class_capacity;

/* Record for an internal class name, if the unit compiled one. */
static const jabs_class *jabs_record_named(const char *name) {{
    for (const jabs_class *const *r = jabs_records; *r; r++) if (strcmp((*r)->name, name) == 0) return *r;
    return NULL;
}}

/* Build a String from UTF-8 text (compact Latin-1 when every char fits, else UTF-16). */
static jabs_object *jabs_string_from_utf8(const char *text) {{
    size_t n = strlen(text), count = 0;
    uint16_t *units = (uint16_t *)malloc((n + 1) * sizeof(uint16_t));
    if (!units) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    int latin1 = 1;
    for (size_t i = 0; i < n;) {{
        unsigned char b = (unsigned char)text[i];
        uint32_t cp = b; size_t len = 1;
        if (b >= 0xF0 && i + 3 < n + 1) {{ cp = ((b & 7u) << 18) | ((text[i+1] & 0x3Fu) << 12) | ((text[i+2] & 0x3Fu) << 6) | (text[i+3] & 0x3Fu); len = 4; }}
        else if (b >= 0xE0) {{ cp = ((b & 15u) << 12) | ((text[i+1] & 0x3Fu) << 6) | (text[i+2] & 0x3Fu); len = 3; }}
        else if (b >= 0xC0) {{ cp = ((b & 31u) << 6) | (text[i+1] & 0x3Fu); len = 2; }}
        if (cp > 0xFFFF) {{ cp -= 0x10000; units[count++] = (uint16_t)(0xD800 + (cp >> 10)); units[count++] = (uint16_t)(0xDC00 + (cp & 0x3FF)); latin1 = 0; }}
        else {{ units[count++] = (uint16_t)cp; if (cp > 0xFF) latin1 = 0; }}
        i += len;
    }}
    size_t bytes = latin1 ? count : count * 2;
    jabs_object *value = jabs_new_array((int32_t)bytes, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!value) {{ free(units); return NULL; }}
    unsigned char *data = (unsigned char *)jabs_array_data((jabs_array *)value);
    for (size_t i = 0; i < count; i++) {{
        if (latin1) data[i] = (unsigned char)units[i];
        else memcpy(data + 2 * i, &units[i], 2); /* UTF-16 in the target's byte order */
    }}
    free(units);
    if ({string_init}() != 0) return NULL;
    jabs_object *s = jabs_new(&{string_record});
    if (!s) return NULL;
    (({string_struct} *)s)->{value} = value;
    (({string_struct} *)s)->{coder} = latin1 ? 0 : 1;
    return s;
}}

/* Copy an all-ASCII Latin-1 String into `out`; 0 if it is not one or does not fit. */
int jabs_string_to_ascii(jabs_object *s, char *out, size_t size) {{
    jabs_array *v = (jabs_array *)(({string_struct} *)s)->{value};
    if ((({string_struct} *)s)->{coder} != 0 || !v || (size_t)v->length >= size) return 0;
    memcpy(out, jabs_array_data(v), (size_t)v->length);
    out[v->length] = 0;
    return 1;
}}

static int jabs_is_primitive_name(const char *n) {{
    static const char *const names[] = {{ "boolean", "byte", "char", "short", "int", "long", "float", "double", "void" }};
    for (size_t i = 0; i < sizeof names / sizeof names[0]; i++) if (strcmp(n, names[i]) == 0) return 1;
    return 0;
}}

/* The Class object for an internal name ("java/lang/String"), array descriptor ("[I") or
 * primitive name ("int"). Created once; the same object is returned afterwards. */
jabs_object *jabs_class_object(const char *name) {{
    for (size_t i = 0; i < jabs_class_count; i++) if (strcmp(jabs_class_entries[i].name, name) == 0) return jabs_class_entries[i].object;
    if ({class_init}() != 0) return NULL;
    jabs_object *c = jabs_new(&{class_record_name});
    if (!c) return NULL;
    char *binary = (char *)malloc(strlen(name) + 1);
    if (!binary) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    for (size_t i = 0; ; i++) {{ binary[i] = name[i] == '/' ? '.' : name[i]; if (!name[i]) break; }}
    jabs_object *binary_name = jabs_string_from_utf8(binary);
    free(binary);
    if (!binary_name) return NULL;
    const jabs_class *record = name[0] == '[' ? NULL : jabs_record_named(name);
    (({class_struct} *)c)->{name_field} = binary_name;
    if (jabs_is_primitive_name(name)) {{
        (({class_struct} *)c)->{primitive} = 1;
        (({class_struct} *)c)->{modifiers} = 0x411; /* public final abstract */
    }} else if (name[0] == '[') {{
        (({class_struct} *)c)->{modifiers} = 0x411;
    }} else if (record) {{
        (({class_struct} *)c)->{modifiers} = (uint16_t)(record->access & 0x7631);
    }}
    if (jabs_class_count == jabs_class_capacity) {{
        size_t grown = jabs_class_capacity ? jabs_class_capacity * 2 : 64;
        jabs_class_entry *more = (jabs_class_entry *)realloc(jabs_class_entries, grown * sizeof *more);
        if (!more) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
        jabs_class_entries = more;
        jabs_class_capacity = grown;
    }}
    char *copy = (char *)malloc(strlen(name) + 1);
    if (!copy) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    strcpy(copy, name);
    jabs_class_entries[jabs_class_count++] = (jabs_class_entry){{ copy, record, c }};
    if (name[0] == '[') {{
        /* componentType: "[I" -> int, "[Ljava/lang/String;" -> java/lang/String, "[[I" -> [I. */
        char element[512];
        static const char *const prims[] = {{ "Z", "boolean", "B", "byte", "C", "char", "S", "short", "I", "int", "J", "long", "F", "float", "D", "double" }};
        const char *e = name + 1;
        if (*e == 'L') {{ size_t len = strlen(e) - 2; if (len >= sizeof element) return c; memcpy(element, e + 1, len); element[len] = 0; }}
        else if (*e == '[') {{ if (strlen(e) >= sizeof element) return c; strcpy(element, e); }}
        else {{ for (size_t i = 0; i < 16; i += 2) if (*e == prims[i][0]) strcpy(element, prims[i + 1]); }}
        jabs_object *component = jabs_class_object(element);
        if (!component) return NULL;
        (({class_struct} *)c)->{component} = component;
    }}
    return c;
}}

/* Registry entry of a Class object (every Class object comes from jabs_class_object). */
static jabs_class_entry *jabs_class_entry_of(jabs_object *c) {{
    for (size_t i = 0; i < jabs_class_count; i++) if (jabs_class_entries[i].object == c) return &jabs_class_entries[i];
    return NULL;
}}

/* Object.getClass. */
jabs_object *jabs_class_of(jabs_object *o) {{
    return jabs_class_object(o->cls == jabs_array_class() ? ((jabs_array *)o)->descriptor : o->cls->name);
}}

/* Class.isInstance(o) for non-null o. */
int32_t jabs_class_is_instance(jabs_object *c, jabs_object *o) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || jabs_is_primitive_name(e->name)) return 0;
    if (strcmp(e->name, "java/lang/Object") == 0) return 1;
    if (e->name[0] == '[') {{
        const jabs_class *component = NULL;
        if (e->name[1] == 'L') {{
            char element[512]; size_t len = strlen(e->name) - 3;
            if (len < sizeof element) {{ memcpy(element, e->name + 2, len); element[len] = 0; component = jabs_record_named(element); }}
        }}
        return jabs_is_array_instance(o, e->name, component);
    }}
    if (e->record) return jabs_is_instance(o, e->record);
    jabs_not_compiled("Class.isInstance for a class outside the unit");
}}

/* Class.isAssignableFrom(other) for non-null other. */
int32_t jabs_class_assignable_from(jabs_object *c, jabs_object *other) {{
    if (c == other) return 1;
    jabs_class_entry *a = jabs_class_entry_of(c), *b = jabs_class_entry_of(other);
    if (!a || !b || jabs_is_primitive_name(a->name) || jabs_is_primitive_name(b->name)) return 0;
    if (strcmp(a->name, "java/lang/Object") == 0) return 1;
    if (b->name[0] == '[') {{
        if (a->name[0] != '[') return strcmp(a->name, "java/lang/Cloneable") == 0 || strcmp(a->name, "java/io/Serializable") == 0;
        jabs_not_compiled("Class.isAssignableFrom between two array types");
    }}
    if (a->name[0] == '[') return 0;
    if (a->record && b->record) return jabs_class_assignable(b->record, a->record);
    jabs_not_compiled("Class.isAssignableFrom for a class outside the unit");
}}

/* Unsafe.arrayIndexScale0: element size of an array class. */
int32_t jabs_array_scale(jabs_object *c) {{
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e || e->name[0] != '[') {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    switch (e->name[1]) {{
    case 'Z': case 'B': return 1;
    case 'C': case 'S': return 2;
    case 'I': case 'F': return 4;
    case 'J': case 'D': return 8;
    default: return (int32_t)sizeof(jabs_object *);
    }}
}}

/* Unsafe.objectFieldOffset1: byte offset of a declared instance field in our object layout. */
int64_t jabs_field_offset(jabs_object *c, jabs_object *name) {{
    char field[256];
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e || !name || !jabs_string_to_ascii(name, field, sizeof field)) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    for (const jabs_field_offset_entry *f = jabs_field_offsets; f->class_name; f++) {{
        if (strcmp(f->class_name, e->name) == 0 && strcmp(f->field_name, field) == 0) return (int64_t)f->offset;
    }}
    jabs_not_compiled("Unsafe.objectFieldOffset for a field without a compiled layout");
}}

/* Class.getSuperclass: null for Object, interfaces and primitives; Object for arrays. */
jabs_object *jabs_class_superclass(jabs_object *c) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || jabs_is_primitive_name(e->name) || strcmp(e->name, "java/lang/Object") == 0) return NULL;
    if (e->name[0] == '[') return jabs_class_object("java/lang/Object");
    if (!e->record) jabs_not_compiled("Class.getSuperclass for a class outside the unit");
    if (e->record->is_interface) return NULL;
    return jabs_class_object(e->record->super ? e->record->super->name : "java/lang/Object");
}}
"#,
        string_init = init_symbol(STRING),
        string_record = class_record(STRING),
        class_init = init_symbol(CLASS),
        class_record_name = class_record(CLASS),
        value = instance_field("value"),
        coder = instance_field("coder"),
        name_field = instance_field("name"),
        primitive = instance_field("primitive"),
        modifiers = instance_field("modifiers"),
        component = instance_field("componentType"),
    ));
    out.push_str(&main_thread_function(unit, recorded));
    out.push('\n');
}

/// `jabs_main_thread()`: the main Thread the VM would create at startup (thread groups "system"
/// and "main", priority 5, not a daemon, RUNNABLE), built once without running constructors.
/// Only fields the JDK's classes declare are set.
fn main_thread_function(unit: &Unit, recorded: &[&ClassFile]) -> String {
    let (thread, holder, group) = (
        "java/lang/Thread",
        "java/lang/Thread$FieldHolder",
        "java/lang/ThreadGroup",
    );
    let have = |c: &str| recorded.iter().any(|r| r.name == c);
    if ![thread, holder, group, OBJECT].iter().all(|c| have(c)) {
        return "jabs_object *jabs_main_thread(void) { jabs_not_compiled(\"Thread.currentThread (thread classes are not in the unit)\"); }\n".to_string();
    }
    // `object.field = value` if the class declares that instance field with that descriptor.
    let set = |class: &str, object: &str, name: &str, descriptor: &str, value: &str| -> String {
        let declared = unit.get(class).is_some_and(|c| {
            c.fields
                .iter()
                .any(|f| f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0)
        });
        if declared {
            format!(
                "    (({} *){object})->{} = {value};\n",
                object_struct(class),
                instance_field(name)
            )
        } else {
            format!("    /* {class} has no {name}:{descriptor} */\n")
        }
    };
    let mut body = String::from(
        "jabs_object *jabs_main_thread(void) {\n    static jabs_object *main_thread;\n    if (main_thread) return main_thread;\n",
    );
    let _ = writeln!(body, "    if ({}() != 0) return NULL;", init_symbol(thread));
    let _ = writeln!(
        body,
        "    jabs_object *system = jabs_new(&{g}), *group = jabs_new(&{g}), *holder = jabs_new(&{h}), *t = jabs_new(&{t}), *lock = jabs_new(&{o});",
        g = class_record(group),
        h = class_record(holder),
        t = class_record(thread),
        o = class_record(OBJECT)
    );
    body.push_str("    jabs_object *system_name = jabs_string_from_utf8(\"system\"), *main_name = jabs_string_from_utf8(\"main\");\n");
    body.push_str("    if (!system || !group || !holder || !t || !lock || !system_name || !main_name) return NULL;\n");
    body.push_str(&set(
        group,
        "system",
        "name",
        "Ljava/lang/String;",
        "system_name",
    ));
    body.push_str(&set(group, "system", "maxPriority", "I", "10"));
    body.push_str(&set(
        group,
        "group",
        "parent",
        "Ljava/lang/ThreadGroup;",
        "system",
    ));
    body.push_str(&set(
        group,
        "group",
        "name",
        "Ljava/lang/String;",
        "main_name",
    ));
    body.push_str(&set(group, "group", "maxPriority", "I", "10"));
    body.push_str(&set(
        holder,
        "holder",
        "group",
        "Ljava/lang/ThreadGroup;",
        "group",
    ));
    body.push_str(&set(holder, "holder", "priority", "I", "5"));
    body.push_str(&set(holder, "holder", "daemon", "Z", "0"));
    body.push_str(&set(
        holder,
        "holder",
        "threadStatus",
        "I",
        "5 /* JVMTI alive | runnable */",
    ));
    body.push_str(&set(thread, "t", "tid", "J", "1"));
    body.push_str(&set(thread, "t", "name", "Ljava/lang/String;", "main_name"));
    body.push_str(&set(
        thread,
        "t",
        "holder",
        "Ljava/lang/Thread$FieldHolder;",
        "holder",
    ));
    body.push_str(&set(
        thread,
        "t",
        "interruptLock",
        "Ljava/lang/Object;",
        "lock",
    ));
    body.push_str("    main_thread = t;\n    return t;\n}\n");
    body
}

/// Classes whose initialization must precede this one's: the superclass, then (for classes)
/// every superinterface that declares a non-abstract instance method.
fn init_prerequisites(unit: &Unit, class: &ClassFile) -> Vec<String> {
    if class.access & ACC_INTERFACE != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(parent) = class.super_name.as_deref().filter(|p| *p != OBJECT) {
        out.push(parent.to_string());
    }
    for iface in unit.superinterfaces(&class.name).unwrap_or_default() {
        if iface
            .methods
            .iter()
            .any(|m| m.access & (ACC_STATIC | ACC_ABSTRACT) == 0)
        {
            out.push(iface.name.clone());
        }
    }
    out
}

/// Emit a class's static field storage and initialization function (JVMS 5.5, single-threaded).
fn emit_statics_and_init(out: &mut String, unit: &Unit, class: &ClassFile) {
    let init = init_symbol(&class.name);
    let state = format!("{init}_state");
    let mut constants = String::new();
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
        let Some(ty) = storage_type(&f.descriptor) else {
            continue;
        };
        let sym = field_symbol(&class.name, &f.name);
        // External linkage: fields only read through inlined constants must not trip unused warnings.
        let _ = writeln!(out, "{ty} {sym};");
        // ConstantValue fields are set by the initialization procedure itself, before <clinit> runs.
        let value = match f.constant_value.and_then(|i| class.pool.get(i as usize)) {
            Some(Constant::Integer(v)) => Some(Value::I(*v)),
            Some(Constant::Long(v)) => Some(Value::J(*v)),
            Some(Constant::Float(v)) => Some(Value::F(*v)),
            Some(Constant::Double(v)) => Some(Value::D(*v)),
            _ => None,
        };
        if let Some(value) = value {
            let literal = narrow_for_store(&f.descriptor, &value_literal(value));
            let _ = writeln!(constants, "    {sym} = {literal};");
        } else if let Some(units) = constant_string(class, f) {
            let _ = writeln!(
                constants,
                "    {sym} = {}(); if (!{sym}) {{ {state} = 3; return -1; }}",
                literal_symbol(&units)
            );
        }
    }
    let _ = writeln!(
        out,
        "\n/* 0 = uninitialized, 1 = initializing, 2 = initialized, 3 = erroneous. */"
    );
    let _ = writeln!(out, "static int {state};");
    let _ = writeln!(out, "int {init}(void) {{");
    let _ = writeln!(
        out,
        "    /* State 1 can only mean this thread is mid-initialization (no other threads exist yet). */"
    );
    let _ = writeln!(out, "    if ({state} == 1 || {state} == 2) return 0;");
    let _ = writeln!(
        out,
        "    if ({state} == 3) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NO_CLASS_DEF_FOUND); return -1; }}"
    );
    let _ = writeln!(out, "    {state} = 1;");
    // Superclass first, then superinterfaces with default methods (JVMS 5.5 step 7).
    for prerequisite in init_prerequisites(unit, class) {
        let _ = writeln!(
            out,
            "    if ({}() != 0) {{ {state} = 3; return -1; }}",
            init_symbol(&prerequisite)
        );
    }
    out.push_str(&constants);
    if class.methods.iter().any(|m| m.name == "<clinit>") {
        let _ = writeln!(out, "    {}();", symbol(&class.name, "<clinit>", "()V"));
        let _ = writeln!(
            out,
            "    if (jabs_pending) {{ jabs_pending = jabs_initializer_failure(jabs_pending); {state} = 3; return -1; }}"
        );
    }
    let _ = writeln!(out, "    {state} = 2;");
    let _ = writeln!(out, "    return 0;\n}}\n");
}

/// Why a class's hierarchy keeps it from being initialized, or None if it is complete in the unit.
fn structural_problem(unit: &Unit, class: &ClassFile) -> Option<String> {
    if let Err(e) = unit.chain(&class.name) {
        return Some(format!("class hierarchy incomplete: {e}"));
    }
    if let Err(e) = unit.superinterfaces(&class.name) {
        return Some(format!("class hierarchy incomplete: {e}"));
    }
    None
}

/// Classes whose init function emitted code may call: the methods' own classes, their init
/// dependencies, and the prerequisites of every class initialized.
fn emitted_init_targets(
    unit: &Unit,
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = emission.inits.clone();
    // Instance methods assume their class is initialized; runtime-created objects need the init.
    out.extend(emission.methods.iter().map(|k| k.0.clone()));
    for key in &emission.methods {
        if let Some(Ok((_, a))) = results.get(key) {
            out.extend(a.deps.iter().filter_map(|d| match d {
                Dep::Init(c) => Some(c.clone()),
                Dep::Method(_) => None,
            }));
        }
    }
    for class in emission.inits.iter().filter_map(|c| unit.get(c)) {
        out.extend(init_prerequisites(unit, class));
    }
    out
}

/// Emit stand-ins for what emitted code may call but the unit could not compile: a function with
/// the callee's exact signature (under its real symbol) or a class init function, each stopping
/// with the reason; plus static field storage for classes whose initialization is a stub, so
/// field references still link (the guard stops before they are touched).
/// Methods emitted code calls directly that are not emitted themselves (they get stubs).
fn reached_stub_targets(
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
) -> BTreeSet<MethodKey> {
    let mut calls = BTreeSet::new();
    for key in &emission.methods {
        let Some(Ok((_, a))) = results.get(key) else {
            continue;
        };
        for resolved in a.resolved.values() {
            if let Resolved::Call(CallKind::Direct(target)) = resolved
                && !emission.methods.contains(target)
            {
                calls.insert(target.clone());
            }
        }
    }
    calls
}

fn emit_reached_stubs(
    out: &mut String,
    unit: &Unit,
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
    init_ok: &HashMap<String, std::result::Result<(), String>>,
) {
    let mut calls: BTreeSet<MethodKey> = BTreeSet::new();
    let mut statics: BTreeSet<String> = BTreeSet::new();
    for key in &emission.methods {
        let Some(Ok((_, a))) = results.get(key) else {
            continue;
        };
        for (pc, resolved) in &a.resolved {
            match resolved {
                Resolved::Call(CallKind::Direct(target)) if !emission.methods.contains(target) => {
                    calls.insert(target.clone());
                }
                Resolved::Field(declaring) if !emission.inits.contains(declaring) => {
                    let is_static = a.insns.iter().any(|i| {
                        i.pc == *pc && matches!(i.op, Op::GetStatic { .. } | Op::PutStatic { .. })
                    });
                    if is_static {
                        statics.insert(declaring.clone());
                    }
                }
                _ => {}
            }
        }
    }
    for target in &calls {
        let Some(class) = unit.get(&target.0) else {
            continue;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| m.name == target.1 && m.descriptor == target.2)
        else {
            continue;
        };
        let Ok(sig) = parse_signature(&target.2) else {
            continue;
        };
        let reason = match results.get(target) {
            Some(Err(reason)) => reason.clone(),
            _ => "not reached by reachability analysis".to_string(),
        };
        let mut unused = String::new();
        if method.access & ACC_STATIC == 0 {
            unused.push_str(" (void)self;");
        }
        for i in 0..sig.params.len() {
            let _ = write!(unused, " (void)a{i};");
        }
        let message = c_escape(&format!("{}.{}{} ({reason})", target.0, target.1, target.2));
        let _ = writeln!(
            out,
            "{} {{{unused} jabs_not_compiled(\"{message}\"); }}",
            prototype(&target.0, method, &sig)
        );
    }
    for class in emitted_init_targets(unit, results, emission).difference(&emission.inits) {
        let reason = match init_ok.get(class) {
            Some(Err(reason)) => reason.clone(),
            _ => "not in the compilation unit".to_string(),
        };
        let message = c_escape(&format!(
            "class {class} cannot be initialized natively ({reason})"
        ));
        let _ = writeln!(
            out,
            "int {}(void) {{ jabs_not_compiled(\"{message}\"); }}",
            init_symbol(class)
        );
    }
    for class in statics.iter().filter_map(|c| unit.get(c)) {
        for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
            if let Some(ty) = storage_type(&f.descriptor) {
                let _ = writeln!(out, "{ty} {};", field_symbol(&class.name, &f.name));
            }
        }
    }
    out.push('\n');
}

/// Compiled methods that never reach a stub: every method they may call directly and every class
/// they may initialize (with its whole initializer) is native too. Greatest fixpoint: start from
/// everything compiled and remove methods with a missing dependency until nothing changes.
fn self_contained_methods(
    unit: &Unit,
    classes: &[ClassFile],
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    initializable: &BTreeSet<String>,
) -> BTreeSet<MethodKey> {
    // Nodes are methods and class initializations; an edge points from a dependency to what
    // depends on it. Everything not compiled (or not initializable) is bad, and badness flows to
    // dependents. One pass over the reverse graph: linear in the number of dependencies.
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum Node {
        Method(MethodKey),
        Class(String),
    }
    let mut dependents: BTreeMap<Node, Vec<Node>> = BTreeMap::new();
    let mut edge = |from: Node, to: Node| dependents.entry(from).or_default().push(to);
    for (key, result) in results {
        let Ok((_, a)) = result else { continue };
        let me = Node::Method(key.clone());
        if key.1 != "<clinit>" {
            edge(Node::Class(key.0.clone()), me.clone());
        }
        for dep in &a.deps {
            match dep {
                Dep::Method(m) => edge(Node::Method(m.clone()), me.clone()),
                Dep::Init(c) => edge(Node::Class(c.clone()), me.clone()),
            }
        }
    }
    for class in classes {
        let me = Node::Class(class.name.clone());
        let clinit = (
            class.name.clone(),
            "<clinit>".to_string(),
            "()V".to_string(),
        );
        if results.contains_key(&clinit) {
            edge(Node::Method(clinit), me.clone());
        }
        for p in init_prerequisites(unit, class) {
            edge(Node::Class(p), me.clone());
        }
    }
    let mut bad: BTreeSet<Node> = BTreeSet::new();
    let mut work: Vec<Node> = Vec::new();
    // Seeds: methods that did not compile, called methods that do not exist, classes that
    // cannot be initialized (including ones outside the unit).
    for node in dependents.keys() {
        let seed = match node {
            Node::Method(m) => !matches!(results.get(m), Some(Ok(_))),
            Node::Class(c) => !initializable.contains(c),
        };
        if seed {
            work.push(node.clone());
        }
    }
    while let Some(node) = work.pop() {
        if !bad.insert(node.clone()) {
            continue;
        }
        if let Some(next) = dependents.get(&node) {
            work.extend(next.iter().filter(|n| !bad.contains(n)).cloned());
        }
    }
    results
        .iter()
        .filter(|(k, r)| r.is_ok() && !bad.contains(&Node::Method((*k).clone())))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Compile a set of classes together into one C unit.
pub fn compile_unit(classes: &[ClassFile]) -> CompiledUnit {
    compile_unit_rooted(classes, None)
}

/// What emission needs: the methods to emit, the classes needing a runtime record, the classes
/// that are instantiated (whose vtables must hold real code), and the classes to initialize.
struct Emission {
    methods: BTreeSet<MethodKey>,
    records: BTreeSet<String>,
    instantiated: BTreeSet<String>,
    inits: BTreeSet<String>,
}

/// Everything reachable from the root classes. `new K` makes K's compiled vtable implementations
/// reachable, since virtual calls may now dispatch to them; vtables of classes never
/// instantiated hold stubs, which no call can reach because no such object exists.
fn reachable(
    unit: &Unit,
    classes: &[ClassFile],
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    intrinsics: &HashMap<MethodKey, (String, bool)>,
    initializable: &BTreeSet<String>,
    roots: &BTreeSet<String>,
) -> Emission {
    let compiled = |k: &MethodKey| matches!(results.get(k), Some(Ok(_)));
    let mut out = Emission {
        methods: BTreeSet::new(),
        records: BTreeSet::new(),
        instantiated: BTreeSet::new(),
        inits: BTreeSet::new(),
    };
    let mut work: Vec<MethodKey> = Vec::new();
    let mut pending_inits: Vec<String> = Vec::new();
    for class in classes.iter().filter(|c| roots.contains(&c.name)) {
        pending_inits.push(class.name.clone());
        for m in &class.methods {
            let key = (class.name.clone(), m.name.clone(), m.descriptor.clone());
            if compiled(&key) {
                work.push(key);
            }
        }
    }
    // A record for `class` also needs records for its superclasses and superinterfaces.
    let add_records = |name: &str, records: &mut BTreeSet<String>| {
        // The class itself (Object's own chain is empty), then its superclasses.
        if unit.get(name).is_some() {
            records.insert(name.to_string());
        }
        if let Ok(chain) = unit.chain(name) {
            for c in chain.iter() {
                records.insert(c.name.clone());
            }
        }
        for i in unit.superinterfaces(name).unwrap_or_default() {
            records.insert(i.name.clone());
        }
    };
    // Objects of `class` now exist: their vtable and interface-table implementations are reachable.
    let implementations = |class: &str| -> Vec<MethodKey> {
        let mut keys: Vec<MethodKey> = unit
            .vtable(class)
            .map(|v| (*v).clone())
            .unwrap_or_default()
            .into_iter()
            .map(|slot| (slot.owner, slot.name, slot.descriptor))
            .collect();
        keys.extend(unit.itable(class).iter().filter_map(|e| match &e.target {
            ItableTarget::Impl(k) => Some(k.clone()),
            _ => None,
        }));
        keys
    };
    // Rapid Type Analysis: an implementation is reachable only when its class is instantiated AND
    // some reachable code calls its (name, descriptor) virtually or through an interface.
    let mut called: BTreeSet<(String, String)> = BTreeSet::new();
    // The runtime may raise any VM exception, so those classes have instances.
    for (name, _) in VM_EXCEPTIONS {
        if unit.get(name).is_some() && unit.chain(name).is_ok() {
            add_records(name, &mut out.records);
            if out.instantiated.insert(name.to_string()) {
                work.extend(
                    implementations(name)
                        .into_iter()
                        .filter(|k| compiled(k) && called.contains(&(k.1.clone(), k.2.clone()))),
                );
            }
        }
    }
    let mut needs_classes = false;
    loop {
        // Class objects (and the String names they carry) now exist: make their code reachable.
        if needs_classes {
            // Class objects, their String names, and the main thread the registry creates.
            let thread_classes = [
                "java/lang/Thread",
                "java/lang/Thread$FieldHolder",
                "java/lang/ThreadGroup",
                OBJECT,
            ];
            for name in [CLASS, STRING].into_iter().chain(thread_classes) {
                if unit.get(name).is_none() {
                    continue;
                }
                add_records(name, &mut out.records);
                if out.instantiated.insert(name.to_string()) {
                    work.extend(
                        implementations(name).into_iter().filter(|k| {
                            compiled(k) && called.contains(&(k.1.clone(), k.2.clone()))
                        }),
                    );
                    pending_inits.push(name.to_string());
                }
            }
        }
        while let Some(class) = pending_inits.pop() {
            if !initializable.contains(&class) || !out.inits.insert(class.clone()) {
                continue;
            }
            let clinit = (class.clone(), "<clinit>".to_string(), "()V".to_string());
            if compiled(&clinit) {
                work.push(clinit);
            }
            if let Some(c) = unit.get(&class) {
                pending_inits.extend(init_prerequisites(unit, c));
            }
        }
        let Some(key) = work.pop() else { break };
        if !out.methods.insert(key.clone()) {
            continue;
        }
        let Some(Ok((_, analyzed))) = results.get(&key) else {
            continue;
        };
        if intrinsics
            .get(&key)
            .is_some_and(|(_, needs_classes)| *needs_classes)
        {
            needs_classes = true;
        }
        for catch in analyzed.dispatch.iter().flatten() {
            if let Some(c) = &catch.class {
                add_records(c, &mut out.records);
            }
        }
        pending_inits.push(key.0.clone());
        for dep in &analyzed.deps {
            match dep {
                Dep::Method(m) if compiled(m) => work.push(m.clone()),
                Dep::Method(_) => {}
                Dep::Init(c) => pending_inits.push(c.clone()),
            }
        }
        for resolved in analyzed.resolved.values() {
            let target = match resolved {
                Resolved::Call(
                    CallKind::Virtual { target, .. } | CallKind::Interface { target, .. },
                ) => target,
                _ => continue,
            };
            let selector = (target.1.clone(), target.2.clone());
            if called.insert(selector.clone()) {
                for class in &out.instantiated {
                    work.extend(
                        implementations(class)
                            .into_iter()
                            .filter(|k| k.1 == selector.0 && k.2 == selector.1 && compiled(k)),
                    );
                }
            }
        }
        for insn in &analyzed.insns {
            match &insn.op {
                Op::New { class } => {
                    add_records(class, &mut out.records);
                    if out.instantiated.insert(class.clone()) {
                        work.extend(implementations(class).into_iter().filter(|k| {
                            compiled(k) && called.contains(&(k.1.clone(), k.2.clone()))
                        }));
                    }
                }
                Op::LdcClass(_) => needs_classes = true,
                Op::LdcString(_) => {
                    add_records(STRING, &mut out.records);
                    if out.instantiated.insert(STRING.to_string()) {
                        work.extend(implementations(STRING).into_iter().filter(|k| {
                            compiled(k) && called.contains(&(k.1.clone(), k.2.clone()))
                        }));
                    }
                }
                Op::CheckCast { class } | Op::InstanceOf { class } if class.starts_with('[') => {
                    if let Some(c) = element_class(class)
                        .filter(|c| !c.starts_with('[') && unit.get(c).is_some())
                    {
                        add_records(&c, &mut out.records);
                    }
                }
                Op::ANewArray { class } | Op::CheckCast { class } | Op::InstanceOf { class }
                    if !class.starts_with('[') && unit.get(class).is_some() =>
                {
                    add_records(class, &mut out.records);
                }
                _ => {}
            }
        }
    }
    out
}

/// Compile a set of classes together into one C unit, emitting only what is reachable from
/// `roots` (all classes when None). Every method's status is reported either way.
pub fn compile_unit_rooted(
    classes: &[ClassFile],
    roots: Option<&BTreeSet<String>>,
) -> CompiledUnit {
    let unit = Unit::new(classes);
    let have_classes = classes_available(&unit);
    let mut intrinsics: HashMap<MethodKey, (String, bool)> = HashMap::new();
    let mut results: BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>> =
        BTreeMap::new();
    for class in classes {
        let structural = structural_problem(&unit, class);
        for method in &class.methods {
            let key = (
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            );
            let result = (|| {
                if let Some(reason) = &structural {
                    return Err(reason.clone());
                }

                let sig = parse_signature(&method.descriptor).map_err(|e| e.to_string())?;
                if method.access & ACC_NATIVE != 0 {
                    let Some(body) = intrinsic(&key, have_classes) else {
                        return Err("native method without a JABS implementation".to_string());
                    };
                    intrinsics.insert(key.clone(), body);
                    return Ok((sig, Analyzed::empty()));
                }
                let analyzed = analyze(&unit, class, method, &sig)?;
                Ok((sig, analyzed))
            })();
            results.insert(key, result);
        }
    }
    // A class can be initialized natively when its hierarchy is in the unit, its <clinit> body
    // compiles and its prerequisites can be initialized. Calls to methods that did not compile
    // no longer block their callers: they reach a stub that stops with a diagnostic only if it
    // actually runs. Failures only spread, so iterating until nothing changes terminates.
    let mut init_ok: HashMap<String, std::result::Result<(), String>> = classes
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                structural_problem(&unit, c).map_or(Ok(()), Err),
            )
        })
        .collect();
    loop {
        let mut changed = false;
        for class in classes {
            if init_ok[&class.name].is_err() {
                continue;
            }
            let clinit = (
                class.name.clone(),
                "<clinit>".to_string(),
                "()V".to_string(),
            );
            let problem = match results.get(&clinit) {
                Some(Err(reason)) => Some(format!(
                    "static initializer of {} not compiled: {reason}",
                    class.name
                )),
                _ => init_prerequisites(&unit, class).into_iter().find_map(|p| {
                    match init_ok.get(&p) {
                        Some(Ok(())) => None,
                        Some(Err(reason)) => Some(reason.clone()),
                        None => Some(format!("{p} is not in the compilation unit")),
                    }
                }),
            };
            if let Some(problem) = problem {
                init_ok.insert(class.name.clone(), Err(problem));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let compiled: BTreeSet<MethodKey> = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(k, _)| k.clone())
        .collect();
    let initializable: BTreeSet<String> = classes
        .iter()
        .filter(|c| init_ok[&c.name].is_ok())
        .map(|c| c.name.clone())
        .collect();
    let self_contained = self_contained_methods(&unit, classes, &results, &initializable);
    let laid_out_all: Vec<&ClassFile> = classes
        .iter()
        .filter(|c| c.access & ACC_INTERFACE == 0 && unit.chain(&c.name).is_ok())
        .collect();
    let emission = match roots {
        Some(roots) => reachable(&unit, classes, &results, &intrinsics, &initializable, roots),
        None => Emission {
            methods: compiled.clone(),
            records: laid_out_all
                .iter()
                .map(|c| c.name.clone())
                .chain(
                    classes
                        .iter()
                        .filter(|c| c.access & ACC_INTERFACE != 0)
                        .map(|c| c.name.clone()),
                )
                .collect(),
            instantiated: laid_out_all.iter().map(|c| c.name.clone()).collect(),
            inits: initializable.clone(),
        },
    };

    let mut source = String::new();
    let _ = writeln!(
        source,
        "/* Generated by JABS from {} classes. */",
        classes.len()
    );
    source.push_str(RUNTIME_PRELUDE);
    source.push('\n');
    // Layouts for every class whose hierarchy is complete, superclasses first (types only).
    let mut laid_out = laid_out_all.clone();
    laid_out.sort_by_key(|c| {
        (
            unit.chain(&c.name).map(|ch| ch.len()).unwrap_or(0),
            c.name.clone(),
        )
    });
    for class in &laid_out {
        emit_object_layout(&mut source, class);
    }
    for class in classes {
        for method in &class.methods {
            let key = (
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            );
            if let (true, Some(Ok((sig, _)))) = (emission.methods.contains(&key), results.get(&key))
            {
                let _ = writeln!(source, "{};", prototype(&class.name, method, sig));
            }
        }
    }
    // Every class whose initialization emitted code may trigger: real or stub.
    for class in emitted_init_targets(&unit, &results, &emission) {
        let _ = writeln!(source, "int {}(void);", init_symbol(&class));
    }
    // Callers reach not-compiled methods through stubs under the real symbol: declare those too.
    for key in reached_stub_targets(&results, &emission) {
        let Some(class) = unit.get(&key.0) else {
            continue;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| m.name == key.1 && m.descriptor == key.2)
        else {
            continue;
        };
        if let Ok(sig) = parse_signature(&key.2) {
            let _ = writeln!(source, "{};", prototype(&key.0, method, &sig));
        }
    }
    source.push_str("\n/* @@definitions@@ */\n");
    // A vtable entry is real code only for instantiated classes; everything else is a stub.
    let live = |class: &str, key: &MethodKey| {
        emission.instantiated.contains(class) && emission.methods.contains(key)
    };
    let recorded: Vec<&ClassFile> = laid_out
        .iter()
        .copied()
        .filter(|c| emission.records.contains(&c.name))
        .collect();
    emit_stubs(&mut source, &unit, &recorded, &live);
    // Interface records first: class records point at them.
    let mut interface_records: BTreeSet<&str> = emission
        .records
        .iter()
        .map(String::as_str)
        .filter(|n| unit.is_interface(n))
        .collect();
    for class in &recorded {
        for i in unit.superinterfaces(&class.name).unwrap_or_default() {
            interface_records.insert(&i.name);
        }
    }
    for name in &interface_records {
        emit_interface_record(&mut source, unit.get(name).unwrap());
    }
    let recorded_names: BTreeSet<&str> = recorded.iter().map(|c| c.name.as_str()).collect();
    for class in &recorded {
        emit_class_record(&mut source, &unit, class, &live);
    }
    emit_vm_throwables(&mut source, &recorded_names);
    let mut literals: BTreeSet<Vec<u16>> = emission
        .methods
        .iter()
        .filter_map(|k| results.get(k)?.as_ref().ok())
        .flat_map(|(_, a)| a.insns.iter())
        .filter_map(|i| match &i.op {
            Op::LdcString(units) => Some(units.clone()),
            _ => None,
        })
        .collect();
    for class in classes.iter().filter(|c| emission.inits.contains(&c.name)) {
        literals.extend(
            class
                .fields
                .iter()
                .filter(|f| f.access & ACC_STATIC != 0)
                .filter_map(|f| constant_string(class, f)),
        );
    }
    emit_literals(&mut source, &literals);
    if have_classes && emission.records.contains(CLASS) && emission.records.contains(STRING) {
        emit_class_registry(&mut source, &unit, &recorded);
    }
    for class in classes.iter().filter(|c| emission.inits.contains(&c.name)) {
        emit_statics_and_init(&mut source, &unit, class);
    }
    emit_reached_stubs(&mut source, &unit, &results, &emission, &init_ok);
    // Everything above the methods: shared declarations, then data and support definitions.
    let header_end = source
        .find(&format!("\n{}", "/* @@definitions@@ */"))
        .unwrap_or(0);
    let shared = source[..header_end].to_string();
    let support = source[header_end..].to_string();
    source.clear();
    let mut method_texts: Vec<String> = Vec::new();
    let mut methods = Vec::new();
    for class in classes {
        for method in &class.methods {
            let key = (
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            );
            let status = match &results[&key] {
                _ if method.access & ACC_ABSTRACT != 0 => MethodStatus::Abstract,
                Ok((sig, analyzed)) => {
                    if !emission.methods.contains(&key) {
                    } else if let Some((body, _)) = intrinsics.get(&key) {
                        let mut unused = String::new();
                        if method.access & ACC_STATIC == 0 {
                            unused.push_str(" (void)self;");
                        }
                        for i in 0..sig.params.len() {
                            let _ = write!(unused, " (void)a{i};");
                        }
                        let mut text = String::new();
                        let _ = writeln!(
                            text,
                            "/* native {}.{}{} */",
                            class.name, method.name, method.descriptor
                        );
                        let _ = writeln!(
                            text,
                            "{} {{{unused} {body} }}\n",
                            prototype(&class.name, method, sig)
                        );
                        method_texts.push(text);
                    } else {
                        let mut text = String::new();
                        emit_method(&mut text, &unit, class, method, sig, analyzed);
                        method_texts.push(text);
                    }
                    MethodStatus::Compiled {
                        symbol: symbol(&key.0, &key.1, &key.2),
                        self_contained: self_contained.contains(&key),
                    }
                }
                Err(reason) => MethodStatus::Unsupported {
                    reason: reason.clone(),
                },
            };
            methods.push(MethodReport {
                class: key.0,
                name: key.1,
                descriptor: key.2,
                status,
            });
        }
    }
    let compiled = methods
        .iter()
        .filter(|m| matches!(m.status, MethodStatus::Compiled { .. }))
        .count();
    let abstract_methods = methods
        .iter()
        .filter(|m| m.status == MethodStatus::Abstract)
        .count();
    let self_contained_count = methods
        .iter()
        .filter(|m| {
            matches!(
                m.status,
                MethodStatus::Compiled {
                    self_contained: true,
                    ..
                }
            )
        })
        .count();
    CompiledUnit {
        schema_version: 1,
        classes: classes.len(),
        compiled,
        unsupported: methods.len() - compiled - abstract_methods,
        abstract_methods,
        self_contained: self_contained_count,
        methods,
        c_source: format!(
            "{shared}{RUNTIME_GLOBALS}{support}{}",
            method_texts.concat()
        ),
        c_files: {
            let header = format!(
                "{shared}\n/* Definitions elsewhere in this program. */\n{}",
                declarations_for(&support)
            );
            let mut files = vec![
                ("jabs_unit.h".to_string(), header),
                (
                    "unit_0.c".to_string(),
                    format!("#include \"jabs_unit.h\"\n{RUNTIME_GLOBALS}{support}"),
                ),
            ];
            for (i, chunk) in method_chunks(&method_texts, 32).into_iter().enumerate() {
                files.push((
                    format!("unit_{}.c", i + 1),
                    format!("#include \"jabs_unit.h\"\n{chunk}"),
                ));
            }
            files
        },
    }
}
