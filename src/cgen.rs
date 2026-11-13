//! Per-class C emission for the compilable subset.
//!
//! A method compiles only when every instruction is understood, the operand
//! stack is consistent on every path, and every callee also compiles. All
//! other methods are reported with a specific reason; nothing is guessed.

use crate::bytecode::{self, BinOp, Compare, Cond, Convert, DecodeError, Insn, Kind, Op, Value};
use crate::classfile::{ACC_STATIC, ACC_SYNCHRONIZED, ClassFile, Constant, Member};
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

/// Parameter and return types of a primitive-only method descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    /// Raw descriptor characters: I J F D Z B C S.
    pub params: Vec<char>,
    /// Raw return character, `V` for void.
    pub ret: char,
}

/// Stack kind for a descriptor char; boolean/byte/char/short all live as int.
pub fn kind_of(c: char) -> Option<Kind> {
    match c {
        'I' | 'Z' | 'B' | 'C' | 'S' => Some(Kind::I),
        'J' => Some(Kind::J),
        'F' => Some(Kind::F),
        'D' => Some(Kind::D),
        _ => None,
    }
}

/// Parse a primitive-only method descriptor; reference types are rejected until the object model exists.
pub fn parse_signature(descriptor: &str) -> Result<Signature> {
    let Some(rest) = descriptor.strip_prefix('(') else {
        bail!("invalid method descriptor {descriptor}")
    };
    let Some((params, ret)) = rest.split_once(')') else {
        bail!("invalid method descriptor {descriptor}")
    };
    if params.contains(['L', '['])
        || ret.len() != 1
        || !(ret == "V" || kind_of(ret.chars().next().unwrap()).is_some())
    {
        bail!("reference types in {descriptor} need the object model");
    }
    let params: Vec<char> = params.chars().collect();
    if params.iter().any(|&c| kind_of(c).is_none()) {
        bail!("invalid method descriptor {descriptor}");
    }
    Ok(Signature {
        params,
        ret: ret.chars().next().unwrap(),
    })
}

/// C type used for a descriptor char in generated signatures (`void` for V).
pub fn c_type(c: char) -> &'static str {
    match kind_of(c) {
        Some(Kind::I) => "int32_t",
        Some(Kind::J) => "int64_t",
        Some(Kind::F) => "float",
        Some(Kind::D) => "double",
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

/// Stable C symbol for a Java method identity (class, name, descriptor).
pub fn symbol(class: &str, name: &str, descriptor: &str) -> String {
    let mut out = String::from("jm_");
    mangle(class, &mut out);
    out.push_str("__");
    mangle(name, &mut out);
    out.push_str("__");
    mangle(descriptor, &mut out);
    out
}

/// C symbol for a static field's storage.
pub fn field_symbol(class: &str, name: &str) -> String {
    let mut out = String::from("jf_");
    mangle(class, &mut out);
    out.push_str("__");
    mangle(name, &mut out);
    out
}

/// C symbol of a class's initialization entry point; returns 0 when the class is ready, -1 with `jabs_pending` set.
pub fn init_symbol(class: &str) -> String {
    let mut out = String::from("jc_");
    mangle(class, &mut out);
    out.push_str("__init");
    out
}

/// C storage type for a primitive static field, sized like the JVM's (so byte fields really hold a byte).
fn storage_type(descriptor: &str) -> Option<&'static str> {
    Some(match descriptor {
        "Z" => "uint8_t",
        "B" => "int8_t",
        "C" => "uint16_t",
        "S" => "int16_t",
        "I" => "int32_t",
        "J" => "int64_t",
        "F" => "float",
        "D" => "double",
        _ => return None,
    })
}

/// Expression converting a stack value into a field's storage type (putstatic narrowing; boolean keeps bit 0).
fn narrow_for_store(descriptor: &str, value: &str) -> String {
    match descriptor {
        "Z" => format!("(uint8_t)({value} & 1)"),
        "B" => format!("(int8_t)jabs_i2b({value})"),
        "C" => format!("(uint16_t)jabs_i2c({value})"),
        "S" => format!("(int16_t)jabs_i2s({value})"),
        _ => value.to_string(),
    }
}

