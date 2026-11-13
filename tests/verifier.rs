//! The verifier must reject unsafe bytecode, not just accept javac's output.
//!
//! Classes are assembled by hand so they can contain shapes javac never emits
//! (as bytecode rewriters like Mixin/ASM can). Each bad method must come back
//! unsupported, for the stated reason; one good method guards the builder.

use jabs::{cgen, classfile::ClassFile};
use std::collections::HashMap;

/// Minimal class-file assembler: constant pool, fields and methods with Code attributes.
#[derive(Default)]
struct Builder {
    pool: Vec<Vec<u8>>,
    interned: HashMap<Vec<u8>, u16>,
    fields: Vec<(u16, u16, u16)>,
    methods: Vec<Vec<u8>>,
}

impl Builder {
    /// Add (or reuse) a constant-pool entry; returns its index.
    fn constant(&mut self, entry: Vec<u8>) -> u16 {
        if let Some(&i) = self.interned.get(&entry) {
            return i;
        }
        self.pool.push(entry.clone());
        let index = self.pool.len() as u16;
        self.interned.insert(entry, index);
        index
    }
    fn utf8(&mut self, text: &str) -> u16 {
        let mut e = vec![1];
        e.extend((text.len() as u16).to_be_bytes());
        e.extend(text.as_bytes());
        self.constant(e)
    }
    fn class(&mut self, name: &str) -> u16 {
        let n = self.utf8(name);
        let mut e = vec![7];
        e.extend(n.to_be_bytes());
        self.constant(e)
    }
    /// A Fieldref (tag 9) or Methodref (tag 10).
    fn member(&mut self, tag: u8, owner: &str, name: &str, descriptor: &str) -> u16 {
        let class = self.class(owner);
        let (n, d) = (self.utf8(name), self.utf8(descriptor));
        let mut nat = vec![12];
        nat.extend(n.to_be_bytes());
        nat.extend(d.to_be_bytes());
        let nat = self.constant(nat);
        let mut e = vec![tag];
        e.extend(class.to_be_bytes());
        e.extend(nat.to_be_bytes());
        self.constant(e)
    }
    fn field(&mut self, access: u16, name: &str, descriptor: &str) {
        let (n, d) = (self.utf8(name), self.utf8(descriptor));
        self.fields.push((access, n, d));
    }
    fn method(
        &mut self,
        access: u16,
        name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: &[u8],
    ) {
        let (n, d, attr) = (self.utf8(name), self.utf8(descriptor), self.utf8("Code"));
        let mut m = Vec::new();
        m.extend(access.to_be_bytes());
        m.extend(n.to_be_bytes());
        m.extend(d.to_be_bytes());
        m.extend(1u16.to_be_bytes());
        m.extend(attr.to_be_bytes());
        m.extend((12 + code.len() as u32).to_be_bytes());
        m.extend(max_stack.to_be_bytes());
        m.extend(max_locals.to_be_bytes());
        m.extend((code.len() as u32).to_be_bytes());
        m.extend(code);
        m.extend(0u16.to_be_bytes()); // exception table
        m.extend(0u16.to_be_bytes()); // attributes
        self.methods.push(m);
    }
    fn build(mut self, name: &str) -> ClassFile {
        let this = self.class(name);
        let parent = self.class("java/lang/Object");
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend((self.pool.len() as u16 + 1).to_be_bytes());
        for e in &self.pool {
            b.extend(e);
        }
        b.extend(0x0021u16.to_be_bytes());
        b.extend(this.to_be_bytes());
        b.extend(parent.to_be_bytes());
        b.extend(0u16.to_be_bytes());
        b.extend((self.fields.len() as u16).to_be_bytes());
        for (access, n, d) in &self.fields {
            b.extend(access.to_be_bytes());
            b.extend(n.to_be_bytes());
            b.extend(d.to_be_bytes());
            b.extend(0u16.to_be_bytes());
        }
        b.extend((self.methods.len() as u16).to_be_bytes());
        for m in &self.methods {
            b.extend(m);
        }
        b.extend(0u16.to_be_bytes());
        ClassFile::parse(&b).unwrap()
    }
}

const STATIC: u16 = 0x0008;

