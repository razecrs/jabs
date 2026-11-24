//! invokedynamic, resolved at compile time. For each call site whose bootstrap method JABS
//! knows, this pass writes the class the JVM would spin at link time as real bytecode, and
//! rewrites the 5-byte `invokedynamic` into `invokestatic` of that class's factory method plus
//! two `nop`s (same stack effect, no branch offset moves). The generated classes then go through
//! the ordinary verifier and compiler.
//!
//! Supported bootstraps:
//! - `LambdaMetafactory.metafactory` / `altMetafactory` (lambdas, method references), with the
//!   JVM's argument and return adaptations, marker interfaces and bridges. A non-capturing
//!   lambda yields one shared instance, as on HotSpot.
//! - `StringConcatFactory.makeConcatWithConstants` / `makeConcat` (`a + b` since Java 9).
//!
//! - `ObjectMethods.bootstrap` (records' equals, hashCode, toString), with the JDK's algorithms.
//! - `SwitchBootstraps.typeSwitch` / `enumSwitch` with Class, String and Integer labels.
//!
//! Other bootstraps are left alone: their methods stay unsupported, with the reason reported.

use crate::classfile::{ClassFile, Code, Constant, Member};
use std::collections::HashMap;

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_SUPER: u16 = 0x0020;
const ACC_SYNTHETIC: u16 = 0x1000;

const OBJECT: &str = "java/lang/Object";

/// What the pass did: generated classes and call sites it could not translate.
#[derive(Debug, Default)]
pub struct Report {
    pub lambdas: usize,
    pub concats: usize,
    /// Record methods (ObjectMethods) and pattern switches (SwitchBootstraps).
    pub others: usize,
    /// (class, method, reason) for call sites left as invokedynamic.
    pub skipped: Vec<(String, String, String)>,
}

/// Translate every supported invokedynamic in `classes`, appending the generated classes.
pub fn desugar(classes: &mut Vec<ClassFile>) -> Report {
    let mut report = Report::default();
    let mut generated = Vec::new();
    for class in classes.iter_mut() {
        let mut counter = 0usize;
        for m in 0..class.methods.len() {
            let Some(code) = class.methods[m].code.as_ref() else {
                continue;
            };
            let sites: Vec<usize> = instruction_starts(&code.bytecode)
                .into_iter()
                .filter(|&pc| code.bytecode[pc] == 0xba)
                .collect();
            for pc in sites {
                let index = u16::from_be_bytes([
                    class.methods[m].code.as_ref().unwrap().bytecode[pc + 1],
                    class.methods[m].code.as_ref().unwrap().bytecode[pc + 2],
                ]);
                match translate(class, index, counter + 1) {
                    Ok((synthetic, factory, kind)) => {
                        counter += 1;
                        match kind {
                            Kind::Lambda => report.lambdas += 1,
                            Kind::Concat => report.concats += 1,
                            Kind::Other => report.others += 1,
                        }
                        let descriptor = indy_descriptor(class, index).unwrap_or_default();
                        let target = add_method_ref(class, &synthetic.name, &factory, &descriptor);
                        let code = class.methods[m].code.as_mut().unwrap();
                        let [hi, lo] = target.to_be_bytes();
                        code.bytecode[pc..pc + 5].copy_from_slice(&[0xb8, hi, lo, 0x00, 0x00]);
                        generated.push(synthetic);
                    }
                    Err(reason) => report.skipped.push((
                        class.name.clone(),
                        class.methods[m].name.clone(),
                        reason,
                    )),
                }
            }
        }
    }
    classes.extend(generated);
    report
}

enum Kind {
    Lambda,
    Concat,
    /// Records' ObjectMethods and pattern switches.
    Other,
}

