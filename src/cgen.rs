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
    ACC_ABSTRACT, ACC_FINAL, ACC_INTERFACE, ACC_PRIVATE, ACC_STATIC, ACC_SYNCHRONIZED, ClassFile,
    Constant, Member,
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

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MethodStatus {
    Compiled {
        symbol: String,
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
    pub methods: Vec<MethodReport>,
    #[serde(skip)]
    pub c_source: String,
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
}

impl<'a> Unit<'a> {
    fn new(classes: &'a [ClassFile]) -> Self {
        Unit {
            classes: classes.iter().map(|c| (c.name.as_str(), c)).collect(),
            names: RefCell::new((Vec::new(), HashMap::new())),
            vtables: RefCell::new(HashMap::new()),
            chains: RefCell::new(HashMap::new()),
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
                self.unit.is_interface(target) || self.unit.is_subclass(name, target) == Some(true)
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
                bail!("pc {pc}: arrays in casts are not compiled yet");
            }
            if unit.get(class).is_none() || unit.is_interface(class) || unit.chain(class).is_err() {
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
        Op::InvokeVirtual(m) => {
            if m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {}", m.name);
            }
            let (declaring, target) = unit.resolve_method(&m.class, &m.name, &m.descriptor)?;
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
        Op::InvokeSpecial(m) => {
            let parent = cx.class.super_name.as_deref().unwrap_or(OBJECT);
            if m.name == "<init>" && m.class == OBJECT && m.descriptor == "()V" {
                // Only `this` in a direct subclass of Object calls Object's constructor.
                if parent != OBJECT || frame.pop(pc)? != VType::UninitThis {
                    bail!(
                        "pc {pc}: Object.<init> called on something other than uninitialized this"
                    );
                }
                frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
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

struct Analyzed {
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
    if !code.handlers.is_empty() {
        return Err("exception handlers are not compiled yet".into());
    }
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
    Ok(Analyzed {
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
    let _ = writeln!(out, "{} {{", prototype(&class.name, method, sig));
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
    let throw = |exc: &str| format!("{{ jabs_pending = \"{exc}\"; {ret_default} }}");
    let npe = throw("java/lang/NullPointerException");
    // Emit a call: arguments (and the receiver, for instance methods) come off the stack.
    let call = |kind: &CallKind, descriptor: &str, d: usize| -> String {
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
            CallKind::Virtual { target, .. } => (target, true),
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
            _ => symbol(&key.0, &key.1, &key.2),
        };
        let guard = if receiver {
            format!("if (!s[{}].a) {npe} ", base - 1)
        } else {
            init_guard(unit, &class.name, &key.0, ret_default)
        };
        let invoke = format!("{function}({})", args.join(", "));
        let assign = match kind_of(callee.ret.code) {
            Some(k) => format!("s[{result_at}].{} = {invoke};", field(k)),
            None => format!("{invoke};"),
        };
        format!("{guard}{assign} if (jabs_pending) {ret_default}")
    };
    for (insn, stack) in a.insns.iter().zip(&a.stacks) {
        if a.targets.contains(&insn.pc) {
            let _ = writeln!(out, "L{}:;", insn.pc);
        }
        let d = stack.len();
        let resolved = a.resolved.get(&insn.pc);
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
                            throw("java/lang/ArithmeticException")
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
            Op::InvokeStatic { descriptor, .. }
            | Op::InvokeVirtual(MemberRef { descriptor, .. })
            | Op::InvokeSpecial(MemberRef { descriptor, .. }) => match resolved {
                Some(Resolved::Call(CallKind::ObjectInit)) => {
                    "/* java/lang/Object.<init>()V has no effect */".into()
                }
                Some(Resolved::Call(kind)) => call(kind, descriptor, d),
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
                let guard = init_guard(unit, &class.name, declaring, ret_default);
                let sym = field_symbol(declaring, name);
                if matches!(insn.op, Op::GetStatic { .. }) {
                    format!("{guard}s[{d}].{} = {sym};", field(k))
                } else {
                    let value = narrow_for_store(descriptor, &format!("s[{}].{}", d - 1, field(k)));
                    format!("{guard}{sym} = {value};")
                }
            }
            Op::New { class: target } => format!(
                "{}s[{d}].a = jabs_new(&{}); if (!s[{d}].a) {ret_default}",
                init_guard(unit, &class.name, target, ret_default),
                class_record(target)
            ),
            Op::NewArray(element) => {
                let r = d - 1;
                format!(
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof({}), JABS_COMPONENT_PRIMITIVE, NULL, \"[{element}\", NULL); if (!s[{r}].a) {ret_default}",
                    throw("java/lang/NegativeArraySizeException"),
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
                    && !unit.is_interface(component)
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
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof(jabs_object *), {kind}, {record}, \"[{}\", {component_descriptor}); if (!s[{r}].a) {ret_default}",
                    throw("java/lang/NegativeArraySizeException"),
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
                    throw("java/lang/ArrayIndexOutOfBoundsException")
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
                                    throw("java/lang/ArrayStoreException")
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
                throw("java/lang/ClassCastException"),
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
    let _ = writeln!(out, "}}\n");
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

/// C name of the stub that stands in for an implementation that is not compiled.
fn stub_symbol(key: &MethodKey) -> String {
    mangled("jx_", &[&key.0, &key.1, &key.2])
}

/// Emit typed stubs for vtable implementations that did not compile. Abstract methods throw
/// AbstractMethodError, as Java does; anything else stops with a diagnostic (the JVM backend
/// must run the profile until that method compiles). Stubs keep each class's code independent
/// of which subclasses exist, so per-class caching stays valid when mods add subclasses.
fn emit_stubs(
    out: &mut String,
    unit: &Unit,
    laid_out: &[&ClassFile],
    compiled: &BTreeSet<MethodKey>,
) {
    let mut done = BTreeSet::new();
    for class in laid_out {
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
            if compiled.contains(&key) || !done.insert(key.clone()) {
                continue;
            }
            // Calls with unparsable descriptors (arrays) never compile, so their slots stay NULL.
            let Ok(sig) = parse_signature(&key.2) else {
                continue;
            };
            let is_abstract = unit
                .get(&key.0)
                .and_then(|c| {
                    c.methods
                        .iter()
                        .find(|m| m.name == key.1 && m.descriptor == key.2)
                })
                .is_some_and(|m| m.access & ACC_ABSTRACT != 0);
            let body = if is_abstract {
                let ret = if sig.ret.code == 'V' {
                    "return;"
                } else {
                    "return 0;"
                };
                format!("jabs_pending = \"java/lang/AbstractMethodError\"; {ret}")
            } else {
                format!("jabs_not_compiled(\"{}.{}{}\");", key.0, key.1, key.2)
            };
            let unused: String = (0..sig.params.len())
                .map(|i| format!(" (void)a{i};"))
                .collect();
            let _ = writeln!(
                out,
                "static {} {}({}) {{ (void)self;{unused} {body} }}",
                c_type(sig.ret.code),
                stub_symbol(&key),
                c_params(false, &sig, true)
            );
        }
    }
    out.push('\n');
}

/// Emit a class's vtable and runtime record; implementations that did not compile are stubs.
fn emit_class_record(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    compiled: &BTreeSet<MethodKey>,
) {
    let vtable_name = mangled("jv_", &[&class.name]);
    let slots = unit
        .vtable(&class.name)
        .map(|v| (*v).clone())
        .unwrap_or_default();
    let entries: Vec<String> = slots
        .iter()
        .map(|s| {
            let key = (s.owner.clone(), s.name.clone(), s.descriptor.clone());
            if compiled.contains(&key) {
                format!("(void (*)(void)){}", symbol(&key.0, &key.1, &key.2))
            } else if parse_signature(&key.2).is_ok() {
                format!("(void (*)(void)){}", stub_symbol(&key))
            } else {
                "0".into()
            }
        })
        .collect();
    let entries = if entries.is_empty() {
        "0".to_string()
    } else {
        entries.join(", ")
    };
    let parent = match class.super_name.as_deref() {
        Some(p) if p != OBJECT => format!("&{}", class_record(p)),
        _ => "NULL".to_string(),
    };
    // External linkage: records are referenced across the unit and must not trip unused warnings.
    let _ = writeln!(
        out,
        "void (*const {vtable_name}[])(void) = {{ {entries} }};"
    );
    let _ = writeln!(
        out,
        "const jabs_class {} = {{ \"{}\", sizeof({}), {vtable_name}, {parent} }};\n",
        class_record(&class.name),
        class.name,
        object_struct(&class.name)
    );
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
    let mut constants = String::new();
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
        let Some(ty) = storage_type(&f.descriptor) else {
            continue;
        };
        let sym = field_symbol(&class.name, &f.name);
        let _ = writeln!(out, "static {ty} {sym};");
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
        }
    }
    let init = init_symbol(&class.name);
    let state = format!("{init}_state");
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
        "    if ({state} == 3) {{ jabs_pending = \"java/lang/NoClassDefFoundError\"; return -1; }}"
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

/// Compile a set of classes together into one C unit.
pub fn compile_unit(classes: &[ClassFile]) -> CompiledUnit {
    let unit = Unit::new(classes);
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
                if method.access & ACC_SYNCHRONIZED != 0 {
                    return Err("synchronized methods need monitors".into());
                }
                if class.access & ACC_INTERFACE != 0 && method.access & ACC_STATIC == 0 {
                    return Err("interface instance methods are not compiled yet".into());
                }
                let sig = parse_signature(&method.descriptor).map_err(|e| e.to_string())?;
                let analyzed = analyze(&unit, class, method, &sig)?;
                Ok((sig, analyzed))
            })();
            results.insert(key, result);
        }
    }
    // A class initializes only if its hierarchy is in the unit, its <clinit> compiles and its
    // prerequisites initialize; a method compiles only if every dependency does. Failures only
    // spread, so iterating until nothing changes terminates.
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
        let failed: Vec<(MethodKey, String)> = results
            .iter()
            .filter_map(|(key, r)| {
                let a = &r.as_ref().ok()?.1;
                // Every method runs only after its own class is initialized.
                let own = Dep::Init(key.0.clone());
                std::iter::once(&own)
                    .chain(a.deps.iter())
                    .find_map(|dep| match dep {
                        Dep::Method(m) => match results.get(m) {
                            Some(Ok(_)) => None,
                            _ => Some((
                                key.clone(),
                                format!("calls {}.{}{}, which is not compiled", m.0, m.1, m.2),
                            )),
                        },
                        Dep::Init(c) if *c == key.0 && key.1 == "<clinit>" => None,
                        Dep::Init(c) => match init_ok.get(c) {
                            Some(Ok(())) => None,
                            Some(Err(reason)) if *c == key.0 => Some((key.clone(), reason.clone())),
                            Some(Err(_)) => Some((
                                key.clone(),
                                format!("initializes {c}, which cannot be initialized"),
                            )),
                            None => Some((
                                key.clone(),
                                format!("initializes {c}, which is not in the compilation unit"),
                            )),
                        },
                    })
            })
            .collect();
        for (key, reason) in failed {
            results.insert(key, Err(reason));
            changed = true;
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
    let initializable: Vec<&ClassFile> = classes
        .iter()
        .filter(|c| init_ok[&c.name].is_ok())
        .collect();

    let mut source = String::new();
    let _ = writeln!(
        source,
        "/* Generated by JABS from {} classes. */",
        classes.len()
    );
    source.push_str(RUNTIME_PRELUDE);
    source.push('\n');
    // Layouts and records for every class whose hierarchy is complete, superclasses first.
    let mut laid_out: Vec<&ClassFile> = classes
        .iter()
        .filter(|c| c.access & ACC_INTERFACE == 0 && unit.chain(&c.name).is_ok())
        .collect();
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
            if let Some(Ok((sig, _))) = results.get(&(
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            )) {
                let _ = writeln!(source, "{};", prototype(&class.name, method, sig));
            }
        }
    }
    for class in &initializable {
        let _ = writeln!(source, "int {}(void);", init_symbol(&class.name));
    }
    source.push('\n');
    emit_stubs(&mut source, &unit, &laid_out, &compiled);
    for class in &laid_out {
        emit_class_record(&mut source, &unit, class, &compiled);
    }
    for class in &initializable {
        emit_statics_and_init(&mut source, &unit, class);
    }
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
                    emit_method(&mut source, &unit, class, method, sig, analyzed);
                    MethodStatus::Compiled {
                        symbol: symbol(&key.0, &key.1, &key.2),
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
    CompiledUnit {
        schema_version: 1,
        classes: classes.len(),
        compiled,
        unsupported: methods.len() - compiled - abstract_methods,
        abstract_methods,
        methods,
        c_source: source,
    }
}
