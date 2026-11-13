//! Bounds-checked class-file model (JVMS chapter 4) shared by inventory and compilation.

use anyhow::{Result, bail};

pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;
pub const ACC_SYNCHRONIZED: u16 = 0x0020;
pub const ACC_NATIVE: u16 = 0x0100;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    /// Index 0 and the second slot of long/double entries.
    Unusable,
    Utf8(String),
    Integer(i32),
    /// Raw IEEE bits so NaN payloads and -0.0 survive exactly.
    Float(u32),
    Long(i64),
    Double(u64),
    Class(u16),
    String(u16),
    FieldRef {
        class: u16,
        name_and_type: u16,
    },
    MethodRef {
        class: u16,
        name_and_type: u16,
    },
    InterfaceMethodRef {
        class: u16,
        name_and_type: u16,
    },
    NameAndType {
        name: u16,
        descriptor: u16,
    },
    MethodHandle {
        kind: u8,
        reference: u16,
    },
    MethodType(u16),
    Dynamic {
        bootstrap: u16,
        name_and_type: u16,
    },
    InvokeDynamic {
        bootstrap: u16,
        name_and_type: u16,
    },
    Module(u16),
    Package(u16),
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    pub catch_type: u16,
}

#[derive(Debug, Clone)]
pub struct Code {
    pub max_stack: u16,
    pub max_locals: u16,
    pub bytecode: Vec<u8>,
    pub handlers: Vec<Handler>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub access: u16,
    pub name: String,
    pub descriptor: String,
    pub code: Option<Code>,
    /// Pool index of a field's ConstantValue attribute (`static final` constants).
    pub constant_value: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub minor: u16,
    pub major: u16,
    pub pool: Vec<Constant>,
    pub access: u16,
    pub name: String,
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
}

pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }
    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    /// Borrow the next `n` bytes, failing instead of reading past the end.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(end) = self.pos.checked_add(n).filter(|&e| e <= self.data.len()) else {
            bail!("truncated data at offset {}", self.pos)
        };
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// Read a big-endian u64 (high word first, as long/double constants are stored).
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(((self.u32()? as u64) << 32) | self.u32()? as u64)
    }
}

/// Decode JVM modified UTF-8 (encoded NUL, surrogate pairs as two 3-byte units).
pub fn decode_modified_utf8(bytes: &[u8]) -> Result<String> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (unit, len) = match b {
            0x01..=0x7F => (b as u16, 1),
            0xC0..=0xDF if i + 1 < bytes.len() && bytes[i + 1] & 0xC0 == 0x80 => {
                ((((b & 0x1F) as u16) << 6) | (bytes[i + 1] & 0x3F) as u16, 2)
            }
            0xE0..=0xEF
                if i + 2 < bytes.len()
                    && bytes[i + 1] & 0xC0 == 0x80
                    && bytes[i + 2] & 0xC0 == 0x80 =>
            {
                (
                    (((b & 0x0F) as u16) << 12)
                        | (((bytes[i + 1] & 0x3F) as u16) << 6)
                        | (bytes[i + 2] & 0x3F) as u16,
                    3,
                )
            }
            _ => bail!("invalid modified UTF-8 byte 0x{b:02x} at {i}"),
        };
        units.push(unit);
        i += len;
    }
    Ok(String::from_utf16_lossy(&units))
}

/// Walk an attribute table, handing each (name, payload) to `on`.
fn parse_attributes(
    c: &mut Cursor,
    pool: &[Constant],
    mut on: impl FnMut(&str, &[u8]) -> Result<()>,
) -> Result<()> {
    for _ in 0..c.u16()? {
        let name_index = c.u16()?;
        let len = c.u32()? as usize;
        let data = c.take(len)?;
        let Some(Constant::Utf8(name)) = pool.get(name_index as usize) else {
            bail!("attribute name {name_index} is not Utf8")
        };
        on(name, data)?;
    }
    Ok(())
}