/// Find this class's own primitive static field, or explain why the access cannot be compiled.
fn own_static_field<'a>(
    class: &'a ClassFile,
    owner: &str,
    name: &str,
    descriptor: &str,
    pc: u32,
) -> Result<&'a Member> {
    if owner != class.name {
        bail!("pc {pc}: static field {owner}.{name} in another class is not compiled yet");
    }
    let Some(field) = class
        .fields
        .iter()
        .find(|f| f.name == name && f.descriptor == descriptor)
    else {
        bail!(
            "pc {pc}: static field {name} is inherited; cross-class field resolution is not compiled yet"
        );
    };
    if field.access & ACC_STATIC == 0 {
        bail!("pc {pc}: {name} is not a static field");
    }
    if storage_type(descriptor).is_none() {
        bail!("pc {pc}: static field {name} of reference type {descriptor} needs the object model");
    }
    Ok(field)
}

struct Analyzed {
    insns: Vec<Insn>,
    /// Operand stack (one entry per value) before each instruction.
    stacks: Vec<Vec<Kind>>,
    targets: BTreeSet<u32>,
    callees: BTreeSet<(String, String)>,
}

/// Pop one value and require it to be `want`.
fn pop(stack: &mut Vec<Kind>, want: Kind, pc: u32) -> Result<()> {
    match stack.pop() {
        Some(k) if k == want => Ok(()),
        other => bail!("pc {pc}: expected {want:?} on stack, found {other:?}"),
    }
}

/// Pop one int/float value (not long/double), as pop/dup/swap require.
fn pop_cat1(stack: &mut Vec<Kind>, pc: u32) -> Result<Kind> {
    match stack.pop() {
        Some(k) if k.category() == 1 => Ok(k),
        other => bail!("pc {pc}: expected category-1 value, found {other:?}"),
    }
}

/// Simulate the operand stack; return successor pcs (excluding fallthrough when terminal).
fn step(
    insn: &Insn,
    stack: &mut Vec<Kind>,
    class: &ClassFile,
    sig: &Signature,
    max_locals: u16,
) -> Result<(bool, Vec<u32>)> {
    let pc = insn.pc;
    let local = |n: u16, k: Kind| -> Result<()> {
        if n as u32 + k.category() as u32 > max_locals as u32 {
            bail!("pc {pc}: local {n} out of range");
        }
        Ok(())
    };
    let mut falls = true;
    let mut targets = Vec::new();
    match &insn.op {
        Op::Nop | Op::Goto(_) => {}
        Op::Const(v) => stack.push(v.kind()),
        Op::Load(k, n) => {
            local(*n, *k)?;
            stack.push(*k);
        }
        Op::Store(k, n) => {
            local(*n, *k)?;
            pop(stack, *k, pc)?;
        }
        Op::Iinc(n, _) => local(*n, Kind::I)?,
        Op::Pop => {
            pop_cat1(stack, pc)?;
        }
        Op::Pop2 => {
            if stack.last().is_some_and(|k| k.category() == 2) {
                stack.pop();
            } else {
                pop_cat1(stack, pc)?;
                pop_cat1(stack, pc)?;
            }
        }
        Op::Dup => {
            let k = pop_cat1(stack, pc)?;
            stack.extend([k, k]);
        }
        Op::DupX1 => {
            let a = pop_cat1(stack, pc)?;
            let b = pop_cat1(stack, pc)?;
            stack.extend([a, b, a]);
        }
        Op::Dup2 => {
            if let Some(&k) = stack.last().filter(|k| k.category() == 2) {
                stack.push(k);
            } else {
                let a = pop_cat1(stack, pc)?;
                let b = pop_cat1(stack, pc)?;
                stack.extend([b, a, b, a]);
            }
        }
        Op::Swap => {
            let a = pop_cat1(stack, pc)?;
            let b = pop_cat1(stack, pc)?;
            stack.extend([a, b]);
        }
        Op::Binary(k, op) => {
            let shift = matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr);
            pop(stack, if shift { Kind::I } else { *k }, pc)?;
            pop(stack, *k, pc)?;
            stack.push(*k);
        }
        Op::Neg(k) => {
            pop(stack, *k, pc)?;
            stack.push(*k);
        }
        Op::Convert(c) => {
            let (from, to) = c.kinds();
            pop(stack, from, pc)?;
            stack.push(to);
        }
        Op::Compare(c) => {
            let k = match c {
                Compare::Lcmp => Kind::J,
                Compare::Fcmpl | Compare::Fcmpg => Kind::F,
                Compare::Dcmpl | Compare::Dcmpg => Kind::D,
            };
            pop(stack, k, pc)?;
            pop(stack, k, pc)?;
            stack.push(Kind::I);
        }
        Op::If(_, t) => {
            pop(stack, Kind::I, pc)?;
            targets.push(*t);
        }
        Op::IfICmp(_, t) => {
            pop(stack, Kind::I, pc)?;
            pop(stack, Kind::I, pc)?;
            targets.push(*t);
        }
        Op::TableSwitch {
            default,
            targets: ts,
            ..
        } => {
            pop(stack, Kind::I, pc)?;
            falls = false;
            targets.push(*default);
            targets.extend(ts);
        }
        Op::LookupSwitch { default, pairs } => {
            pop(stack, Kind::I, pc)?;
            falls = false;
            targets.push(*default);
            targets.extend(pairs.iter().map(|(_, t)| *t));
        }
        Op::Return(k) => {
            if *k != kind_of(sig.ret) {
                bail!(
                    "pc {pc}: return kind {k:?} does not match descriptor return {}",
                    sig.ret
                );
            }
            if let Some(k) = k {
                pop(stack, *k, pc)?;
            }
            falls = false;
        }
        Op::InvokeStatic {
            class: owner,
            name,
            descriptor,
        } => {
            if owner != &class.name {
                bail!(
                    "pc {pc}: call to {owner}.{name}{descriptor} outside this class is not compiled yet"
                );
            }
            let callee = parse_signature(descriptor)
                .map_err(|e| anyhow::anyhow!("pc {pc}: call to {name}: {e}"))?;
            for &p in callee.params.iter().rev() {
                pop(stack, kind_of(p).unwrap(), pc)?;
            }
            if let Some(k) = kind_of(callee.ret) {
                stack.push(k);
            }
        }
        Op::GetStatic {
            class: owner,
            name,
            descriptor,
        } => {
            own_static_field(class, owner, name, descriptor, pc)?;
            stack.push(kind_of(descriptor.chars().next().unwrap()).unwrap());
        }
        Op::PutStatic {
            class: owner,
            name,
            descriptor,
        } => {
            own_static_field(class, owner, name, descriptor, pc)?;
            pop(
                stack,
                kind_of(descriptor.chars().next().unwrap()).unwrap(),
                pc,
            )?;
        }
    }
    if let Op::Goto(t) = insn.op {
        falls = false;
        targets.push(t);
    }
    Ok((falls, targets))
}

