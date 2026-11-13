//! Per-class C emission for the compilable subset.
//!
//! A method compiles only when every instruction is understood, a small type
//! verifier proves every path type-correct (references included, since they
//! become C pointers), and every callee also compiles. All other methods are
//! reported with a specific reason; nothing is guessed.
//!
//! Object model (first slice): objects of the class being compiled only, a
//! header pointing at a class record with a vtable, fields at JVM widths.
//! Objects are never freed yet (no garbage collector).

use crate::bytecode::{
    self, BinOp, Compare, Cond, Convert, DecodeError, Insn, Kind, MemberRef, Op, Value,
};
use crate::classfile::{
    ACC_ABSTRACT, ACC_FINAL, ACC_INTERFACE, ACC_PRIVATE, ACC_STATIC, ACC_SYNCHRONIZED, ClassFile,
    Constant, Member,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write,
};

pub const RUNTIME_PRELUDE: &str = include_str!("../runtime/jabs_rt.h");

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MethodStatus {
    Compiled { symbol: String },
    Unsupported { reason: String },
}

#[derive(Debug, Serialize)]
pub struct MethodReport {
    pub name: String,
    pub descriptor: String,
    #[serde(flatten)]
    pub status: MethodStatus,
}

#[derive(Debug, Serialize)]
pub struct CompiledClass {
    pub schema_version: u32,
    pub class: String,
    pub compiled: usize,
    pub unsupported: usize,
    pub methods: Vec<MethodReport>,
    #[serde(skip)]
    pub c_source: String,
}

/// Parameter and return types of a method descriptor in the compilable subset.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    /// Descriptor characters I J F D Z B C S, or `L` for a reference to the class being compiled.
    pub params: Vec<char>,
    /// Same encoding, plus `V` for void.
    pub ret: char,
}

/// Stack kind for a signature char; boolean/byte/char/short all live as int.
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

/// Map one field/parameter descriptor to a signature char; only this class's references are allowed.
fn type_char(descriptor: &str, method: &str, this_class: &str) -> Result<char> {
    match descriptor.as_bytes() {
        [c] if b"IJFDZBCS".contains(c) => Ok(*c as char),
        [b'L', .., b';'] if &descriptor[1..descriptor.len() - 1] == this_class => Ok('L'),
        [b'L', ..] => bail!(
            "reference types in {method}: {descriptor} is another class; only references to the class being compiled are supported yet"
        ),
        [b'[', ..] => bail!("arrays in {method} are not compiled yet"),
        _ => bail!("invalid descriptor {descriptor} in {method}"),
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

/// Parse a method descriptor whose reference types may only name `this_class`.
pub fn parse_signature_in(descriptor: &str, this_class: &str) -> Result<Signature> {
    let Some((params, ret)) = descriptor
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
    else {
        bail!("invalid method descriptor {descriptor}")
    };
    let params = split_params(params)?
        .into_iter()
        .map(|p| type_char(p, descriptor, this_class))
        .collect::<Result<Vec<_>>>()?;
    let ret = if ret == "V" {
        'V'
    } else {
        type_char(ret, descriptor, this_class)?
    };
    Ok(Signature { params, ret })
}

/// Parse a primitive-only method descriptor.
pub fn parse_signature(descriptor: &str) -> Result<Signature> {
    parse_signature_in(descriptor, "")
}

/// C type used for a signature char in generated prototypes (`void` for V).
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

/// C name of an instance field inside the object struct.
fn instance_field(name: &str) -> String {
    mangled("f_", &[name])
}

/// C storage type for a field, sized like the JVM's (so byte fields really hold a byte).
fn storage_type(descriptor: &str, this_class: &str) -> Option<&'static str> {
    Some(match descriptor {
        "Z" => "uint8_t",
        "B" => "int8_t",
        "C" => "uint16_t",
        "S" => "int16_t",
        "I" => "int32_t",
        "J" => "int64_t",
        "F" => "float",
        "D" => "double",
        d if d.len() > 2 && d.starts_with('L') && &d[1..d.len() - 1] == this_class => {
            "jabs_object *"
        }
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

/// Find a field declared by this class with the given staticness, or explain why the access cannot be compiled.
fn own_field<'a>(
    class: &'a ClassFile,
    m: &MemberRef,
    want_static: bool,
    pc: u32,
) -> Result<&'a Member> {
    if m.class != class.name {
        bail!(
            "pc {pc}: field {}.{} in another class is not compiled yet",
            m.class,
            m.name
        );
    }
    let Some(field) = class
        .fields
        .iter()
        .find(|f| f.name == m.name && f.descriptor == m.descriptor)
    else {
        bail!(
            "pc {pc}: field {} is inherited; cross-class field resolution is not compiled yet",
            m.name
        );
    };
    if (field.access & ACC_STATIC != 0) != want_static {
        bail!(
            "pc {pc}: field {} has the wrong staticness for this instruction",
            m.name
        );
    }
    if storage_type(&m.descriptor, &class.name).is_none() {
        bail!(
            "pc {pc}: field {} of type {} is not compiled yet (only primitives and this class's references)",
            m.name,
            m.descriptor
        );
    }
    Ok(field)
}

/// Find a method declared by this class, or explain why the call cannot be compiled.
fn own_method<'a>(class: &'a ClassFile, m: &MemberRef, pc: u32) -> Result<&'a Member> {
    if m.class != class.name {
        bail!(
            "pc {pc}: call to {}.{}{} outside this class is not compiled yet",
            m.class,
            m.name,
            m.descriptor
        );
    }
    class
        .methods
        .iter()
        .find(|x| x.name == m.name && x.descriptor == m.descriptor)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "pc {pc}: {}{} is inherited (e.g. from java/lang/Object); cross-class calls are not compiled yet",
                m.name,
                m.descriptor
            )
        })
}