/// Start offsets of every instruction in a method's bytecode (empty if it is malformed).
fn instruction_starts(code: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    let read_i32 = |at: usize| -> Option<i32> {
        code.get(at..at + 4)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    while pc < code.len() {
        out.push(pc);
        let op = code[pc];
        let len = match op {
            0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
            0x11
            | 0x13
            | 0x14
            | 0x84
            | 0x99..=0xa8
            | 0xb2..=0xb8
            | 0xbb
            | 0xbd
            | 0xc0
            | 0xc1
            | 0xc6
            | 0xc7 => 3,
            0xc5 => 4,
            0xb9 | 0xba | 0xc8 | 0xc9 => 5,
            0xc4 => {
                if code.get(pc + 1) == Some(&0x84) {
                    6
                } else {
                    4
                }
            }
            0xaa => {
                let base = (pc + 4) & !3;
                let (Some(low), Some(high)) = (read_i32(base + 4), read_i32(base + 8)) else {
                    return Vec::new();
                };
                let count = (high as i64 - low as i64 + 1).max(0) as usize;
                base + 12 + count * 4 - pc
            }
            0xab => {
                let base = (pc + 4) & !3;
                let Some(pairs) = read_i32(base + 4) else {
                    return Vec::new();
                };
                base + 8 + pairs.max(0) as usize * 8 - pc
            }
            _ => 1,
        };
        pc += len;
    }
    out
}

/// Utf8 text at a pool index.
fn utf8(class: &ClassFile, index: u16) -> Option<&str> {
    match class.pool.get(index as usize)? {
        Constant::Utf8(s) => Some(s),
        _ => None,
    }
}

/// (name, descriptor) of a NameAndType constant.
fn name_and_type(class: &ClassFile, index: u16) -> Option<(&str, &str)> {
    match class.pool.get(index as usize)? {
        Constant::NameAndType { name, descriptor } => {
            Some((utf8(class, *name)?, utf8(class, *descriptor)?))
        }
        _ => None,
    }
}

/// Internal name of a Class constant.
fn class_name(class: &ClassFile, index: u16) -> Option<&str> {
    match class.pool.get(index as usize)? {
        Constant::Class(n) => utf8(class, *n),
        _ => None,
    }
}

/// The invoked type of an InvokeDynamic constant.
fn indy_descriptor(class: &ClassFile, index: u16) -> Option<String> {
    match class.pool.get(index as usize)? {
        Constant::InvokeDynamic {
            name_and_type: nt, ..
        } => Some(name_and_type(class, *nt)?.1.to_string()),
        _ => None,
    }
}

/// A MethodHandle constant: (reference kind, owner, name, descriptor, owner is an interface).
fn method_handle(class: &ClassFile, index: u16) -> Option<(u8, String, String, String, bool)> {
    let Constant::MethodHandle { kind, reference } = class.pool.get(index as usize)? else {
        return None;
    };
    let (owner, nt, interface) = match class.pool.get(*reference as usize)? {
        Constant::MethodRef {
            class: c,
            name_and_type,
        } => (*c, *name_and_type, false),
        Constant::InterfaceMethodRef {
            class: c,
            name_and_type,
        } => (*c, *name_and_type, true),
        Constant::FieldRef {
            class: c,
            name_and_type,
        } => (*c, *name_and_type, false),
        _ => return None,
    };
    let (name, descriptor) = name_and_type(class, nt)?;
    Some((
        *kind,
        class_name(class, owner)?.to_string(),
        name.to_string(),
        descriptor.to_string(),
        interface,
    ))
}

/// Descriptor of a MethodType constant.
fn method_type(class: &ClassFile, index: u16) -> Option<String> {
    match class.pool.get(index as usize)? {
        Constant::MethodType(d) => Some(utf8(class, *d)?.to_string()),
        _ => None,
    }
}

/// Append a Methodref to `class`'s pool and return its index.
fn add_method_ref(class: &mut ClassFile, owner: &str, name: &str, descriptor: &str) -> u16 {
    let push = |pool: &mut Vec<Constant>, c: Constant| -> u16 {
        pool.push(c);
        (pool.len() - 1) as u16
    };
    let owner_utf8 = push(&mut class.pool, Constant::Utf8(owner.to_string()));
    let owner_class = push(&mut class.pool, Constant::Class(owner_utf8));
    let name_utf8 = push(&mut class.pool, Constant::Utf8(name.to_string()));
    let desc_utf8 = push(&mut class.pool, Constant::Utf8(descriptor.to_string()));
    let nt = push(
        &mut class.pool,
        Constant::NameAndType {
            name: name_utf8,
            descriptor: desc_utf8,
        },
    );
    push(
        &mut class.pool,
        Constant::MethodRef {
            class: owner_class,
            name_and_type: nt,
        },
    )
}

/// Dispatch on the call site's bootstrap method.
fn translate(
    class: &ClassFile,
    index: u16,
    number: usize,
) -> Result<(ClassFile, String, Kind), String> {
    let Some(Constant::InvokeDynamic {
        bootstrap,
        name_and_type: nt,
    }) = class.pool.get(index as usize)
    else {
        return Err("invokedynamic operand is not an InvokeDynamic constant".into());
    };
    let (name, descriptor) = name_and_type(class, *nt).ok_or("malformed InvokeDynamic constant")?;
    let (handle, args) = class
        .bootstrap_methods
        .get(*bootstrap as usize)
        .ok_or("missing BootstrapMethods entry")?;
    let (_, owner, bsm, _, _) =
        method_handle(class, *handle).ok_or("malformed bootstrap method handle")?;
    let synthetic_name = format!("{}$$Lambda${number}", class.name);
    match (owner.as_str(), bsm.as_str()) {
        ("java/lang/invoke/LambdaMetafactory", "metafactory" | "altMetafactory") => lambda(
            class,
            &synthetic_name,
            name,
            descriptor,
            args,
            bsm == "altMetafactory",
        )
        .map(|c| (c, "make".to_string(), Kind::Lambda)),
        ("java/lang/invoke/StringConcatFactory", "makeConcatWithConstants" | "makeConcat") => {
            let concat_name = format!("{}$$Concat${number}", class.name);
            concat(
                class,
                &concat_name,
                descriptor,
                args,
                bsm == "makeConcatWithConstants",
            )
            .map(|c| (c, "concat".to_string(), Kind::Concat))
        }
        ("java/lang/runtime/ObjectMethods", "bootstrap") => {
            let record_name = format!("{}$$Record${number}", class.name);
            object_method(class, &record_name, name, descriptor, args)
                .map(|c| (c, "invoke".to_string(), Kind::Other))
        }
        ("java/lang/runtime/SwitchBootstraps", "typeSwitch" | "enumSwitch") => {
            let switch_name = format!("{}$$Switch${number}", class.name);
            type_switch(class, &switch_name, descriptor, args, bsm == "enumSwitch")
                .map(|c| (c, "invoke".to_string(), Kind::Other))
        }
        _ => Err(format!(
            "invokedynamic with bootstrap {owner}.{bsm} is not supported yet"
        )),
    }
}

// ---- Descriptors ----

/// Field-type descriptors of a method descriptor's parameters, and its return type.
fn split_method(descriptor: &str) -> Option<(Vec<String>, String)> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let (params, ret) = (&inner[..close], &inner[close + 1..]);
    let mut out = Vec::new();
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while bytes[i] == b'[' {
            i += 1;
        }
        if bytes[i] == b'L' {
            i += params[i..].find(';')? + 1;
        } else {
            i += 1;
        }
        out.push(params[start..i].to_string());
    }
    Some((out, ret.to_string()))
}

/// Local-variable slots a type takes.
fn slots(ty: &str) -> u16 {
    if ty == "J" || ty == "D" { 2 } else { 1 }
}

fn is_reference(ty: &str) -> bool {
    ty.starts_with('L') || ty.starts_with('[')
}

/// The class a reference descriptor names, for checkcast ("Ljava/lang/String;" -> "java/lang/String").
fn reference_class(ty: &str) -> String {
    if let Some(name) = ty.strip_prefix('L').and_then(|t| t.strip_suffix(';')) {
        name.to_string()
    } else {
        ty.to_string()
    }
}

/// The wrapper class of a primitive and the wrapper of a wrapper descriptor.
fn wrapper_of(primitive: &str) -> Option<&'static str> {
    Some(match primitive {
        "Z" => "java/lang/Boolean",
        "B" => "java/lang/Byte",
        "C" => "java/lang/Character",
        "S" => "java/lang/Short",
        "I" => "java/lang/Integer",
        "J" => "java/lang/Long",
        "F" => "java/lang/Float",
        "D" => "java/lang/Double",
        _ => return None,
    })
}