/// Decode a method and type-check every reachable path, or explain why it cannot be compiled.
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
    let index: HashMap<u32, usize> = insns
        .iter()
        .enumerate()
        .map(|(i, insn)| (insn.pc, i))
        .collect();
    let mut stacks: Vec<Option<Vec<Kind>>> = vec![None; insns.len()];
    let mut targets = BTreeSet::new();
    let mut callees = BTreeSet::new();
    let mut work = vec![(0usize, Vec::new())];
    while let Some((i, incoming)) = work.pop() {
        match &stacks[i] {
            Some(existing) if *existing == incoming => continue,
            Some(existing) => {
                return Err(format!(
                    "pc {}: inconsistent stack {existing:?} vs {incoming:?}",
                    insns[i].pc
                ));
            }
            None => {}
        }
        let depth: u32 = incoming.iter().map(|k| k.category() as u32).sum();
        if depth > code.max_stack as u32 {
            return Err(format!("pc {}: stack exceeds max_stack", insns[i].pc));
        }
        stacks[i] = Some(incoming.clone());
        let mut stack = incoming;
        let (falls, succ) = step(&insns[i], &mut stack, class, sig, code.max_locals)
            .map_err(|e| format!("{e:#}"))?;
        if let Op::InvokeStatic {
            name, descriptor, ..
        } = &insns[i].op
        {
            callees.insert((name.clone(), descriptor.clone()));
        }
        for t in succ {
            let Some(&j) = index.get(&t) else {
                return Err(format!(
                    "pc {}: branch target {t} is not an instruction",
                    insns[i].pc
                ));
            };
            targets.insert(t);
            work.push((j, stack.clone()));
        }
        if falls {
            if i + 1 >= insns.len() {
                return Err(format!(
                    "pc {}: execution falls off the end of the method",
                    insns[i].pc
                ));
            }
            work.push((i + 1, stack));
        }
    }
    // Unreachable instructions are never emitted.
    let (insns, stacks): (Vec<_>, Vec<_>) = insns
        .into_iter()
        .zip(stacks)
        .filter_map(|(insn, s)| s.map(|s| (insn, s)))
        .unzip();
    Ok(Analyzed {
        insns,
        stacks,
        targets,
        callees,
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

/// C function prototype for a compiled method.
fn prototype(class: &str, method: &Member, sig: &Signature) -> String {
    let params: Vec<String> = sig
        .params
        .iter()
        .enumerate()
        .map(|(i, &p)| format!("{} a{i}", c_type(p)))
        .collect();
    format!(
        "{} {}({})",
        c_type(sig.ret),
        symbol(class, &method.name, &method.descriptor),
        if params.is_empty() {
            "void".into()
        } else {
            params.join(", ")
        }
    )
}

/// Emit one analyzed method as C: locals and stack become `jabs_value` arrays, pcs become labels.
fn emit_method(
    out: &mut String,
    class: &ClassFile,
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
    for (i, &p) in sig.params.iter().enumerate() {
        let k = kind_of(p).unwrap();
        let _ = writeln!(out, "    l[{slot}].{} = a{i};", field(k));
        slot += k.category() as usize;
    }
    let throw = |exc: &str| format!("{{ jabs_pending = \"{exc}\"; {ret_default} }}");
    for (insn, stack) in a.insns.iter().zip(&a.stacks) {
        if a.targets.contains(&insn.pc) {
            let _ = writeln!(out, "L{}:;", insn.pc);
        }
        let d = stack.len();
        let line = match &insn.op {
            Op::Nop | Op::Pop | Op::Pop2 => continue,
            Op::Const(v) => format!("s[{d}].{} = {};", field(v.kind()), value_literal(*v)),
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
            } => {
                let callee = parse_signature(descriptor).unwrap();
                let base = d - callee.params.len();
                let args: Vec<String> = callee
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| format!("s[{}].{}", base + i, field(kind_of(p).unwrap())))
                    .collect();
                let call = format!(
                    "{}({})",
                    symbol(&class.name, name, descriptor),
                    args.join(", ")
                );
                let assign = match kind_of(callee.ret) {
                    Some(k) => format!("s[{base}].{} = {call};", field(k)),
                    None => format!("{call};"),
                };
                format!("{assign} if (jabs_pending) {ret_default}")
            }
            Op::GetStatic {
                name, descriptor, ..
            } => {
                let k = kind_of(descriptor.chars().next().unwrap()).unwrap();
                format!("s[{d}].{} = {};", field(k), field_symbol(&class.name, name))
            }
            Op::PutStatic {
                name, descriptor, ..
            } => {
                let k = kind_of(descriptor.chars().next().unwrap()).unwrap();
                let value = narrow_for_store(descriptor, &format!("s[{}].{}", d - 1, field(k)));
                format!("{} = {value};", field_symbol(&class.name, name))
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

/// Emit static field storage and the class initialization function (JVMS 5.5, single-threaded).
fn emit_statics_and_init(out: &mut String, class: &ClassFile, has_clinit: bool) {
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

/// Compile every eligible static method of one class to a self-contained C unit.
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
            if method.access & ACC_STATIC == 0 {
                return Err("instance methods need the object model".into());
            }
            if method.access & ACC_SYNCHRONIZED != 0 {
                return Err("synchronized methods need monitors".into());
            }
            let sig = parse_signature(&method.descriptor).map_err(|e| e.to_string())?;
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
    // Every static method runs only after <clinit>, so an uncompilable initializer blocks the class.
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
    let any_compiled = results.values().any(|r| r.is_ok());

    let mut source = String::new();
    let _ = writeln!(
        source,
        "/* Generated by JABS from {} (class-file {}.{}). */",
        class.name, class.major, class.minor
    );
    source.push_str(RUNTIME_PRELUDE);
    source.push('\n');
    let mut methods = Vec::new();
    for method in &class.methods {
        if let Some(Ok((sig, _))) = results.get(&(method.name.clone(), method.descriptor.clone())) {
            let _ = writeln!(source, "{};", prototype(&class.name, method, sig));
        }
    }
    source.push('\n');
    if any_compiled {
        emit_statics_and_init(&mut source, class, has_clinit);
    }
    for method in &class.methods {
        let status = match &results[&(method.name.clone(), method.descriptor.clone())] {
            Ok((sig, analyzed)) => {
                emit_method(&mut source, class, method, sig, analyzed);
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