/// Verification type of a local variable or stack slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VType {
    /// Unusable (unset, or the second half of a long/double, or merged incompatible types).
    Top,
    I,
    J,
    F,
    D,
    Null,
    /// Initialized reference to an instance of the class being compiled.
    Obj,
    /// `this` inside a constructor before the superclass constructor has run.
    UninitThis,
    /// Result of `new` at this pc, before its constructor has run.
    Uninit(u32),
}

impl VType {
    /// Verification type of a primitive kind.
    fn of(kind: Kind) -> VType {
        match kind {
            Kind::I => VType::I,
            Kind::J => VType::J,
            Kind::F => VType::F,
            Kind::D => VType::D,
            Kind::A => VType::Obj,
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
            VType::Null | VType::Obj | VType::UninitThis | VType::Uninit(_)
        )
    }
    /// Least common type at a control-flow join: null merges into a reference; anything else must match.
    fn merge(self, other: VType) -> Option<VType> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (VType::Null, VType::Obj) | (VType::Obj, VType::Null) => Some(VType::Obj),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Frame {
    stack: Vec<VType>,
    locals: Vec<VType>,
}

impl Frame {
    fn pop(&mut self, pc: u32) -> Result<VType> {
        self.stack
            .pop()
            .ok_or_else(|| anyhow::anyhow!("pc {pc}: stack underflow"))
    }
    /// Pop one value and require it to have exactly the primitive type `want`.
    fn pop_prim(&mut self, want: Kind, pc: u32) -> Result<()> {
        match self.pop(pc)? {
            v if v == VType::of(want) && want != Kind::A => Ok(()),
            other => bail!("pc {pc}: expected {want:?} on stack, found {other:?}"),
        }
    }
    /// Pop an initialized reference or null (what calls, field reads and returns accept).
    fn pop_ref(&mut self, pc: u32) -> Result<VType> {
        match self.pop(pc)? {
            v @ (VType::Obj | VType::Null) => Ok(v),
            other => bail!("pc {pc}: expected an initialized reference, found {other:?}"),
        }
    }
    /// Pop a value for a signature char (`L` accepts this class's references or null).
    fn pop_param(&mut self, c: char, pc: u32) -> Result<()> {
        match kind_of(c).unwrap() {
            Kind::A => self.pop_ref(pc).map(|_| ()),
            k => self.pop_prim(k, pc),
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
            .ok_or_else(|| anyhow::anyhow!("pc {pc}: local {n} out of range"))
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
    /// After a constructor call, every copy of the uninitialized object becomes initialized.
    fn initialize(&mut self, uninit: VType) {
        for v in self.stack.iter_mut().chain(self.locals.iter_mut()) {
            if *v == uninit {
                *v = VType::Obj;
            }
        }
    }
    /// Merge `other` into `self`; stack shapes must agree, locals that disagree become unusable.
    fn merge(&self, other: &Frame, pc: u32) -> Result<Frame> {
        if self.stack.len() != other.stack.len() {
            bail!("pc {pc}: stack depth differs between paths");
        }
        let stack = self
            .stack
            .iter()
            .zip(&other.stack)
            .map(|(a, b)| {
                a.merge(*b)
                    .ok_or_else(|| anyhow::anyhow!("pc {pc}: stack types {a:?} and {b:?} disagree"))
            })
            .collect::<Result<_>>()?;
        let locals = self
            .locals
            .iter()
            .zip(&other.locals)
            .map(|(a, b)| a.merge(*b).unwrap_or(VType::Top))
            .collect();
        Ok(Frame { stack, locals })
    }
}

/// How an `invokespecial` compiles.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Special {
    /// A constructor or private method of this class: a direct call.
    Direct,
    /// `java/lang/Object.<init>()V`, which has no effect.
    ObjectInit,
}

/// Facts about the method being analyzed that instruction checks need.
struct Context<'a> {
    class: &'a ClassFile,
    sig: &'a Signature,
    is_constructor: bool,
}

/// What one instruction does to control flow.
struct Step {
    /// Execution can continue with the next instruction.
    falls: bool,
    /// Branch targets.
    targets: Vec<u32>,
    /// Method of this class it calls, as (name, descriptor).
    callee: Option<(String, String)>,
}

/// Apply one instruction to the frame and report its control flow.
fn step(
    insn: &Insn,
    frame: &mut Frame,
    cx: &Context,
    specials: &mut HashMap<u32, Special>,
) -> Result<Step> {
    let pc = insn.pc;
    let class = cx.class;
    let mut falls = true;
    let mut targets = Vec::new();
    let mut callee = None;
    match &insn.op {
        Op::Nop => {}
        Op::Goto(t) => {
            falls = false;
            targets.push(*t);
        }
        Op::Const(v) => frame.stack.push(VType::of(v.kind())),
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
            if v != VType::of(*k) {
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
            frame.set_local(*n, VType::of(*k), pc)?;
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
            frame.stack.push(VType::of(*k));
        }
        Op::Neg(k) => {
            frame.pop_prim(*k, pc)?;
            frame.stack.push(VType::of(*k));
        }
        Op::Convert(c) => {
            let (from, to) = c.kinds();
            frame.pop_prim(from, pc)?;
            frame.stack.push(VType::of(to));
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
            targets.push(*t);
        }
        Op::IfICmp(_, t) => {
            frame.pop_prim(Kind::I, pc)?;
            frame.pop_prim(Kind::I, pc)?;
            targets.push(*t);
        }
        Op::IfNull { target, .. } => {
            frame.pop_ref(pc)?;
            targets.push(*target);
        }
        Op::IfACmp { target, .. } => {
            frame.pop_ref(pc)?;
            frame.pop_ref(pc)?;
            targets.push(*target);
        }
        Op::TableSwitch {
            default,
            targets: ts,
            ..
        } => {
            frame.pop_prim(Kind::I, pc)?;
            falls = false;
            targets.push(*default);
            targets.extend(ts);
        }
        Op::LookupSwitch { default, pairs } => {
            frame.pop_prim(Kind::I, pc)?;
            falls = false;
            targets.push(*default);
            targets.extend(pairs.iter().map(|(_, t)| *t));
        }
        Op::Return(k) => {
            if *k != kind_of(cx.sig.ret) {
                bail!(
                    "pc {pc}: return kind {k:?} does not match descriptor return {}",
                    cx.sig.ret
                );
            }
            match k {
                Some(Kind::A) => {
                    frame.pop_ref(pc)?;
                }
                Some(k) => frame.pop_prim(*k, pc)?,
                None => {}
            }
            if cx.is_constructor && frame.locals.first() == Some(&VType::UninitThis) {
                bail!("pc {pc}: constructor returns before calling a superclass constructor");
            }
            falls = false;
        }
        Op::InvokeStatic {
            class: owner,
            name,
            descriptor,
        } => {
            let m = MemberRef {
                class: owner.clone(),
                name: name.clone(),
                descriptor: descriptor.clone(),
            };
            let target = own_method(class, &m, pc)?;
            if target.access & ACC_STATIC == 0 {
                bail!("pc {pc}: invokestatic of instance method {name}");
            }
            let s = parse_signature_in(descriptor, &class.name)
                .map_err(|e| anyhow::anyhow!("pc {pc}: {e}"))?;
            for &p in s.params.iter().rev() {
                frame.pop_param(p, pc)?;
            }
            if let Some(k) = kind_of(s.ret) {
                frame.stack.push(VType::of(k));
            }
            callee = Some((name.clone(), descriptor.clone()));
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
            let m = MemberRef {
                class: owner.clone(),
                name: name.clone(),
                descriptor: descriptor.clone(),
            };
            own_field(class, &m, true, pc)?;
            let k = kind_of(type_char(descriptor, name, &class.name)?).unwrap();
            if matches!(insn.op, Op::GetStatic { .. }) {
                frame.stack.push(VType::of(k));
            } else if k == Kind::A {
                frame.pop_ref(pc)?;
            } else {
                frame.pop_prim(k, pc)?;
            }
        }
        Op::New { class: target } => {
            if *target != class.name {
                bail!("pc {pc}: new {target}: objects of other classes are not compiled yet");
            }
            if class.access & (ACC_ABSTRACT | ACC_INTERFACE) != 0 {
                bail!("pc {pc}: new of an abstract class or interface");
            }
            frame.stack.push(VType::Uninit(pc));
        }
        Op::GetField(m) => {
            own_field(class, m, false, pc)?;
            frame.pop_ref(pc)?;
            let k = kind_of(type_char(&m.descriptor, &m.name, &class.name)?).unwrap();
            frame.stack.push(VType::of(k));
        }
        Op::PutField(m) => {
            own_field(class, m, false, pc)?;
            let k = kind_of(type_char(&m.descriptor, &m.name, &class.name)?).unwrap();
            if k == Kind::A {
                frame.pop_ref(pc)?;
            } else {
                frame.pop_prim(k, pc)?;
            }
            // Constructors may assign their own fields before calling super().
            match frame.pop(pc)? {
                VType::Obj | VType::Null => {}
                VType::UninitThis if cx.is_constructor => {}
                other => bail!("pc {pc}: putfield on {other:?}"),
            }
        }
        Op::InvokeVirtual(m) => {
            let target = own_method(class, m, pc)?;
            if target.access & ACC_STATIC != 0 || m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {}", m.name);
            }
            let s = parse_signature_in(&m.descriptor, &class.name)
                .map_err(|e| anyhow::anyhow!("pc {pc}: {e}"))?;
            for &p in s.params.iter().rev() {
                frame.pop_param(p, pc)?;
            }
            frame.pop_ref(pc)?;
            if let Some(k) = kind_of(s.ret) {
                frame.stack.push(VType::of(k));
            }
            callee = Some((m.name.clone(), m.descriptor.clone()));
        }
        Op::InvokeSpecial(m) => {
            let object_init =
                m.class == "java/lang/Object" && m.name == "<init>" && m.descriptor == "()V";
            if object_init && class.super_name.as_deref() == Some("java/lang/Object") {
                // Only `this` in a constructor may call the superclass constructor.
                if frame.pop(pc)? != VType::UninitThis {
                    bail!(
                        "pc {pc}: Object.<init> called on something other than uninitialized this"
                    );
                }
                frame.initialize(VType::UninitThis);
                specials.insert(pc, Special::ObjectInit);
                return Ok(Step {
                    falls,
                    targets,
                    callee,
                });
            }
            let target = own_method(class, m, pc)?;
            let s = parse_signature_in(&m.descriptor, &class.name)
                .map_err(|e| anyhow::anyhow!("pc {pc}: {e}"))?;
            for &p in s.params.iter().rev() {
                frame.pop_param(p, pc)?;
            }
            if m.name == "<init>" {
                match frame.pop(pc)? {
                    v @ VType::Uninit(_) => frame.initialize(v),
                    VType::UninitThis if cx.is_constructor => frame.initialize(VType::UninitThis),
                    other => bail!("pc {pc}: constructor called on {other:?}"),
                }
            } else if target.access & ACC_PRIVATE != 0 && target.access & ACC_STATIC == 0 {
                frame.pop_ref(pc)?;
                if let Some(k) = kind_of(s.ret) {
                    frame.stack.push(VType::of(k));
                }
            } else {
                bail!(
                    "pc {pc}: invokespecial of {} (super calls) is not compiled yet",
                    m.name
                );
            }
            specials.insert(pc, Special::Direct);
            callee = Some((m.name.clone(), m.descriptor.clone()));
        }
    }
    Ok(Step {
        falls,
        targets,
        callee,
    })
}