/// One bad method: (name, descriptor, max_stack, max_locals, code, expected reason fragment).
type Case = (&'static str, &'static str, u16, u16, Vec<u8>, &'static str);

/// Bytes of a big-endian u16, for instruction operands.
fn be(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

#[test]
fn verifier_rejects_unsafe_bytecode() {
    let mut b = Builder::default();
    b.field(0, "x", "I");
    b.field(0, "next", "LBad;");
    let fx = b.member(9, "Bad", "x", "I");
    let fnext = b.member(9, "Bad", "next", "LBad;");
    let inst = b.member(10, "Bad", "inst", "()V");
    let object_init = b.member(10, "java/lang/Object", "<init>", "()V");
    let bad = b.class("Bad");

    // Control: a well-formed method must compile, or the builder itself is broken.
    b.method(STATIC, "good", "()I", 2, 0, &[0x05, 0x06, 0x60, 0xac]);
    b.method(0, "inst", "()V", 0, 1, &[0xb1]);
    let [o1, o2] = be(object_init);
    b.method(0, "<init>", "()V", 1, 1, &[0x2a, 0xb7, o1, o2, 0xb1]);

    // (method, descriptor, max_stack, max_locals, code, expected reason fragment)
    let [x1, x2] = be(fx);
    let [n1, n2] = be(fnext);
    let [i1, i2] = be(inst);
    let [c1, c2] = be(bad);
    let cases: Vec<Case> = vec![
        // lconst_0; iconst_1; iadd: a long where an int is required.
        (
            "mixLong",
            "()I",
            3,
            0,
            vec![0x09, 0x04, 0x60, 0xac],
            "expected I",
        ),
        // iconst_0; getfield Bad.x: an int used as an object.
        (
            "getfieldOnInt",
            "()I",
            1,
            0,
            vec![0x03, 0xb4, x1, x2, 0xac],
            "expected a Bad reference",
        ),
        // aload_0 of an int parameter.
        (
            "aloadInt",
            "(I)I",
            1,
            1,
            vec![0x2a, 0x57, 0x03, 0xac],
            "aload of local 0",
        ),
        // new Bad; getfield Bad.x: reading a field before the constructor ran.
        (
            "useBeforeInit",
            "()I",
            2,
            0,
            vec![0xbb, c1, c2, 0xb4, x1, x2, 0xac],
            "expected a Bad reference",
        ),
        // iadd on an empty stack.
        (
            "underflow",
            "()I",
            2,
            0,
            vec![0x60, 0xac],
            "stack underflow",
        ),
        // bipush 7; goto -1: a jump into the middle of bipush.
        (
            "intoMiddle",
            "()I",
            1,
            0,
            vec![0x10, 0x07, 0xa7, 0xff, 0xff],
            "not an instruction",
        ),
        // iconst_0; pop: execution runs off the end of the code.
        (
            "fallsOff",
            "()V",
            1,
            0,
            vec![0x03, 0x57],
            "falls off the end",
        ),
        // Two paths reach pc 9 with an int and a long on the stack.
        (
            "joinMismatch",
            "(I)I",
            2,
            1,
            vec![0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x09, 0xac],
            "disagree",
        ),
        // aconst_null; ireturn: a reference returned as an int.
        ("returnNull", "()I", 1, 0, vec![0x01, 0xac], "expected I"),
        // putfield Bad.next with an Other: unrelated class stored into a Bad field.
        (
            "wrongPutfield",
            "(LBad;LOther;)V",
            2,
            2,
            vec![0x2a, 0x2b, 0xb5, n1, n2, 0xb1],
            "expected a Bad reference",
        ),
        // invokevirtual Bad.inst on an Other receiver.
        (
            "wrongReceiver",
            "(LOther;)V",
            1,
            1,
            vec![0x2a, 0xb6, i1, i2, 0xb1],
            "expected a Bad reference",
        ),
        // iload 5 with one local.
        (
            "localOutOfRange",
            "()I",
            1,
            1,
            vec![0x15, 0x05, 0xac],
            "out of range",
        ),
        // Two values pushed with max_stack 1.
        (
            "overflow",
            "()V",
            1,
            0,
            vec![0x03, 0x03, 0x57, 0x57, 0xb1],
            "max_stack",
        ),
    ];
    for (name, descriptor, max_stack, max_locals, code, _) in &cases {
        b.method(STATIC, name, descriptor, *max_stack, *max_locals, code);
    }
    let bad_class = b.build("Bad");
    let other = Builder::default().build("Other");

    let unit = cgen::compile_unit(&[bad_class, other]);
    let status = |name: &str| {
        unit.methods
            .iter()
            .find(|m| m.class == "Bad" && m.name == name)
            .unwrap_or_else(|| panic!("no method {name}"))
            .status
            .clone()
    };
    assert!(
        matches!(status("good"), cgen::MethodStatus::Compiled { .. }),
        "{:?}",
        status("good")
    );
    for (name, _, _, _, _, expected) in &cases {
        match status(name) {
            cgen::MethodStatus::Unsupported { reason } => {
                assert!(
                    reason.contains(expected),
                    "{name}: rejected, but for `{reason}` (expected `{expected}`)"
                )
            }
            other => panic!("{name}: unsafe bytecode was accepted: {other:?}"),
        }
    }
}

#[test]
fn constructor_must_call_super() {
    let mut b = Builder::default();
    // <init> that returns without calling Object.<init>.
    b.method(0, "<init>", "()V", 0, 1, &[0xb1]);
    let unit = cgen::compile_unit(&[b.build("NoSuper")]);
    let m = unit.methods.iter().find(|m| m.name == "<init>").unwrap();
    match &m.status {
        cgen::MethodStatus::Unsupported { reason } => assert!(
            reason.contains("before calling a superclass constructor"),
            "{reason}"
        ),
        other => panic!("constructor without super() accepted: {other:?}"),
    }
}