fn primitive_of_wrapper(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "Ljava/lang/Boolean;" => "Z",
        "Ljava/lang/Byte;" => "B",
        "Ljava/lang/Character;" => "C",
        "Ljava/lang/Short;" => "S",
        "Ljava/lang/Integer;" => "I",
        "Ljava/lang/Long;" => "J",
        "Ljava/lang/Float;" => "F",
        "Ljava/lang/Double;" => "D",
        _ => return None,
    })
}

// ---- Constant pool and bytecode for generated classes ----

/// A constant pool under construction, with exact UTF-16 text for String constants.
struct Pool {
    entries: Vec<Constant>,
    utf8: HashMap<String, u16>,
    string_units: HashMap<u16, Vec<u16>>,
}

impl Pool {
    fn new() -> Self {
        Pool {
            entries: vec![Constant::Unusable],
            utf8: HashMap::new(),
            string_units: HashMap::new(),
        }
    }
    fn push(&mut self, c: Constant) -> u16 {
        self.entries.push(c);
        (self.entries.len() - 1) as u16
    }
    fn utf8(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.utf8.get(s) {
            return i;
        }
        let i = self.push(Constant::Utf8(s.to_string()));
        self.utf8.insert(s.to_string(), i);
        i
    }
    fn class(&mut self, name: &str) -> u16 {
        let n = self.utf8(name);
        self.push(Constant::Class(n))
    }
    fn nat(&mut self, name: &str, descriptor: &str) -> u16 {
        let (n, d) = (self.utf8(name), self.utf8(descriptor));
        self.push(Constant::NameAndType {
            name: n,
            descriptor: d,
        })
    }
    fn method(&mut self, owner: &str, name: &str, descriptor: &str, interface: bool) -> u16 {
        let (c, nt) = (self.class(owner), self.nat(name, descriptor));
        self.push(if interface {
            Constant::InterfaceMethodRef {
                class: c,
                name_and_type: nt,
            }
        } else {
            Constant::MethodRef {
                class: c,
                name_and_type: nt,
            }
        })
    }
    fn field(&mut self, owner: &str, name: &str, descriptor: &str) -> u16 {
        let (c, nt) = (self.class(owner), self.nat(name, descriptor));
        self.push(Constant::FieldRef {
            class: c,
            name_and_type: nt,
        })
    }
    /// A String constant with exact UTF-16 contents (its own Utf8 entry, so lone surrogates survive).
    fn string(&mut self, units: Vec<u16>) -> u16 {
        let text = self.push(Constant::Utf8(String::from_utf16_lossy(&units)));
        self.string_units.insert(text, units);
        self.push(Constant::String(text))
    }
    /// Copy a loadable constant (String, Integer, Long, Float, Double, Class) from another class.
    fn copy_constant(&mut self, from: &ClassFile, index: u16) -> Option<(u16, char)> {
        Some(match from.pool.get(index as usize)? {
            Constant::String(text) => {
                let units =
                    from.string_units.get(text).cloned().unwrap_or_else(|| {
                        utf8(from, *text).unwrap_or("").encode_utf16().collect()
                    });
                (self.string(units), 'S')
            }
            Constant::Integer(v) => (self.push(Constant::Integer(*v)), 'I'),
            Constant::Float(v) => (self.push(Constant::Float(*v)), 'F'),
            Constant::Long(v) => {
                let i = self.push(Constant::Long(*v));
                self.push(Constant::Unusable);
                (i, 'J')
            }
            Constant::Double(v) => {
                let i = self.push(Constant::Double(*v));
                self.push(Constant::Unusable);
                (i, 'D')
            }
            Constant::Class(n) => (self.class(utf8(from, *n)?), 'C'),
            _ => return None,
        })
    }
}

/// Bytecode under construction.
#[derive(Default)]
struct Asm {
    code: Vec<u8>,
}

impl Asm {
    fn op(&mut self, op: u8) -> &mut Self {
        self.code.push(op);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.code.extend(v.to_be_bytes());
        self
    }
    /// Load local `slot` of type `ty` (wide form past 255).
    fn load(&mut self, ty: &str, slot: u16) -> &mut Self {
        let op = match ty {
            "J" => 0x16,
            "F" => 0x17,
            "D" => 0x18,
            t if is_reference(t) => 0x19,
            _ => 0x15,
        };
        self.local(op, slot)
    }
    fn local(&mut self, op: u8, slot: u16) -> &mut Self {
        if slot <= 255 {
            self.op(op).op(slot as u8)
        } else {
            self.op(0xc4).op(op).u16(slot)
        }
    }
    /// The return instruction for a type ("V" for void).
    fn ret(&mut self, ty: &str) -> &mut Self {
        self.op(match ty {
            "V" => 0xb1,
            "J" => 0xad,
            "F" => 0xae,
            "D" => 0xaf,
            t if is_reference(t) => 0xb0,
            _ => 0xac,
        })
    }
    fn checkcast(&mut self, pool: &mut Pool, class: &str) -> &mut Self {
        let c = pool.class(class);
        self.op(0xc0).u16(c)
    }
    fn invoke(&mut self, op: u8, index: u16, arg_slots: u16) -> &mut Self {
        self.op(op).u16(index);
        if op == 0xb9 {
            self.op((arg_slots + 1) as u8).op(0);
        }
        self
    }
    fn ldc(&mut self, index: u16, wide: bool) -> &mut Self {
        self.op(if wide { 0x14 } else { 0x13 }).u16(index)
    }
    /// Emit a branch with a placeholder offset; returns its position for `land`.
    fn branch(&mut self, op: u8) -> usize {
        let at = self.code.len();
        self.op(op).u16(0);
        at
    }
    /// Point the branch at `at` to the current position.
    fn land(&mut self, at: usize) {
        let offset = (self.code.len() as i64 - at as i64) as i16;
        self.code[at + 1..at + 3].copy_from_slice(&offset.to_be_bytes());
    }
    /// An int constant (iconst, bipush, sipush or ldc).
    fn int(&mut self, pool: &mut Pool, v: i32) -> &mut Self {
        match v {
            -1..=5 => self.op((0x03 + v) as u8),
            -128..=127 => self.op(0x10).op(v as i8 as u8),
            -32768..=32767 => self.op(0x11).u16(v as i16 as u16),
            _ => {
                let i = pool.push(Constant::Integer(v));
                self.ldc(i, false)
            }
        }
    }
}