struct Analyzed {
    insns: Vec<Insn>,
    /// Operand stack types before each instruction.
    stacks: Vec<Vec<VType>>,
    targets: BTreeSet<u32>,
    callees: BTreeSet<(String, String)>,
    specials: HashMap<u32, Special>,
}

/// Decode a method and verify every reachable path, or explain why it cannot be compiled.
fn analyze(
    class: &ClassFile,
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
    let is_static = method.access & ACC_STATIC != 0;
    let is_constructor = method.name == "<init>";
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
    if !is_static {
        place(
            if is_constructor {
                VType::UninitThis
            } else {
                VType::Obj
            },
            &mut entry,
        )?;
    }
    for &p in &sig.params {
        place(VType::of(kind_of(p).unwrap()), &mut entry)?;
    }

    let index: HashMap<u32, usize> = insns
        .iter()
        .enumerate()
        .map(|(i, insn)| (insn.pc, i))
        .collect();
    let cx = Context {
        class,
        sig,
        is_constructor,
    };
    let mut frames: Vec<Option<Frame>> = vec![None; insns.len()];
    let mut targets = BTreeSet::new();
    let mut callees = BTreeSet::new();
    let mut specials = HashMap::new();
    let mut work = vec![(0usize, entry)];
    while let Some((i, incoming)) = work.pop() {
        let pc = insns[i].pc;
        let frame = match &frames[i] {
            None => incoming,
            Some(existing) => {
                let merged = existing
                    .merge(&incoming, pc)
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
        let Step {
            falls,
            targets: succ,
            callee,
        } = step(&insns[i], &mut next, &cx, &mut specials).map_err(|e| format!("{e:#}"))?;
        callees.extend(callee);
        for t in succ {
            let Some(&j) = index.get(&t) else {
                return Err(format!("pc {pc}: branch target {t} is not an instruction"));
            };
            targets.insert(t);
            work.push((j, next.clone()));
        }
        if falls {
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
        callees,
        specials,
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
fn c_params(method: &Member, sig: &Signature, names: bool) -> String {
    let mut params = Vec::new();
    if method.access & ACC_STATIC == 0 {
        params.push(if names {
            "jabs_object *self".to_string()
        } else {
            "jabs_object *".to_string()
        });
    }
    for (i, &p) in sig.params.iter().enumerate() {
        params.push(if names {
            format!("{} a{i}", c_type(p))
        } else {
            c_type(p).to_string()
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
        c_type(sig.ret),
        symbol(class, &method.name, &method.descriptor),
        c_params(method, sig, true)
    )
}

/// Everything emission needs about the class beyond one method.
struct ClassLayout {
    /// Virtual method (name, descriptor) -> vtable slot.
    vtable: HashMap<(String, String), usize>,
}

/// Emit one analyzed method as C: locals and stack become `jabs_value` arrays, pcs become labels.
fn emit_method(
    out: &mut String,
    class: &ClassFile,
    layout: &ClassLayout,
    method: &Member,
    sig: &Signature,
    a: &Analyzed,
) {
    let code = method.code.as_ref().unwrap();
    let ret_default = if sig.ret == 'V' {
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
    for (i, &p) in sig.params.iter().enumerate() {
        let k = kind_of(p).unwrap();
        let _ = writeln!(out, "    l[{slot}].{} = a{i};", field(k));
        slot += k.category() as usize;
    }
    let throw = |exc: &str| format!("{{ jabs_pending = \"{exc}\"; {ret_default} }}");
    let npe = throw("java/lang/NullPointerException");
    let obj = object_struct(&class.name);
    // Call `name descriptor` with the receiver (if any) and arguments taken from the stack.
    let call = |name: &str,
                descriptor: &str,
                d: usize,
                receiver: bool,
                virtual_slot: Option<usize>|
     -> String {
        let callee = parse_signature_in(descriptor, &class.name).unwrap();
        let base = d - callee.params.len();
        let mut args: Vec<String> = Vec::new();
        let result_at = if receiver { base - 1 } else { base };
        if receiver {
            args.push(format!("s[{}].a", base - 1));
        }
        args.extend(
            callee
                .params
                .iter()
                .enumerate()
                .map(|(i, &p)| format!("s[{}].{}", base + i, field(kind_of(p).unwrap()))),
        );
        let function = match virtual_slot {
            Some(slot) => {
                let target = class
                    .methods
                    .iter()
                    .find(|m| m.name == name && m.descriptor == descriptor)
                    .unwrap();
                format!(
                    "(({} (*)({}))s[{}].a->cls->vtable[{slot}])",
                    c_type(callee.ret),
                    c_params(target, &callee, false),
                    base - 1
                )
            }
            None => symbol(&class.name, name, descriptor),
        };
        let invoke = format!("{function}({})", args.join(", "));
        let null_check = if receiver {
            format!("if (!s[{}].a) {npe} ", base - 1)
        } else {
            String::new()
        };
        let assign = match kind_of(callee.ret) {
            Some(k) => format!("s[{result_at}].{} = {invoke};", field(k)),
            None => format!("{invoke};"),
        };
        format!("{null_check}{assign} if (jabs_pending) {ret_default}")
    };
    for (insn, stack) in a.insns.iter().zip(&a.stacks) {
        if a.targets.contains(&insn.pc) {
            let _ = writeln!(out, "L{}:;", insn.pc);
        }
        let d = stack.len();
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
                match sig.ret {
                    'Z' => format!("return {v} & 1;"),
                    'B' => format!("return jabs_i2b({v});"),
                    'C' => format!("return jabs_i2c({v});"),
                    'S' => format!("return jabs_i2s({v});"),
                    _ => format!("return {v};"),
                }
            }
            Op::InvokeStatic {
                name, descriptor, ..
            } => call(name, descriptor, d, false, None),
            Op::GetStatic {
                name, descriptor, ..
            } => {
                let k = kind_of(type_char(descriptor, name, &class.name).unwrap()).unwrap();
                format!("s[{d}].{} = {};", field(k), field_symbol(&class.name, name))
            }
            Op::PutStatic {
                name, descriptor, ..
            } => {
                let k = kind_of(type_char(descriptor, name, &class.name).unwrap()).unwrap();
                let value = narrow_for_store(descriptor, &format!("s[{}].{}", d - 1, field(k)));
                format!("{} = {value};", field_symbol(&class.name, name))
            }
            Op::New { .. } => {
                format!(
                    "s[{d}].a = jabs_new(&{}); if (!s[{d}].a) {ret_default}",
                    mangled("jk_", &[&class.name])
                )
            }
            Op::GetField(m) => {
                let k = kind_of(type_char(&m.descriptor, &m.name, &class.name).unwrap()).unwrap();
                let r = d - 1;
                format!(
                    "if (!s[{r}].a) {npe} s[{r}].{} = (({obj} *)s[{r}].a)->{};",
                    field(k),
                    instance_field(&m.name)
                )
            }
            Op::PutField(m) => {
                let k = kind_of(type_char(&m.descriptor, &m.name, &class.name).unwrap()).unwrap();
                let (r, v) = (d - 2, d - 1);
                let value = narrow_for_store(&m.descriptor, &format!("s[{v}].{}", field(k)));
                format!(
                    "if (!s[{r}].a) {npe} (({obj} *)s[{r}].a)->{} = {value};",
                    instance_field(&m.name)
                )
            }
            Op::InvokeVirtual(m) => {
                let target = class
                    .methods
                    .iter()
                    .find(|x| x.name == m.name && x.descriptor == m.descriptor)
                    .unwrap();
                // Private, final and final-class methods cannot be overridden: call them directly.
                let fixed =
                    target.access & (ACC_PRIVATE | ACC_FINAL) != 0 || class.access & ACC_FINAL != 0;
                let slot = if fixed {
                    None
                } else {
                    layout
                        .vtable
                        .get(&(m.name.clone(), m.descriptor.clone()))
                        .copied()
                };
                call(&m.name, &m.descriptor, d, true, slot)
            }
            Op::InvokeSpecial(m) => match a.specials[&insn.pc] {
                Special::ObjectInit => "/* java/lang/Object.<init>()V has no effect */".into(),
                Special::Direct => call(&m.name, &m.descriptor, d, true, None),
            },
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

/// Emit the object layout struct: the header, then every instance field at its JVM width.
fn emit_object_layout(out: &mut String, class: &ClassFile) {
    let _ = writeln!(
        out,
        "{} {{\n    jabs_object header;",
        object_struct(&class.name)
    );
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC == 0) {
        match storage_type(&f.descriptor, &class.name) {
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

/// Emit the vtable and class record. Only compiled methods get entries; the rest stay NULL and
/// are never called, because a call compiles only when its target method compiles.
fn emit_class_record(
    out: &mut String,
    class: &ClassFile,
    virtuals: &[&Member],
    compiled: &BTreeSet<(String, String)>,
) {
    let vtable = mangled("jv_", &[&class.name]);
    let entries: Vec<String> = virtuals
        .iter()
        .map(|m| {
            if compiled.contains(&(m.name.clone(), m.descriptor.clone())) {
                format!(
                    "(void (*)(void)){}",
                    symbol(&class.name, &m.name, &m.descriptor)
                )
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
    let _ = writeln!(
        out,
        "static void (*const {vtable}[])(void) = {{ {entries} }};"
    );
    let _ = writeln!(
        out,
        "static const jabs_class {} = {{ \"{}\", sizeof({}), {vtable} }};\n",
        mangled("jk_", &[&class.name]),
        class.name,
        object_struct(&class.name)
    );
}

/// Emit static field storage and the class initialization function (JVMS 5.5, single-threaded).
fn emit_statics_and_init(out: &mut String, class: &ClassFile, has_clinit: bool) {
    let mut constants = String::new();
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
        let Some(ty) = storage_type(&f.descriptor, &class.name) else {
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
    let _ = writeln!(out, "int {init}(void);");
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
    out.push_str(&constants);
    if has_clinit {
        let _ = writeln!(out, "    {}();", symbol(&class.name, "<clinit>", "()V"));
        let _ = writeln!(
            out,
            "    if (jabs_pending) {{ jabs_pending = jabs_initializer_failure(jabs_pending); {state} = 3; return -1; }}"
        );
    }
    let _ = writeln!(out, "    {state} = 2;");
    let _ = writeln!(out, "    return 0;\n}}\n");
}

/// Compile every eligible method of one class to a self-contained C unit.
pub fn compile_class(class: &ClassFile) -> CompiledClass {
    let clinit_key = ("<clinit>".to_string(), "()V".to_string());
    let has_clinit = class.methods.iter().any(|m| m.name == "<clinit>");
    // JVMS 5.5: initializing a class first initializes its superclass and any superinterface
    // with default methods. Those live in other classes, which are not compiled together yet.
    let foreign_init = match class.super_name.as_deref() {
        Some("java/lang/Object") if class.interfaces.is_empty() => None,
        Some("java/lang/Object") => Some(format!(
            "initialization may first run superinterface initializers ({}); cross-class initialization is not compiled yet",
            class.interfaces.join(", ")
        )),
        Some(parent) => Some(format!(
            "initialization first runs superclass {parent}'s initializer; cross-class initialization is not compiled yet"
        )),
        None => Some("java/lang/Object itself is provided by the class library".to_string()),
    };
    let mut results: BTreeMap<
        (String, String),
        std::result::Result<(Signature, Analyzed), String>,
    > = BTreeMap::new();
    for method in &class.methods {
        let key = (method.name.clone(), method.descriptor.clone());
        let result = (|| {
            if let Some(reason) = &foreign_init {
                return Err(reason.clone());
            }
            if method.access & ACC_SYNCHRONIZED != 0 {
                return Err("synchronized methods need monitors".into());
            }
            if class.access & ACC_INTERFACE != 0 && method.access & ACC_STATIC == 0 {
                return Err("interface instance methods are not compiled yet".into());
            }
            let sig =
                parse_signature_in(&method.descriptor, &class.name).map_err(|e| e.to_string())?;
            let analyzed = analyze(class, method, &sig)?;
            Ok((sig, analyzed))
        })();
        results.insert(key, result);
    }
    // A method compiles only if all of its callees compile: iterate to a fixpoint.
    loop {
        let failed: Vec<((String, String), String)> = results
            .iter()
            .filter_map(|(key, r)| {
                let a = &r.as_ref().ok()?.1;
                a.callees
                    .iter()
                    .find(|c| !matches!(results.get(*c), Some(Ok(_))))
                    .map(|c| {
                        (
                            key.clone(),
                            format!("calls {}{}, which is not compiled", c.0, c.1),
                        )
                    })
            })
            .collect();
        if failed.is_empty() {
            break;
        }
        for (key, reason) in failed {
            results.insert(key, Err(reason));
        }
    }
    // Every method runs only after <clinit>, so an uncompilable initializer blocks the class.
    if has_clinit
        && foreign_init.is_none()
        && let Some(Err(reason)) = results.get(&clinit_key)
    {
        let reason = format!("static initializer not compiled: {reason}");
        for (key, result) in results.iter_mut() {
            if *key != clinit_key && result.is_ok() {
                *result = Err(reason.clone());
            }
        }
    }
    let compiled: BTreeSet<(String, String)> = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(k, _)| k.clone())
        .collect();

    // Overridable instance methods get vtable slots in declaration order.
    let virtuals: Vec<&Member> = class
        .methods
        .iter()
        .filter(|m| m.access & (ACC_STATIC | ACC_PRIVATE) == 0 && !m.name.starts_with('<'))
        .collect();
    let layout = ClassLayout {
        vtable: virtuals
            .iter()
            .enumerate()
            .map(|(i, m)| ((m.name.clone(), m.descriptor.clone()), i))
            .collect(),
    };

    let mut source = String::new();
    let _ = writeln!(
        source,
        "/* Generated by JABS from {} (class-file {}.{}). */",
        class.name, class.major, class.minor
    );
    source.push_str(RUNTIME_PRELUDE);
    source.push('\n');
    if !compiled.is_empty() {
        emit_object_layout(&mut source, class);
    }
    for method in &class.methods {
        if let Some(Ok((sig, _))) = results.get(&(method.name.clone(), method.descriptor.clone())) {
            let _ = writeln!(source, "{};", prototype(&class.name, method, sig));
        }
    }
    source.push('\n');
    // The class record is only referenced by `new`, so emit it only when compiled code allocates.
    let allocates = results
        .values()
        .filter_map(|r| r.as_ref().ok())
        .any(|(_, a)| a.insns.iter().any(|i| matches!(i.op, Op::New { .. })));
    if allocates {
        emit_class_record(&mut source, class, &virtuals, &compiled);
    }
    if !compiled.is_empty() {
        emit_statics_and_init(&mut source, class, has_clinit);
    }
    let mut methods = Vec::new();
    for method in &class.methods {
        let status = match &results[&(method.name.clone(), method.descriptor.clone())] {
            Ok((sig, analyzed)) => {
                emit_method(&mut source, class, &layout, method, sig, analyzed);
                MethodStatus::Compiled {
                    symbol: symbol(&class.name, &method.name, &method.descriptor),
                }
            }
            Err(reason) => MethodStatus::Unsupported {
                reason: reason.clone(),
            },
        };
        methods.push(MethodReport {
            name: method.name.clone(),
            descriptor: method.descriptor.clone(),
            status,
        });
    }
    let compiled = methods
        .iter()
        .filter(|m| matches!(m.status, MethodStatus::Compiled { .. }))
        .count();
    CompiledClass {
        schema_version: 1,
        class: class.name.clone(),
        compiled,
        unsupported: methods.len() - compiled,
        methods,
        c_source: source,
    }
}
