//! Bytecode decoding for the compilable subset.
//! Decoding stops at the first opcode outside the subset and reports it, so a
//! method is either fully understood or explicitly unsupported.

use crate::classfile::{ClassFile, Constant, Cursor};
use anyhow::{Result, bail};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    I,
    J,
    F,
    D,
    /// Object reference (or null).
    A,
}

impl Kind {
    /// Operand-stack/local slots the kind occupies: 2 for long and double, else 1.
    pub fn category(self) -> u16 {
        if matches!(self, Kind::J | Kind::D) {
            2
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    I(i32),
    J(i64),
    /// Raw IEEE bits.
    F(u32),
    D(u64),
}

impl Value {
    /// Primitive kind of the constant.
    pub fn kind(self) -> Kind {
        match self {
            Value::I(_) => Kind::I,
            Value::J(_) => Kind::J,
            Value::F(_) => Kind::F,
            Value::D(_) => Kind::D,
        }
    }
}
// Binary ops starts here
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    Ushr,
    And,
    Or,
    Xor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Lcmp,
    Fcmpl,
    Fcmpg,
    Dcmpl,
    Dcmpg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convert {
    I2l,
    I2f,
    I2d,
    L2i,
    L2f,
    L2d,
    F2i,
    F2l,
    F2d,
    D2i,
    D2l,
    D2f,
    I2b,
    I2c,
    I2s,
}

impl Convert {
    /// (source kind, result kind); i2b/i2c/i2s narrow but stay int on the stack.
    pub fn kinds(self) -> (Kind, Kind) {
        use {Convert::*, Kind::*};
        match self {
            I2l => (I, J),
            I2f => (I, F),
            I2d => (I, D),
            L2i => (J, I),
            L2f => (J, F),
            L2d => (J, D),
            F2i => (F, I),
            F2l => (F, J),
            F2d => (F, D),
            D2i => (D, I),
            D2l => (D, J),
            D2f => (D, F),
            I2b | I2c | I2s => (I, I),
        }
    }
}

/// One decoded instruction; branch targets are absolute bytecode offsets.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Nop,
    Const(Value),
    Load(Kind, u16),
    Store(Kind, u16),
    Iinc(u16, i32),
    Pop,
    Pop2,
    Dup,
    DupX1,
    Dup2,
    Swap,
    Binary(Kind, BinOp),
    Neg(Kind),
    Convert(Convert),
    Compare(Compare),
    /// Compare int on stack with zero.
    If(Cond, u32),
    IfICmp(Cond, u32),
    Goto(u32),
    TableSwitch {
        default: u32,
        low: i32,
        targets: Vec<u32>,
    },
    LookupSwitch {
        default: u32,
        pairs: Vec<(i32, u32)>,
    },
    Return(Option<Kind>),
    InvokeStatic {
        class: String,
        name: String,
        descriptor: String,
    },
    GetStatic {
        class: String,
        name: String,
        descriptor: String,
    },
    PutStatic {
        class: String,
        name: String,
        descriptor: String,
    },
    AConstNull,
    /// Branch if the reference on the stack is (or, with `is_null: false`, is not) null.
    IfNull {
        is_null: bool,
        target: u32,
    },
    /// Branch if two references are (or are not) the same object.
    IfACmp {
        equal: bool,
        target: u32,
    },
    New {
        class: String,
    },
    /// Throw ClassCastException unless the reference is null or an instance of `class`.
    CheckCast {
        class: String,
    },
    /// Push 1 if the reference is a non-null instance of `class`, else 0.
    InstanceOf {
        class: String,
    },
    GetField(MemberRef),
    PutField(MemberRef),
    InvokeVirtual(MemberRef),
    /// Constructors, private methods (pre-Java 11 style) and `super.` calls.
    InvokeSpecial(MemberRef),
}

/// A resolved field or method reference: owner class, name and descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberRef {
    pub class: String,
    pub name: String,
    pub descriptor: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insn {
    pub pc: u32,
    pub op: Op,
}

/// An instruction outside the compilable subset, with where it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Unsupported {
    pub pc: u32,
    pub reason: String,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "pc {}: {}", self.pc, self.reason)
    }
}