/// Parse a Code attribute: limits, bytecode and exception handlers.
fn parse_code(data: &[u8], pool: &[Constant]) -> Result<Code> {
    let mut c = Cursor::new(data);
    let max_stack = c.u16()?;
    let max_locals = c.u16()?;
    let len = c.u32()? as usize;
    if len == 0 || len >= 65536 {
        bail!("invalid code length {len}");
    }
    let bytecode = c.take(len)?.to_vec();
    let mut handlers = Vec::new();
    for _ in 0..c.u16()? {
        let handler = Handler {
            start_pc: c.u16()?,
            end_pc: c.u16()?,
            handler_pc: c.u16()?,
            catch_type: c.u16()?,
        };
        if handler.start_pc >= handler.end_pc
            || handler.end_pc as usize > len
            || handler.handler_pc as usize >= len
        {
            bail!("invalid exception handler range");
        }
        handlers.push(handler);
    }
    parse_attributes(&mut c, pool, |_, _| Ok(()))?;
    if c.remaining() != 0 {
        bail!("Code attribute has trailing bytes");
    }
    Ok(Code {
        max_stack,
        max_locals,
        bytecode,
        handlers,
    })
}

impl ClassFile {
    /// Parse and bounds-check a whole class file.
    pub fn parse(data: &[u8]) -> Result<ClassFile> {
        let mut c = Cursor::new(data);
        if c.u32()? != 0xCAFEBABE {
            bail!("bad class-file magic");
        }
        let minor = c.u16()?;
        let major = c.u16()?;
        if !(45..=255).contains(&major) {
            bail!("unsupported class-file major version {major}");
        }
        let count = c.u16()? as usize;
        if count == 0 {
            bail!("empty constant pool");
        }
        let mut pool = Vec::with_capacity(count);
        pool.push(Constant::Unusable);
        while pool.len() < count {
            let tag = c.u8()?;
            let entry = match tag {
                1 => {
                    let len = c.u16()? as usize;
                    Constant::Utf8(decode_modified_utf8(c.take(len)?)?)
                }
                3 => Constant::Integer(c.u32()? as i32),
                4 => Constant::Float(c.u32()?),
                5 | 6 => {
                    let bits = c.u64()?;
                    if pool.len() + 1 >= count {
                        bail!("long/double constant overruns pool");
                    }
                    pool.push(if tag == 5 {
                        Constant::Long(bits as i64)
                    } else {
                        Constant::Double(bits)
                    });
                    Constant::Unusable
                }
                7 => Constant::Class(c.u16()?),
                8 => Constant::String(c.u16()?),
                9 => Constant::FieldRef {
                    class: c.u16()?,
                    name_and_type: c.u16()?,
                },
                10 => Constant::MethodRef {
                    class: c.u16()?,
                    name_and_type: c.u16()?,
                },
                11 => Constant::InterfaceMethodRef {
                    class: c.u16()?,
                    name_and_type: c.u16()?,
                },
                12 => Constant::NameAndType {
                    name: c.u16()?,
                    descriptor: c.u16()?,
                },
                15 => Constant::MethodHandle {
                    kind: c.u8()?,
                    reference: c.u16()?,
                },
                16 => Constant::MethodType(c.u16()?),
                17 => Constant::Dynamic {
                    bootstrap: c.u16()?,
                    name_and_type: c.u16()?,
                },
                18 => Constant::InvokeDynamic {
                    bootstrap: c.u16()?,
                    name_and_type: c.u16()?,
                },
                19 => Constant::Module(c.u16()?),
                20 => Constant::Package(c.u16()?),
                _ => bail!("unknown constant-pool tag {tag} at index {}", pool.len()),
            };
            pool.push(entry);
        }
        let access = c.u16()?;
        let this_class = c.u16()?;
        let super_class = c.u16()?;
        let mut interfaces = Vec::new();
        for _ in 0..c.u16()? {
            interfaces.push(class_name(&pool, c.u16()?)?.to_owned());
        }
        let members = |c: &mut Cursor| -> Result<Vec<Member>> {
            let mut out = Vec::new();
            for _ in 0..c.u16()? {
                let access = c.u16()?;
                let name = utf8(&pool, c.u16()?)?.to_owned();
                let descriptor = utf8(&pool, c.u16()?)?.to_owned();
                let mut code = None;
                let mut constant_value = None;
                parse_attributes(c, &pool, |attr, data| {
                    match attr {
                        "Code" if code.is_some() => bail!("duplicate Code attribute"),
                        "Code" => code = Some(parse_code(data, &pool)?),
                        "ConstantValue" => {
                            if data.len() != 2 {
                                bail!("ConstantValue attribute must be 2 bytes");
                            }
                            constant_value = Some(u16::from_be_bytes([data[0], data[1]]));
                        }
                        _ => {}
                    }
                    Ok(())
                })?;
                out.push(Member {
                    access,
                    name,
                    descriptor,
                    code,
                    constant_value,
                });
            }
            Ok(out)
        };
        let fields = members(&mut c)?;
        let methods = members(&mut c)?;
        parse_attributes(&mut c, &pool, |_, _| Ok(()))?;
        if c.remaining() != 0 {
            bail!("{} trailing bytes after class file", c.remaining());
        }
        Ok(ClassFile {
            minor,
            major,
            name: class_name(&pool, this_class)?.to_owned(),
            super_name: if super_class == 0 {
                None
            } else {
                Some(class_name(&pool, super_class)?.to_owned())
            },
            pool,
            access,
            interfaces,
            fields,
            methods,
        })
    }