/// Convert the value on the stack from type `from` to type `to` the way LambdaMetafactory adapts
/// arguments and results: casts, boxing, unboxing (from the known wrapper, or the target's), and
/// primitive widening. `None` when the types are not adaptable.
fn adapt(asm: &mut Asm, pool: &mut Pool, from: &str, to: &str) -> Option<()> {
    if from == to {
        return Some(());
    }
    match (is_reference(from), is_reference(to)) {
        (true, true) => {
            if to != "Ljava/lang/Object;" {
                asm.checkcast(pool, &reference_class(to));
            }
        }
        (true, false) => {
            // Unbox: from a wrapper type if the source is one, else the target's own wrapper.
            let source = primitive_of_wrapper(from).unwrap_or(to);
            let wrapper = wrapper_of(source)?;
            if primitive_of_wrapper(from).is_none() {
                asm.checkcast(pool, wrapper);
            }
            let name = match source {
                "Z" => "booleanValue",
                "B" => "byteValue",
                "C" => "charValue",
                "S" => "shortValue",
                "I" => "intValue",
                "J" => "longValue",
                "F" => "floatValue",
                _ => "doubleValue",
            };
            let m = pool.method(wrapper, name, &format!("(){source}"), false);
            asm.invoke(0xb6, m, 0);
            widen(asm, source, to)?;
        }
        (false, true) => {
            let wrapper = wrapper_of(from)?;
            let m = pool.method(wrapper, "valueOf", &format!("({from})L{wrapper};"), false);
            asm.invoke(0xb8, m, slots(from));
            let boxed = format!("L{wrapper};");
            if to != boxed
                && !matches!(
                    to,
                    "Ljava/lang/Object;"
                        | "Ljava/lang/Number;"
                        | "Ljava/io/Serializable;"
                        | "Ljava/lang/Comparable;"
                )
            {
                asm.checkcast(pool, &reference_class(to));
            }
        }
        (false, false) => widen(asm, from, to)?,
    }
    Some(())
}

/// Primitive widening conversion (JLS 5.1.2); int-like types share the int representation.
fn widen(asm: &mut Asm, from: &str, to: &str) -> Option<()> {
    let int_like = |t: &str| matches!(t, "B" | "S" | "C" | "I" | "Z");
    match (from, to) {
        _ if from == to => {}
        (f, t) if int_like(f) && int_like(t) && f != "Z" && t != "Z" => {}
        (f, "J") if int_like(f) && f != "Z" => {
            asm.op(0x85);
        }
        (f, "F") if int_like(f) && f != "Z" => {
            asm.op(0x86);
        }
        (f, "D") if int_like(f) && f != "Z" => {
            asm.op(0x87);
        }
        ("J", "F") => {
            asm.op(0x89);
        }
        ("J", "D") => {
            asm.op(0x8a);
        }
        ("F", "D") => {
            asm.op(0x8d);
        }
        _ => return None,
    }
    Some(())
}

/// A synthetic final class with the given members.
fn synthetic_class(
    name: &str,
    interfaces: Vec<String>,
    fields: Vec<Member>,
    methods: Vec<Member>,
    pool: Pool,
) -> ClassFile {
    ClassFile {
        minor: 0,
        major: 52,
        pool: pool.entries,
        access: ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_SYNTHETIC,
        name: name.to_string(),
        super_name: Some(OBJECT.to_string()),
        interfaces,
        fields,
        methods,
        string_units: pool.string_units,
        bootstrap_methods: Vec::new(),
    }
}

fn member(access: u16, name: &str, descriptor: &str, code: Option<Asm>, max_locals: u16) -> Member {
    Member {
        access,
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        code: code.map(|a| Code {
            // Generous bounds: the verifier computes the real stack shape.
            max_stack: 64 + max_locals * 2,
            max_locals,
            bytecode: a.code,
            handlers: Vec::new(),
        }),
        constant_value: None,
        annotations: Vec::new(),
    }
}

// ---- LambdaMetafactory ----

const FLAG_SERIALIZABLE: i32 = 1;
const FLAG_MARKERS: i32 = 2;
const FLAG_BRIDGES: i32 = 4;