impl std::error::Error for Unsupported {}

/// Unsupported means valid-but-not-yet-compiled; Malformed means the bytecode itself is broken.
pub enum DecodeError {
    Unsupported(Unsupported),
    Malformed(anyhow::Error),
}

/// Resolve a relative branch offset to an absolute pc, rejecting jumps outside the method.
fn branch(pc: u32, offset: i32, len: usize) -> Result<u32> {
    let target = pc as i64 + offset as i64;
    if target < 0 || target >= len as i64 {
        bail!("branch from {pc} to {target} leaves the method");
    }
    Ok(target as u32)
}

/// Load/store/return opcode families list kinds in this order; arithmetic uses only the first four.
const KINDS: [Kind; 5] = [Kind::I, Kind::J, Kind::F, Kind::D, Kind::A];
const CONDS: [Cond; 6] = [Cond::Eq, Cond::Ne, Cond::Lt, Cond::Ge, Cond::Gt, Cond::Le];

/// Decode an entire method body. Branch targets are checked against the code
/// length here and against instruction boundaries by the compiler.
pub fn decode(class: &ClassFile, code: &[u8]) -> std::result::Result<Vec<Insn>, DecodeError> {
    let mut c = Cursor::new(code);
    let mut out = Vec::new();
    while c.remaining() > 0 {
        let pc = c.pos() as u32;
        match decode_one(class, &mut c, pc, code.len()) {
            Ok(op) => out.push(Insn { pc, op }),
            Err(e) => {
                return Err(match e.downcast::<Unsupported>() {
                    Ok(unsupported) => DecodeError::Unsupported(unsupported),
                    Err(e) => DecodeError::Malformed(e),
                });
            }
        }
    }
    Ok(out)
}