    /// Utf8 constant at `index`.
    pub fn utf8(&self, index: u16) -> Result<&str> {
        utf8(&self.pool, index)
    }

    /// Internal name of the Class constant at `index` (e.g. `java/lang/Object`).
    pub fn class_name(&self, index: u16) -> Result<&str> {
        class_name(&self.pool, index)
    }

    /// Resolve a Methodref/InterfaceMethodref to (class, name, descriptor).
    pub fn method_ref(&self, index: u16) -> Result<(&str, &str, &str)> {
        let (Some(Constant::MethodRef {
            class,
            name_and_type,
        })
        | Some(Constant::InterfaceMethodRef {
            class,
            name_and_type,
        })) = self.pool.get(index as usize)
        else {
            bail!("constant {index} is not a method reference")
        };
        self.member_ref(*class, *name_and_type)
    }

    /// Resolve a Fieldref to (class, name, descriptor).
    pub fn field_ref(&self, index: u16) -> Result<(&str, &str, &str)> {
        let Some(Constant::FieldRef {
            class,
            name_and_type,
        }) = self.pool.get(index as usize)
        else {
            bail!("constant {index} is not a field reference")
        };
        self.member_ref(*class, *name_and_type)
    }

    /// Resolve a (Class, NameAndType) pair to (class, name, descriptor).
    fn member_ref(&self, class: u16, name_and_type: u16) -> Result<(&str, &str, &str)> {
        let Some(Constant::NameAndType { name, descriptor }) =
            self.pool.get(name_and_type as usize)
        else {
            bail!("constant {name_and_type} is not NameAndType")
        };
        Ok((
            self.class_name(class)?,
            self.utf8(*name)?,
            self.utf8(*descriptor)?,
        ))
    }
}

/// Utf8 constant at `index` in a raw pool.
fn utf8(pool: &[Constant], index: u16) -> Result<&str> {
    match pool.get(index as usize) {
        Some(Constant::Utf8(s)) => Ok(s),
        _ => bail!("constant {index} is not Utf8"),
    }
}

/// Class constant at `index` resolved to its internal name.
fn class_name(pool: &[Constant], index: u16) -> Result<&str> {
    match pool.get(index as usize) {
        Some(Constant::Class(n)) => utf8(pool, *n),
        _ => bail!("constant {index} is not a class reference"),
    }
}