/// The class LambdaMetafactory would spin for one call site.
fn lambda(
    class: &ClassFile,
    name: &str,
    sam_name: &str,
    invoked: &str,
    args: &[u16],
    alt: bool,
) -> Result<ClassFile, String> {
    let bad = |what: &str| format!("lambda call site: {what}");
    let (captures, iface) = split_method(invoked).ok_or_else(|| bad("malformed invoked type"))?;
    if !iface.starts_with('L') {
        return Err(bad("invoked type does not return an interface"));
    }
    let iface = reference_class(&iface);
    let sam = method_type(
        class,
        *args.first().ok_or_else(|| bad("no static arguments"))?,
    )
    .ok_or_else(|| bad("bad SAM type"))?;
    let (kind, owner, impl_name, impl_desc, impl_interface) =
        method_handle(class, *args.get(1).ok_or_else(|| bad("no implementation"))?)
            .ok_or_else(|| bad("bad implementation handle"))?;
    let instantiated = method_type(
        class,
        *args.get(2).ok_or_else(|| bad("no instantiated type"))?,
    )
    .ok_or_else(|| bad("bad instantiated type"))?;
    let mut interfaces = vec![iface.clone()];
    let mut bridges: Vec<String> = Vec::new();
    if alt {
        let int_arg = |i: usize| match args.get(i).and_then(|a| class.pool.get(*a as usize)) {
            Some(Constant::Integer(v)) => Some(*v),
            _ => None,
        };
        let flags = int_arg(3).ok_or_else(|| bad("altMetafactory without flags"))?;
        let mut at = 4;
        if flags & FLAG_MARKERS != 0 {
            let n = int_arg(at).ok_or_else(|| bad("marker count"))?;
            at += 1;
            for _ in 0..n {
                let marker = args
                    .get(at)
                    .and_then(|a| class_name(class, *a))
                    .ok_or_else(|| bad("marker interface"))?;
                interfaces.push(marker.to_string());
                at += 1;
            }
        }
        if flags & FLAG_BRIDGES != 0 {
            let n = int_arg(at).ok_or_else(|| bad("bridge count"))?;
            at += 1;
            for _ in 0..n {
                let bridge = args
                    .get(at)
                    .and_then(|a| method_type(class, *a))
                    .ok_or_else(|| bad("bridge type"))?;
                bridges.push(bridge);
                at += 1;
            }
        }
        if flags & FLAG_SERIALIZABLE != 0 && !interfaces.iter().any(|i| i == "java/io/Serializable")
        {
            interfaces.push("java/io/Serializable".to_string());
        }
    }

    let mut pool = Pool::new();
    let this = name;
    // Capture fields.
    let fields: Vec<Member> = captures
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            member(
                ACC_PRIVATE | ACC_FINAL,
                &format!("arg${}", i + 1),
                ty,
                None,
                0,
            )
        })
        .collect();
    let ctor_desc = format!("({})V", captures.concat());
    let mut methods = Vec::new();

    // Constructor: store the captured values.
    let mut ctor = Asm::default();
    let object_init = pool.method(OBJECT, "<init>", "()V", false);
    ctor.op(0x2a).invoke(0xb7, object_init, 0);
    let mut slot = 1;
    for (i, ty) in captures.iter().enumerate() {
        let f = pool.field(this, &format!("arg${}", i + 1), ty);
        ctor.op(0x2a).load(ty, slot).op(0xb5).u16(f);
        slot += slots(ty);
    }
    ctor.op(0xb1);
    methods.push(member(ACC_PRIVATE, "<init>", &ctor_desc, Some(ctor), slot));

    // Factory: a fresh object per evaluation, or one shared instance without captures.
    let factory_desc = invoked.to_string();
    let mut factory = Asm::default();
    let this_class = pool.class(this);
    if captures.is_empty() {
        let instance = pool.field(this, "INSTANCE", &format!("L{iface};"));
        factory.op(0xb2).u16(instance).op(0xb0);
        let mut clinit = Asm::default();
        let init = pool.method(this, "<init>", "()V", false);
        clinit
            .op(0xbb)
            .u16(this_class)
            .op(0x59)
            .invoke(0xb7, init, 0)
            .op(0xb3)
            .u16(instance)
            .op(0xb1);
        methods.push(member(ACC_STATIC, "<clinit>", "()V", Some(clinit), 0));
    } else {
        let init = pool.method(this, "<init>", &ctor_desc, false);
        factory.op(0xbb).u16(this_class).op(0x59);
        let mut s = 0;
        for ty in &captures {
            factory.load(ty, s);
            s += slots(ty);
        }
        factory.invoke(0xb7, init, s).op(0xb0);
    }
    let factory_locals: u16 = captures.iter().map(|t| slots(t)).sum();
    methods.push(member(
        ACC_PUBLIC | ACC_STATIC,
        "make",
        &factory_desc,
        Some(factory),
        factory_locals,
    ));

    // The interface method and its bridges.
    let (impl_params, impl_ret) =
        split_method(&impl_desc).ok_or_else(|| bad("malformed implementation type"))?;
    let (inst_params, inst_ret) =
        split_method(&instantiated).ok_or_else(|| bad("malformed instantiated type"))?;
    // The implementation's full argument list: receiver first for instance kinds.
    let mut targets: Vec<String> = Vec::new();
    if matches!(kind, 5 | 7 | 9) {
        targets.push(format!("L{owner};"));
    }
    targets.extend(impl_params.iter().cloned());
    let result = if kind == 8 {
        format!("L{owner};")
    } else {
        impl_ret.clone()
    };
    for descriptor in std::iter::once(sam.clone()).chain(bridges) {
        let (params, ret) = split_method(&descriptor).ok_or_else(|| bad("malformed SAM type"))?;
        if captures.len() + params.len() != targets.len() || params.len() != inst_params.len() {
            return Err(bad(
                "argument counts of the interface and implementation differ",
            ));
        }
        let mut body = Asm::default();
        if kind == 8 {
            let c = pool.class(&owner);
            body.op(0xbb).u16(c).op(0x59);
        }
        for (i, ty) in captures.iter().enumerate() {
            let f = pool.field(this, &format!("arg${}", i + 1), ty);
            body.op(0x2a).op(0xb4).u16(f);
            adapt(&mut body, &mut pool, ty, &targets[i])
                .ok_or_else(|| bad("capture type not adaptable"))?;
        }
        let mut slot = 1;
        for (j, ty) in params.iter().enumerate() {
            body.load(ty, slot);
            slot += slots(ty);
            let target = &targets[captures.len() + j];
            // Through the instantiated type, as LambdaMetafactory checks it.
            let via = &inst_params[j];
            if is_reference(ty) && is_reference(via) && via != ty {
                adapt(&mut body, &mut pool, ty, via)
                    .ok_or_else(|| bad("parameter not adaptable"))?;
                adapt(&mut body, &mut pool, via, target)
                    .ok_or_else(|| bad("parameter not adaptable"))?;
            } else {
                adapt(&mut body, &mut pool, ty, target)
                    .ok_or_else(|| bad("parameter not adaptable"))?;
            }
        }
        let arg_slots: u16 = impl_params.iter().map(|t| slots(t)).sum();
        match kind {
            6 => {
                let m = pool.method(&owner, &impl_name, &impl_desc, impl_interface);
                body.invoke(0xb8, m, arg_slots);
            }
            5 => {
                let m = pool.method(&owner, &impl_name, &impl_desc, false);
                body.invoke(0xb6, m, arg_slots);
            }
            9 => {
                let m = pool.method(&owner, &impl_name, &impl_desc, true);
                body.invoke(0xb9, m, arg_slots);
            }
            7 | 8 => {
                let m = pool.method(&owner, &impl_name, &impl_desc, impl_interface);
                body.invoke(0xb7, m, arg_slots);
            }
            _ => return Err(bad(&format!("method handle kind {kind} not supported"))),
        }
        if ret == "V" {
            match result.as_str() {
                "V" => {}
                "J" | "D" => {
                    body.op(0x58);
                }
                _ => {
                    body.op(0x57);
                }
            }
        } else if result == "V" {
            return Err(bad(
                "void implementation for a value-returning interface method",
            ));
        } else {
            let via = &inst_ret;
            if is_reference(&result) && is_reference(via) && via != &ret && !is_reference(&ret) {
                adapt(&mut body, &mut pool, &result, via)
                    .ok_or_else(|| bad("result not adaptable"))?;
                adapt(&mut body, &mut pool, via, &ret)
                    .ok_or_else(|| bad("result not adaptable"))?;
            } else {
                adapt(&mut body, &mut pool, &result, &ret)
                    .ok_or_else(|| bad("result not adaptable"))?;
            }
        }
        body.ret(&ret);
        let locals = 1 + params.iter().map(|t| slots(t)).sum::<u16>();
        if methods
            .iter()
            .any(|m: &Member| m.name == sam_name && m.descriptor == descriptor)
        {
            continue; // a bridge identical to the main method
        }
        methods.push(member(
            ACC_PUBLIC,
            sam_name,
            &descriptor,
            Some(body),
            locals,
        ));
    }
    let mut fields = fields;
    if captures.is_empty() {
        fields.push(member(
            ACC_PRIVATE | ACC_STATIC | ACC_FINAL,
            "INSTANCE",
            &format!("L{iface};"),
            None,
            0,
        ));
    }
    Ok(synthetic_class(name, interfaces, fields, methods, pool))
}