/// Read one opcode and its operands at `pc`; errors with `Unsupported` for opcodes outside the subset.
fn decode_one(class: &ClassFile, c: &mut Cursor, pc: u32, len: usize) -> Result<Op> {
    let opcode = c.u8()?;
    let unsupported = || Unsupported {
        pc,
        reason: format!(
            "opcode {} (0x{opcode:02x}) not supported yet",
            unsupported_opcode_name(opcode)
        ),
    };
    use Op::*;
    let op = match opcode {
        0x00 => Nop,
        0x02..=0x08 => Const(Value::I(opcode as i32 - 0x03)),
        0x09 | 0x0a => Const(Value::J(opcode as i64 - 0x09)),
        0x0b..=0x0d => Const(Value::F((opcode as f32 - 11.0).to_bits())),
        0x0e | 0x0f => Const(Value::D((opcode as f64 - 14.0).to_bits())),
        0x10 => Const(Value::I(c.u8()? as i8 as i32)),
        0x11 => Const(Value::I(c.u16()? as i16 as i32)),
        0x12..=0x14 => {
            let index = if opcode == 0x12 {
                c.u8()? as u16
            } else {
                c.u16()?
            };
            let value = match class.pool.get(index as usize) {
                Some(Constant::Integer(v)) if opcode != 0x14 => Value::I(*v),
                Some(Constant::Float(v)) if opcode != 0x14 => Value::F(*v),
                Some(Constant::Long(v)) if opcode == 0x14 => Value::J(*v),
                Some(Constant::Double(v)) if opcode == 0x14 => Value::D(*v),
                Some(
                    Constant::String(_)
                    | Constant::Class(_)
                    | Constant::MethodType(_)
                    | Constant::MethodHandle { .. },
                )
                | Some(Constant::Dynamic { .. }) => return Err(unsupported().into()),
                _ => bail!("ldc of invalid constant {index}"),
            };
            Const(value)
        }
        0x15..=0x19 => Load(KINDS[(opcode - 0x15) as usize], c.u8()? as u16),
        0x1a..=0x2d => Load(
            KINDS[((opcode - 0x1a) / 4) as usize],
            ((opcode - 0x1a) % 4) as u16,
        ),
        0x36..=0x3a => Store(KINDS[(opcode - 0x36) as usize], c.u8()? as u16),
        0x3b..=0x4e => Store(
            KINDS[((opcode - 0x3b) / 4) as usize],
            ((opcode - 0x3b) % 4) as u16,
        ),
        0x57 => Pop,
        0x58 => Pop2,
        0x59 => Dup,
        0x5a => DupX1,
        0x5c => Dup2,
        0x5f => Swap,
        0x60..=0x73 => {
            let ops = [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Rem];
            let index = (opcode - 0x60) as usize;
            Binary(KINDS[index % 4], ops[index / 4])
        }
        0x74..=0x77 => Neg(KINDS[(opcode - 0x74) as usize]),
        0x78..=0x83 => {
            let ops = [
                BinOp::Shl,
                BinOp::Shr,
                BinOp::Ushr,
                BinOp::And,
                BinOp::Or,
                BinOp::Xor,
            ];
            let index = (opcode - 0x78) as usize;
            Binary(
                if index.is_multiple_of(2) {
                    Kind::I
                } else {
                    Kind::J
                },
                ops[index / 2],
            )
        }
        0x84 => Iinc(c.u8()? as u16, c.u8()? as i8 as i32),
        0x85..=0x93 => {
            use self::Convert::*;
            let all = [
                I2l, I2f, I2d, L2i, L2f, L2d, F2i, F2l, F2d, D2i, D2l, D2f, I2b, I2c, I2s,
            ];
            Convert(all[(opcode - 0x85) as usize])
        }
        0x94 => Compare(self::Compare::Lcmp),
        0x95 => Compare(self::Compare::Fcmpl),
        0x96 => Compare(self::Compare::Fcmpg),
        0x97 => Compare(self::Compare::Dcmpl),
        0x98 => Compare(self::Compare::Dcmpg),
        0x99..=0x9e => If(
            CONDS[(opcode - 0x99) as usize],
            branch(pc, c.u16()? as i16 as i32, len)?,
        ),
        0x9f..=0xa4 => IfICmp(
            CONDS[(opcode - 0x9f) as usize],
            branch(pc, c.u16()? as i16 as i32, len)?,
        ),
        0xa7 => Goto(branch(pc, c.u16()? as i16 as i32, len)?),
        0xc8 => Goto(branch(pc, c.u32()? as i32, len)?),
        0xaa | 0xab => {
            // Operands start at the next 4-byte boundary, counted from the method's first byte.
            c.take(c.pos().next_multiple_of(4) - c.pos())?;
            let default = branch(pc, c.u32()? as i32, len)?;
            if opcode == 0xaa {
                let low = c.u32()? as i32;
                let high = c.u32()? as i32;
                if high < low || (high as i64 - low as i64) >= 65536 {
                    bail!("invalid tableswitch range {low}..{high}");
                }
                let mut targets = Vec::new();
                for _ in low..=high {
                    targets.push(branch(pc, c.u32()? as i32, len)?);
                }
                TableSwitch {
                    default,
                    low,
                    targets,
                }
            } else {
                let count = c.u32()? as i32;
                if !(0..65536).contains(&count) {
                    bail!("invalid lookupswitch count {count}");
                }
                let mut pairs: Vec<(i32, u32)> = Vec::new();
                for _ in 0..count {
                    let key = c.u32()? as i32;
                    if pairs.last().is_some_and(|(last, _)| *last >= key) {
                        bail!("lookupswitch keys not sorted");
                    }
                    pairs.push((key, branch(pc, c.u32()? as i32, len)?));
                }
                LookupSwitch { default, pairs }
            }
        }
        0xac..=0xb0 => Return(Some(KINDS[(opcode - 0xac) as usize])),
        0x01 => AConstNull,
        0xa5 | 0xa6 => IfACmp {
            equal: opcode == 0xa5,
            target: branch(pc, c.u16()? as i16 as i32, len)?,
        },
        0xc6 | 0xc7 => IfNull {
            is_null: opcode == 0xc6,
            target: branch(pc, c.u16()? as i16 as i32, len)?,
        },
        0xbb => New {
            class: class.class_name(c.u16()?)?.into(),
        },
        0xc0 => CheckCast {
            class: class.class_name(c.u16()?)?.into(),
        },
        0xc1 => InstanceOf {
            class: class.class_name(c.u16()?)?.into(),
        },
        0xb4 | 0xb5 => {
            let (owner, name, descriptor) = class.field_ref(c.u16()?)?;
            let member = MemberRef {
                class: owner.into(),
                name: name.into(),
                descriptor: descriptor.into(),
            };
            if opcode == 0xb4 {
                GetField(member)
            } else {
                PutField(member)
            }
        }
        0xb6 | 0xb7 => {
            let (owner, name, descriptor) = class.method_ref(c.u16()?)?;
            let member = MemberRef {
                class: owner.into(),
                name: name.into(),
                descriptor: descriptor.into(),
            };
            if opcode == 0xb6 {
                InvokeVirtual(member)
            } else {
                InvokeSpecial(member)
            }
        }
        0xb1 => Return(None),
        0xb8 => {
            let (class_name, name, descriptor) = class.method_ref(c.u16()?)?;
            InvokeStatic {
                class: class_name.into(),
                name: name.into(),
                descriptor: descriptor.into(),
            }
        }
        0xb2 | 0xb3 => {
            let (class_name, name, descriptor) = class.field_ref(c.u16()?)?;
            let (class, name, descriptor) = (class_name.into(), name.into(), descriptor.into());
            if opcode == 0xb2 {
                GetStatic {
                    class,
                    name,
                    descriptor,
                }
            } else {
                PutStatic {
                    class,
                    name,
                    descriptor,
                }
            }
        }
        0xc4 => {
            let inner = c.u8()?;
            let index = c.u16()?;
            match inner {
                0x15..=0x19 => Load(KINDS[(inner - 0x15) as usize], index),
                0x36..=0x3a => Store(KINDS[(inner - 0x36) as usize], index),
                0x84 => Iinc(index, c.u16()? as i16 as i32),
                _ => return Err(unsupported().into()),
            }
        }
        _ => return Err(unsupported().into()),
    };
    Ok(op)
}
/// Human-readable name for opcodes the decoder rejects, used in unsupported diagnostics.
pub fn unsupported_opcode_name(opcode: u8) -> &'static str {
    match opcode {
        0x01 => "aconst_null",
        0x19 | 0x2a..=0x2d => "aload",
        0x2e..=0x35 => "array load",
        0x3a | 0x4b..=0x4e => "astore",
        0x4f..=0x56 => "array store",
        0x5b => "dup_x2",
        0x5d => "dup2_x1",
        0x5e => "dup2_x2",
        0xa5 | 0xa6 => "if_acmp",
        0xa8 | 0xa9 | 0xc9 => "jsr/ret",
        0xb0 => "areturn",
        0xb2 => "getstatic",
        0xb3 => "putstatic",
        0xb4 => "getfield",
        0xb5 => "putfield",
        0xb6 => "invokevirtual",
        0xb7 => "invokespecial",
        0xb9 => "invokeinterface",
        0xba => "invokedynamic",
        0xbb => "new",
        0xbc..=0xbe => "array allocation/length",
        0xbf => "athrow",
        0xc0 => "checkcast",
        0xc1 => "instanceof",
        0xc2 | 0xc3 => "monitor",
        0xc5 => "multianewarray",
        0xc6 | 0xc7 => "ifnull/ifnonnull",
        0x12..=0x14 => "ldc (reference constant)",
        _ => "unknown",
    }
}