// ---- StringConcatFactory ----

/// A class with `static String concat(args)` doing what makeConcat[WithConstants] links.
fn concat(
    class: &ClassFile,
    name: &str,
    invoked: &str,
    args: &[u16],
    with_constants: bool,
) -> Result<ClassFile, String> {
    let bad = |what: &str| format!("string concatenation call site: {what}");
    let (params, ret) = split_method(invoked).ok_or_else(|| bad("malformed invoked type"))?;
    if ret != "Ljava/lang/String;" {
        return Err(bad("does not return String"));
    }
    // The recipe: \u0001 = next argument, \u0002 = next constant, anything else is literal text.
    let recipe: Vec<u16> = if with_constants {
        let Some(Constant::String(text)) = args.first().and_then(|a| class.pool.get(*a as usize))
        else {
            return Err(bad("recipe is not a String constant"));
        };
        class
            .string_units
            .get(text)
            .cloned()
            .unwrap_or_else(|| utf8(class, *text).unwrap_or("").encode_utf16().collect())
    } else {
        vec![1u16; params.len()]
    };
    let mut pool = Pool::new();
    let sb = "java/lang/StringBuilder";
    let mut asm = Asm::default();
    let sb_class = pool.class(sb);
    let sb_init = pool.method(sb, "<init>", "()V", false);
    asm.op(0xbb).u16(sb_class).op(0x59).invoke(0xb7, sb_init, 0);
    let append = |pool: &mut Pool, asm: &mut Asm, ty: &str| {
        let arg = match ty {
            "Z" | "C" | "J" | "F" | "D" => ty.to_string(),
            "B" | "S" | "I" => "I".to_string(),
            "Ljava/lang/String;" => ty.to_string(),
            "Ljava/lang/CharSequence;" => "Ljava/lang/Object;".to_string(),
            _ => "Ljava/lang/Object;".to_string(),
        };
        let m = pool.method(
            sb,
            "append",
            &format!("({arg})Ljava/lang/StringBuilder;"),
            false,
        );
        asm.invoke(0xb6, m, slots(&arg));
    };
    let mut literal: Vec<u16> = Vec::new();
    let mut next_arg = 0usize;
    let mut slot = 0u16;
    let mut next_constant = 1usize;
    let flush = |pool: &mut Pool, asm: &mut Asm, literal: &mut Vec<u16>| {
        if !literal.is_empty() {
            let s = pool.string(std::mem::take(literal));
            asm.ldc(s, false);
            let m = pool.method(
                sb,
                "append",
                "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
                false,
            );
            asm.invoke(0xb6, m, 1);
        }
    };
    for &unit in &recipe {
        match unit {
            1 => {
                flush(&mut pool, &mut asm, &mut literal);
                let ty = params
                    .get(next_arg)
                    .ok_or_else(|| bad("recipe has more arguments than the call"))?
                    .clone();
                asm.load(&ty, slot);
                slot += slots(&ty);
                append(&mut pool, &mut asm, &ty);
                next_arg += 1;
            }
            2 => {
                flush(&mut pool, &mut asm, &mut literal);
                let index = *args
                    .get(next_constant)
                    .ok_or_else(|| bad("recipe has more constants than arguments"))?;
                next_constant += 1;
                let (c, kind) = pool
                    .copy_constant(class, index)
                    .ok_or_else(|| bad("unsupported constant"))?;
                let ty = match kind {
                    'S' => "Ljava/lang/String;",
                    'I' => "I",
                    'F' => "F",
                    'J' => "J",
                    'D' => "D",
                    _ => "Ljava/lang/Object;",
                };
                asm.ldc(c, matches!(kind, 'J' | 'D'));
                append(&mut pool, &mut asm, ty);
            }
            u => literal.push(u),
        }
    }
    flush(&mut pool, &mut asm, &mut literal);
    if next_arg != params.len() {
        return Err(bad("recipe uses fewer arguments than the call passes"));
    }
    let to_string = pool.method(sb, "toString", "()Ljava/lang/String;", false);
    asm.invoke(0xb6, to_string, 0).op(0xb0);
    let methods = vec![member(
        ACC_PUBLIC | ACC_STATIC,
        "concat",
        invoked,
        Some(asm),
        slot.max(1),
    )];
    Ok(synthetic_class(name, Vec::new(), Vec::new(), methods, pool))
}

// ---- ObjectMethods (records) ----

/// `Class.getSimpleName` of a record class from its binary name: the last `$` segment, without
/// a local class's leading digits. (The InnerClasses attribute is not consulted.)
fn simple_name(internal: &str) -> String {
    let base = internal.rsplit('/').next().unwrap_or(internal);
    let tail = base.rsplit('$').next().unwrap_or(base);
    let trimmed = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    if trimmed.is_empty() { base } else { trimmed }.to_string()
}

/// A record's `equals`, `hashCode` or `toString`, as ObjectMethods.bootstrap builds them.
fn object_method(
    class: &ClassFile,
    name: &str,
    method: &str,
    invoked: &str,
    args: &[u16],
) -> Result<ClassFile, String> {
    let bad = |what: &str| format!("record {method}: {what}");
    let record = args
        .first()
        .and_then(|a| class_name(class, *a))
        .ok_or_else(|| bad("no record class"))?
        .to_string();
    let names: Vec<String> = match args.get(1).and_then(|a| class.pool.get(*a as usize)) {
        Some(Constant::String(text)) => {
            let text = utf8(class, *text).unwrap_or("");
            if text.is_empty() {
                Vec::new()
            } else {
                text.split(';').map(str::to_string).collect()
            }
        }
        _ => return Err(bad("component names are not a String constant")),
    };
    let mut getters = Vec::new();
    for a in &args[2..] {
        let (kind, owner, field, descriptor, _) =
            method_handle(class, *a).ok_or_else(|| bad("bad getter"))?;
        if kind != 1 {
            return Err(bad("a getter is not a field read"));
        }
        getters.push((owner, field, descriptor));
    }
    if getters.len() != names.len() {
        return Err(bad("names and getters differ in number"));
    }
    let mut pool = Pool::new();
    let mut asm = Asm::default();
    let get = |pool: &mut Pool, asm: &mut Asm, local: u16, g: &(String, String, String)| {
        let f = pool.field(&g.0, &g.1, &g.2);
        asm.load("L", local).op(0xb4).u16(f);
    };
    let locals;
    match method {
        "hashCode" => {
            // result = 0; for each component: result = result * 31 + hash(component)
            asm.op(0x03);
            for g in &getters {
                asm.int(&mut pool, 31).op(0x68);
                get(&mut pool, &mut asm, 0, g);
                let m = match g.2.as_str() {
                    t if is_reference(t) => pool.method(
                        "java/util/Objects",
                        "hashCode",
                        "(Ljava/lang/Object;)I",
                        false,
                    ),
                    t => pool.method(
                        wrapper_of(t).ok_or_else(|| bad("component type"))?,
                        "hashCode",
                        &format!("({t})I"),
                        false,
                    ),
                };
                asm.invoke(0xb8, m, slots(&g.2)).op(0x60);
            }
            asm.op(0xac);
            locals = 1;
        }
        "equals" => {
            // Same object: true. Not an instance: false. Then components, last first.
            let r = pool.class(&record);
            asm.op(0x2a).op(0x2b);
            let different = asm.branch(0xa6);
            asm.op(0x04).op(0xac);
            asm.land(different);
            asm.op(0x2b).op(0xc1).u16(r);
            let mut to_false = vec![asm.branch(0x99)];
            asm.op(0x2b).op(0xc0).u16(r).op(0x4d);
            for g in getters.iter().rev() {
                get(&mut pool, &mut asm, 0, g);
                get(&mut pool, &mut asm, 2, g);
                match g.2.as_str() {
                    "J" => {
                        asm.op(0x94);
                        to_false.push(asm.branch(0x9a));
                    }
                    "F" | "D" => {
                        let w = wrapper_of(&g.2).unwrap();
                        let m = pool.method(w, "compare", &format!("({0}{0})I", g.2), false);
                        asm.invoke(0xb8, m, 2 * slots(&g.2));
                        to_false.push(asm.branch(0x9a));
                    }
                    t if is_reference(t) => {
                        let m = pool.method(
                            "java/util/Objects",
                            "equals",
                            "(Ljava/lang/Object;Ljava/lang/Object;)Z",
                            false,
                        );
                        asm.invoke(0xb8, m, 2);
                        to_false.push(asm.branch(0x99));
                    }
                    _ => to_false.push(asm.branch(0xa0)),
                }
            }
            asm.op(0x04).op(0xac);
            for at in to_false {
                asm.land(at);
            }
            asm.op(0x03).op(0xac);
            locals = 3;
        }
        "toString" => {
            // Name[a=1, b=x]
            let sb = "java/lang/StringBuilder";
            let sb_class = pool.class(sb);
            let init = pool.method(sb, "<init>", "()V", false);
            asm.op(0xbb).u16(sb_class).op(0x59).invoke(0xb7, init, 0);
            let text = |pool: &mut Pool, asm: &mut Asm, s: &str| {
                let c = pool.string(s.encode_utf16().collect());
                let m = pool.method(
                    sb,
                    "append",
                    "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
                    false,
                );
                asm.ldc(c, false).invoke(0xb6, m, 1);
            };
            text(&mut pool, &mut asm, &format!("{}[", simple_name(&record)));
            for (i, (g, n)) in getters.iter().zip(&names).enumerate() {
                let prefix = if i == 0 {
                    format!("{n}=")
                } else {
                    format!(", {n}=")
                };
                text(&mut pool, &mut asm, &prefix);
                get(&mut pool, &mut asm, 0, g);
                let arg = match g.2.as_str() {
                    "Z" | "C" | "J" | "F" | "D" => g.2.clone(),
                    "B" | "S" | "I" => "I".to_string(),
                    "Ljava/lang/String;" => g.2.clone(),
                    _ => "Ljava/lang/Object;".to_string(),
                };
                let m = pool.method(
                    sb,
                    "append",
                    &format!("({arg})Ljava/lang/StringBuilder;"),
                    false,
                );
                asm.invoke(0xb6, m, slots(&arg));
            }
            text(&mut pool, &mut asm, "]");
            let m = pool.method(sb, "toString", "()Ljava/lang/String;", false);
            asm.invoke(0xb6, m, 0).op(0xb0);
            locals = 1;
        }
        _ => return Err(bad("unknown record method")),
    }
    let methods = vec![member(
        ACC_PUBLIC | ACC_STATIC,
        "invoke",
        invoked,
        Some(asm),
        locals,
    )];
    Ok(synthetic_class(name, Vec::new(), Vec::new(), methods, pool))
}

// ---- SwitchBootstraps ----

/// `typeSwitch` / `enumSwitch`: the index of the first label (from `restart`) the target matches,
/// -1 for null, the label count when none matches. Labels: classes, Strings (enum constant names
/// for enumSwitch), Integers.
fn type_switch(
    class: &ClassFile,
    name: &str,
    invoked: &str,
    args: &[u16],
    enums: bool,
) -> Result<ClassFile, String> {
    let bad = |what: &str| format!("pattern switch: {what}");
    let (params, ret) = split_method(invoked).ok_or_else(|| bad("malformed invoked type"))?;
    if params.len() != 2 || !is_reference(&params[0]) || params[1] != "I" || ret != "I" {
        return Err(bad("unexpected invoked type"));
    }
    let mut pool = Pool::new();
    let mut asm = Asm::default();
    asm.op(0x2a);
    let present = asm.branch(0xc7);
    asm.op(0x02).op(0xac);
    asm.land(present);
    for (i, a) in args.iter().enumerate() {
        let i = i as i32;
        // Labels before the restart index are skipped.
        asm.op(0x1b);
        asm.int(&mut pool, i);
        let mut next = vec![asm.branch(0xa3)];
        match class.pool.get(*a as usize) {
            Some(Constant::Class(n)) => {
                let c = pool.class(utf8(class, *n).ok_or_else(|| bad("label"))?);
                asm.op(0x2a).op(0xc1).u16(c);
                next.push(asm.branch(0x99));
            }
            Some(Constant::String(_)) => {
                let (s, _) = pool.copy_constant(class, *a).ok_or_else(|| bad("label"))?;
                asm.ldc(s, false);
                if enums {
                    let e = pool.class("java/lang/Enum");
                    let n = pool.method("java/lang/Enum", "name", "()Ljava/lang/String;", false);
                    asm.op(0x2a).op(0xc0).u16(e).invoke(0xb6, n, 0);
                } else {
                    asm.op(0x2a);
                }
                let eq = pool.method("java/lang/String", "equals", "(Ljava/lang/Object;)Z", false);
                asm.invoke(0xb6, eq, 1);
                next.push(asm.branch(0x99));
            }
            Some(Constant::Integer(v)) => {
                // A Number's intValue, or a Character's value, equal to the label.
                let number = pool.class("java/lang/Number");
                let character = pool.class("java/lang/Character");
                asm.op(0x2a).op(0xc1).u16(number);
                let not_number = asm.branch(0x99);
                let iv = pool.method("java/lang/Number", "intValue", "()I", false);
                asm.op(0x2a).op(0xc0).u16(number).invoke(0xb6, iv, 0);
                asm.int(&mut pool, *v);
                next.push(asm.branch(0xa0));
                asm.int(&mut pool, i).op(0xac);
                asm.land(not_number);
                asm.op(0x2a).op(0xc1).u16(character);
                next.push(asm.branch(0x99));
                let cv = pool.method("java/lang/Character", "charValue", "()C", false);
                asm.op(0x2a).op(0xc0).u16(character).invoke(0xb6, cv, 0);
                asm.int(&mut pool, *v);
                next.push(asm.branch(0xa0));
            }
            Some(Constant::Dynamic { .. }) => {
                // An enum constant (EnumDesc): the target is that very constant.
                let (enum_class, constant) = enum_desc(class, *a)
                    .ok_or_else(|| bad("a dynamic label other than an enum constant"))?;
                let e = pool.class(&enum_class);
                let f = pool.field(&enum_class, &constant, &format!("L{enum_class};"));
                asm.op(0x2a).op(0xc1).u16(e);
                next.push(asm.branch(0x99));
                asm.op(0x2a).op(0xb2).u16(f);
                next.push(asm.branch(0xa6));
            }
            _ => {
                return Err(bad(
                    "a label kind other than Class, String or Integer (e.g. EnumDesc)",
                ));
            }
        }
        asm.int(&mut pool, i).op(0xac);
        for at in next {
            asm.land(at);
        }
    }
    asm.int(&mut pool, args.len() as i32).op(0xac);
    let methods = vec![member(
        ACC_PUBLIC | ACC_STATIC,
        "invoke",
        invoked,
        Some(asm),
        2,
    )];
    Ok(synthetic_class(name, Vec::new(), Vec::new(), methods, pool))
}

/// A dynamic constant that is `EnumDesc.of(ClassDesc.of("p.Outer$E"), "NAME")` (how javac writes
/// enum labels): (enum class internal name, constant name).
fn enum_desc(class: &ClassFile, index: u16) -> Option<(String, String)> {
    // ConstantBootstraps.invoke(handle, args...) of a dynamic constant.
    let invoked = |index: u16| -> Option<(String, Vec<u16>)> {
        let Constant::Dynamic { bootstrap, .. } = class.pool.get(index as usize)? else {
            return None;
        };
        let (handle, args) = class.bootstrap_methods.get(*bootstrap as usize)?;
        let (_, owner, name, _, _) = method_handle(class, *handle)?;
        if owner != "java/lang/invoke/ConstantBootstraps" || name != "invoke" {
            return None;
        }
        let (_, target_owner, target, _, _) = method_handle(class, *args.first()?)?;
        Some((format!("{target_owner}.{target}"), args[1..].to_vec()))
    };
    let string = |index: u16| -> Option<String> {
        match class.pool.get(index as usize)? {
            Constant::String(t) => Some(utf8(class, *t)?.to_string()),
            _ => None,
        }
    };
    let (of, args) = invoked(index)?;
    if of != "java/lang/Enum$EnumDesc.of" || args.len() != 2 {
        return None;
    }
    let (class_of, class_args) = invoked(args[0])?;
    if class_of != "java/lang/constant/ClassDesc.of" || class_args.len() != 1 {
        return None;
    }
    Some((string(class_args[0])?.replace('.', "/"), string(args[1])?))
}
