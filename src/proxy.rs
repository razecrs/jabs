//! java.lang.reflect.Proxy, resolved at build time (as GraalVM native-image does it). The JVM
//! spins a proxy class at run time (ProxyGenerator, then defineClass), which an ahead-of-time
//! compiler cannot do. This pass decides the closed set of interface lists a program can ask
//! for a proxy of, writes the class JDK 25's ProxyGenerator would write for each one as real
//! bytecode, and appends it to the unit, so the ordinary verifier and compiler handle it like
//! the lambda classes of src/indy.rs. At run time the replaced Proxy.getProxyConstructor finds
//! the class in a table (src/cgen/reflect.rs, runtime/jabs_reflect.c); a list not decided here
//! stops there with a diagnostic naming its interfaces.
//!
//! The interface lists:
//! - every annotation interface that a runtime-visible annotation in any input class names
//!   (class, field, method and parameter annotations and annotation defaults, nested
//!   annotations included), and the annotation interfaces those use as element types: the JDK
//!   makes every annotation instance with `Proxy.newProxyInstance(loader, {type}, handler)`
//!   (AnnotationParser.annotationForMap);
//! - constant `new Class[] { A.class, ... }` arrays spelled out in a method that calls
//!   Proxy.newProxyInstance or Proxy.getProxyClass, when every element is an interface in the
//!   unit. An array built any other way is not seen; its list stops at run time.
//!
//! What a generated class has, as ProxyGenerator writes it: `extends Proxy implements <list>`,
//! a public (InvocationHandler) constructor, one `private static final Method mN` per proxied
//! method (hashCode, equals, toString first, from Object), set in <clinit> with getMethod, and
//! every method `public final`, calling `h.invoke(this, mN, args)` with primitives boxed, the
//! result unboxed or cast, and checked exceptions the method does not declare wrapped in
//! UndeclaredThrowableException. Two deliberate differences, both invisible to programs:
//! <clinit> names the interfaces with class constants instead of Class.forName(name, false,
//! loader), and the private `proxyClassLookup` used only by InvocationHandler.invokeDefault
//! (method handles, which JABS does not have) is left out.

use crate::classfile::{
    AnnotationAttribute, AnnotationIndex, ClassFile, Code, Constant, Handler, Member,
    annotation_references,
};
use crate::indy::{
    Asm, Pool, class_name, instruction_starts, is_reference, name_and_type, reference_class, slots,
    split_method, utf8, wrapper_of,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_SUPER: u16 = 0x0020;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ANNOTATION: u16 = 0x2000;

const OBJECT: &str = "java/lang/Object";
const CLASS: &str = "java/lang/Class";
const THROWABLE: &str = "java/lang/Throwable";
const PROXY: &str = "java/lang/reflect/Proxy";
const HANDLER: &str = "java/lang/reflect/InvocationHandler";
const HANDLER_FIELD: &str = "Ljava/lang/reflect/InvocationHandler;";
const INVOKE: &str =
    "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;";
const METHOD_FIELD: &str = "Ljava/lang/reflect/Method;";
const UNDECLARED: &str = "java/lang/reflect/UndeclaredThrowableException";
/// The constructor every proxy class has (ProxyGenerator.generateConstructor).
pub const CONSTRUCTOR: &str = "(Ljava/lang/reflect/InvocationHandler;)V";
/// The package of a proxy whose interfaces are all public: the first dynamic module's package,
/// where the JVM puts such proxies for the first class loader that asks.
const PUBLIC_PACKAGE: &str = "jdk/proxy1";

/// Deepest superinterface chain followed; real hierarchies are a few levels, and the bound
/// keeps a cyclic (malformed) input from looping.
const MAX_HIERARCHY_DEPTH: usize = 256;
/// Longest `new Class[] {...}` literal looked at; no program spells out more.
const MAX_LITERAL_INTERFACES: usize = 64;
/// Proxied methods per class: each takes a field, a method and several constants, and a class
/// file has at most 65535 of each; far beyond any real interface list.
const MAX_PROXY_METHODS: usize = 4096;
/// Constant-pool entries a generated class may use (the format's limit is 65535).
const MAX_POOL: usize = 60000;
/// Bytecode length of one generated method; exception tables address it with u16 offsets.
const MAX_CODE: usize = 60000;

/// A generated proxy class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// Internal name of the proxy class.
    pub name: String,
    /// Its interfaces, in the order Proxy.newProxyInstance is given them.
    pub interfaces: Vec<String>,
}

/// What the pass did.
#[derive(Debug, Default)]
pub struct Report {
    pub proxies: Vec<Generated>,
    /// (interface list, reason) for lists no class could be generated for.
    pub skipped: Vec<(Vec<String>, String)>,
}

/// Generate a proxy class for every interface list the program can ask for, appending them.
pub fn generate(classes: &mut Vec<ClassFile>) -> Report {
    let mut report = Report::default();
    let lists = {
        let by_name: HashMap<&str, &ClassFile> =
            classes.iter().map(|c| (c.name.as_str(), c)).collect();
        let mut lists: BTreeSet<Vec<String>> = annotation_interfaces(classes, &by_name)
            .into_iter()
            .map(|a| vec![a])
            .collect();
        lists.extend(literal_lists(classes, &by_name));
        lists
    };
    let mut taken: HashSet<String> = classes.iter().map(|c| c.name.clone()).collect();
    let mut generated = Vec::new();
    let mut number = 0usize;
    for list in lists {
        let by_name: HashMap<&str, &ClassFile> =
            classes.iter().map(|c| (c.name.as_str(), c)).collect();
        let result = proxy_package(&list, &by_name).and_then(|(package, access)| {
            // The JVM numbers proxies $Proxy0, $Proxy1, ... in creation order; any free name
            // will do here, since the name is only seen through Class.getName.
            let name = loop {
                let candidate = if package.is_empty() {
                    format!("$Proxy{number}")
                } else {
                    format!("{package}/$Proxy{number}")
                };
                number += 1;
                if !taken.contains(&candidate) {
                    break candidate;
                }
            };
            proxy_class(&name, access, &list, &by_name)
        });
        match result {
            Ok(class) => {
                taken.insert(class.name.clone());
                report.proxies.push(Generated {
                    name: class.name.clone(),
                    interfaces: list,
                });
                generated.push(class);
            }
            Err(reason) => report.skipped.push((list, reason)),
        }
    }
    classes.extend(generated);
    report
}

// ---- Which interface lists ----

/// Every annotation-carrying attribute of a class: (payload, kind).
pub fn annotation_attributes(class: &ClassFile) -> Vec<(&[u8], AnnotationAttribute)> {
    let mut out: Vec<(&[u8], AnnotationAttribute)> = Vec::new();
    if let Some(data) = &class.reflect.annotations {
        out.push((data, AnnotationAttribute::Annotations));
    }
    for m in class.methods.iter().chain(&class.fields) {
        if let Some(data) = &m.reflect.annotations {
            out.push((data, AnnotationAttribute::Annotations));
        }
        if let Some(data) = &m.reflect.parameter_annotations {
            out.push((data, AnnotationAttribute::ParameterAnnotations));
        }
        if let Some(data) = &m.reflect.annotation_default {
            out.push((data, AnnotationAttribute::Default));
        }
    }
    out
}

/// The class a field or return descriptor names, looking through arrays: `Lp/A;` and `[[Lp/A;`
/// give `p/A`; primitives and void give None.
fn named_class(descriptor: &str) -> Option<&str> {
    descriptor
        .trim_start_matches('[')
        .strip_prefix('L')?
        .strip_suffix(';')
}

/// Every class an annotation parse of `class`'s attributes loads by name: annotation types
/// (nested ones too), enum element types and the classes of Class elements. Sorted, unique.
pub fn annotation_loaded_classes(class: &ClassFile) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (data, kind) in annotation_attributes(class) {
        // Validated when the class was parsed (ClassFile::parse); a payload that fails here
        // names nothing the build can rely on, so it adds nothing.
        let Ok(references) = annotation_references(data, kind) else {
            continue;
        };
        for (index, what) in references {
            let wanted = matches!(
                what,
                AnnotationIndex::AnnotationType
                    | AnnotationIndex::EnumType
                    | AnnotationIndex::ClassValue
            );
            if !wanted {
                continue;
            }
            if let Some(name) = utf8(class, index).and_then(named_class) {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn is_annotation(by_name: &HashMap<&str, &ClassFile>, name: &str) -> bool {
    by_name.get(name).is_some_and(|c| {
        c.access & (ACC_INTERFACE | ACC_ANNOTATION) == ACC_INTERFACE | ACC_ANNOTATION
    })
}

/// Annotation interfaces in the unit that some runtime-visible annotation names, plus the
/// annotation interfaces those use as element types (an element's default or a nested value
/// is an instance of one).
fn annotation_interfaces(
    classes: &[ClassFile],
    by_name: &HashMap<&str, &ClassFile>,
) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for class in classes {
        for (data, kind) in annotation_attributes(class) {
            let Ok(references) = annotation_references(data, kind) else {
                continue;
            };
            for (index, what) in references {
                if what != AnnotationIndex::AnnotationType {
                    continue;
                }
                let Some(name) = utf8(class, index).and_then(named_class) else {
                    continue;
                };
                if is_annotation(by_name, name) {
                    found.insert(name.to_string());
                }
            }
        }
    }
    // Element types: each name enters the work list once, so this ends after at most one step
    // per annotation interface in the unit.
    let mut work: Vec<String> = found.iter().cloned().collect();
    while let Some(name) = work.pop() {
        let Some(class) = by_name.get(name.as_str()) else {
            continue;
        };
        for m in &class.methods {
            let Some((_, ret)) = split_method(&m.descriptor) else {
                continue;
            };
            let Some(element) = named_class(&ret) else {
                continue;
            };
            if is_annotation(by_name, element) && found.insert(element.to_string()) {
                work.push(element.to_string());
            }
        }
    }
    found
}

/// The int an iconst/bipush/sipush at `pc` pushes.
fn int_constant(code: &[u8], pc: usize) -> Option<i32> {
    match *code.get(pc)? {
        op @ 0x02..=0x08 => Some(i32::from(op) - 3),
        0x10 => Some(i32::from(*code.get(pc + 1)? as i8)),
        0x11 => Some(i32::from(i16::from_be_bytes([
            *code.get(pc + 1)?,
            *code.get(pc + 2)?,
        ]))),
        _ => None,
    }
}

/// The class a ldc/ldc_w at `pc` loads, if it loads a Class constant.
fn ldc_class<'a>(class: &'a ClassFile, code: &[u8], pc: usize) -> Option<&'a str> {
    let index = match *code.get(pc)? {
        0x12 => u16::from(*code.get(pc + 1)?),
        0x13 => u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]),
        _ => return None,
    };
    class_name(class, index)
}

/// Whether the invokestatic at `pc` calls Proxy.newProxyInstance or Proxy.getProxyClass.
fn calls_proxy_factory(class: &ClassFile, code: &[u8], pc: usize) -> bool {
    if code.get(pc) != Some(&0xb8) {
        return false;
    }
    let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) else {
        return false;
    };
    let Some(Constant::MethodRef {
        class: owner,
        name_and_type: nt,
    }) = class.pool.get(usize::from(u16::from_be_bytes([hi, lo])))
    else {
        return false;
    };
    class_name(class, *owner) == Some(PROXY)
        && name_and_type(class, *nt)
            .is_some_and(|(name, _)| name == "newProxyInstance" || name == "getProxyClass")
}

/// Constant interface lists in methods that make proxies: `n; anewarray Class;` followed by n
/// times `dup; i; ldc X; aastore`, every X an interface in the unit.
fn literal_lists(
    classes: &[ClassFile],
    by_name: &HashMap<&str, &ClassFile>,
) -> BTreeSet<Vec<String>> {
    let mut out = BTreeSet::new();
    for class in classes {
        for method in &class.methods {
            let Some(code) = method.code.as_ref() else {
                continue;
            };
            let code = &code.bytecode;
            let starts = instruction_starts(code);
            if !starts
                .iter()
                .any(|&pc| calls_proxy_factory(class, code, pc))
            {
                continue;
            }
            for (k, &pc) in starts.iter().enumerate() {
                if code[pc] != 0xbd {
                    continue;
                }
                let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) else {
                    continue;
                };
                if class_name(class, u16::from_be_bytes([hi, lo])) != Some(CLASS) {
                    continue;
                }
                let Some(count) = k
                    .checked_sub(1)
                    .and_then(|p| int_constant(code, starts[p]))
                    .and_then(|n| usize::try_from(n).ok())
                else {
                    continue;
                };
                if count == 0 || count > MAX_LITERAL_INTERFACES {
                    continue;
                }
                let mut names: Vec<String> = Vec::new();
                for i in 0..count {
                    let at = k + 1 + 4 * i;
                    let Some(group) = starts.get(at..at + 4) else {
                        break;
                    };
                    let element = (code[group[0]] == 0x59
                        && int_constant(code, group[1]) == i32::try_from(i).ok()
                        && code[group[3]] == 0x53)
                        .then(|| ldc_class(class, code, group[2]))
                        .flatten();
                    let Some(element) = element else {
                        break;
                    };
                    names.push(element.to_string());
                }
                let interfaces = names.len() == count
                    && names.iter().all(|n| {
                        by_name
                            .get(n.as_str())
                            .is_some_and(|c| c.access & ACC_INTERFACE != 0)
                    });
                if interfaces {
                    out.insert(names);
                }
            }
        }
    }
    out
}

// ---- The class ProxyGenerator writes ----

/// The package and access flags of a list's proxy, as Proxy.ProxyBuilder decides them: a
/// public final class in the dynamic module's package when every interface is public, else a
/// final class in the package of the non-public interfaces (which must all share one).
fn proxy_package(
    list: &[String],
    by_name: &HashMap<&str, &ClassFile>,
) -> Result<(String, u16), String> {
    let mut package: Option<&str> = None;
    for name in list {
        let Some(class) = by_name.get(name.as_str()) else {
            return Err(format!("{name} is not in the unit"));
        };
        if class.access & ACC_INTERFACE == 0 {
            return Err(format!("{name} is not an interface"));
        }
        if class.access & ACC_PUBLIC != 0 {
            continue;
        }
        let own = name.rsplit_once('/').map_or("", |(p, _)| p);
        match package {
            Some(p) if p != own => {
                return Err("non-public interfaces from different packages".to_string());
            }
            _ => package = Some(own),
        }
    }
    Ok(match package {
        None => (
            PUBLIC_PACKAGE.to_string(),
            ACC_PUBLIC | ACC_FINAL | ACC_SUPER,
        ),
        Some(p) => (p.to_string(), ACC_FINAL | ACC_SUPER),
    })
}

/// One method of the proxy: its Method comes from `from.getMethod(name, parameter types)`.
struct ProxyMethod {
    name: String,
    descriptor: String,
    from: String,
    exceptions: Vec<String>,
}

/// The parameter part of a method descriptor, `(...)`; ProxyGenerator's "short signature" is
/// the name plus this, so methods differing only in return type collide.
fn parameters_of(descriptor: &str) -> Option<&str> {
    let close = descriptor.find(')')?;
    Some(&descriptor[..=close])
}

/// The proxied methods in ProxyGenerator's order: Object's hashCode, equals and toString, then
/// each interface's public instance methods (its own, then its superinterfaces'). A method
/// several interfaces declare is proxied once, with the first interface's Method; lists where
/// such declarations differ in return type or exceptions are refused (the JVM picks the most
/// specific return type and intersects the exceptions, which nothing here needs yet).
fn proxy_methods(
    list: &[String],
    by_name: &HashMap<&str, &ClassFile>,
) -> Result<Vec<ProxyMethod>, String> {
    let object = |name: &str, descriptor: &str| ProxyMethod {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        from: OBJECT.to_string(),
        exceptions: Vec::new(),
    };
    let mut out = vec![
        object("hashCode", "()I"),
        object("equals", "(Ljava/lang/Object;)Z"),
        object("toString", "()Ljava/lang/String;"),
    ];
    for interface in list {
        // The interface and its superinterfaces, depth first, each once.
        let mut order: Vec<&ClassFile> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<(&str, usize)> = vec![(interface.as_str(), 0)];
        while let Some((name, depth)) = stack.pop() {
            if depth > MAX_HIERARCHY_DEPTH {
                return Err(format!("{interface}: superinterfaces nest too deeply"));
            }
            if !seen.insert(name) {
                continue;
            }
            let Some(class) = by_name.get(name) else {
                return Err(format!("superinterface {name} is not in the unit"));
            };
            if class.access & ACC_INTERFACE == 0 {
                return Err(format!("{name} is not an interface"));
            }
            order.push(class);
            for s in class.interfaces.iter().rev() {
                stack.push((s.as_str(), depth + 1));
            }
        }
        // Methods this interface (with its superinterfaces) already gave: a superinterface's
        // declaration of the same method is overridden by the subinterface's.
        let mut own: HashSet<(String, String)> = HashSet::new();
        for class in order {
            for m in &class.methods {
                if m.access & (ACC_STATIC | ACC_PRIVATE) != 0 || m.name.starts_with('<') {
                    continue;
                }
                let Some(parameters) = parameters_of(&m.descriptor) else {
                    return Err(format!("{}.{}: malformed descriptor", class.name, m.name));
                };
                let key = (m.name.clone(), parameters.to_string());
                if own.contains(&key) {
                    continue;
                }
                own.insert(key);
                let existing = out
                    .iter()
                    .find(|p| p.name == m.name && parameters_of(&p.descriptor) == Some(parameters));
                if let Some(existing) = existing {
                    if existing.from == OBJECT {
                        continue;
                    }
                    if existing.descriptor != m.descriptor {
                        return Err(format!(
                            "{}{} is declared with different return types",
                            m.name, parameters
                        ));
                    }
                    let mut a = existing.exceptions.clone();
                    let mut b = m.reflect.exceptions.clone();
                    a.sort();
                    b.sort();
                    if a != b {
                        return Err(format!(
                            "{}{} is declared with different exceptions",
                            m.name, m.descriptor
                        ));
                    }
                    continue;
                }
                out.push(ProxyMethod {
                    name: m.name.clone(),
                    descriptor: m.descriptor.clone(),
                    from: interface.clone(),
                    exceptions: m.reflect.exceptions.clone(),
                });
                if out.len() > MAX_PROXY_METHODS {
                    return Err("too many methods for one proxy class".to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Whether class `a` is `b` or a subclass of it; None when the chain leaves the unit.
fn is_subclass(a: &str, b: &str, by_name: &HashMap<&str, &ClassFile>) -> Option<bool> {
    let mut at = a;
    for _ in 0..MAX_HIERARCHY_DEPTH {
        if at == b {
            return Some(true);
        }
        let class = by_name.get(at)?;
        match class.super_name.as_deref() {
            Some(next) => at = next,
            None => return Some(false),
        }
    }
    None
}

/// The exception types a proxy method lets through unwrapped, in ProxyGenerator's order
/// (computeUniqueCatchList): Error, RuntimeException and the declared checked exceptions with
/// subclasses of listed ones dropped. Empty when Throwable is declared: then nothing is caught.
fn catch_list(
    exceptions: &[String],
    by_name: &HashMap<&str, &ClassFile>,
) -> Result<Vec<String>, String> {
    let mut list = vec![
        "java/lang/Error".to_string(),
        "java/lang/RuntimeException".to_string(),
    ];
    let subclass = |a: &str, b: &str| {
        is_subclass(a, b, by_name)
            .ok_or_else(|| format!("the superclasses of {a} are not all in the unit"))
    };
    for ex in exceptions {
        if ex == THROWABLE || ex == OBJECT {
            list.clear();
            break;
        }
        if !subclass(ex, THROWABLE)? {
            continue;
        }
        let mut covered = false;
        let mut j = 0;
        while j < list.len() {
            if subclass(ex, &list[j])? {
                covered = true;
                break;
            }
            if subclass(&list[j], ex)? {
                list.remove(j);
            } else {
                j += 1;
            }
        }
        if !covered {
            list.push(ex.clone());
        }
    }
    Ok(list)
}

/// The unboxing method of a primitive's wrapper (`intValue` for I).
fn unbox_name(primitive: &str) -> &'static str {
    match primitive {
        "Z" => "booleanValue",
        "B" => "byteValue",
        "C" => "charValue",
        "S" => "shortValue",
        "I" => "intValue",
        "J" => "longValue",
        "F" => "floatValue",
        _ => "doubleValue",
    }
}

/// A generated member with code (Code's bounds are generous: the verifier computes the real
/// stack shape).
fn code_member(
    access: u16,
    name: &str,
    descriptor: &str,
    asm: Asm,
    handlers: Vec<Handler>,
    max_locals: u16,
    exceptions: Vec<String>,
) -> Result<Member, String> {
    if asm.code.len() > MAX_CODE {
        return Err(format!("{name}{descriptor}: generated code too long"));
    }
    let mut member = Member {
        access,
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        code: Some(Code {
            max_stack: 16,
            max_locals,
            bytecode: asm.code,
            handlers,
        }),
        constant_value: None,
        annotations: Vec::new(),
        reflect: Default::default(),
    };
    member.reflect.exceptions = exceptions;
    Ok(member)
}

/// Bytecode offset as an exception-table u16 (code_member bounds every method below 65535).
fn offset(asm: &Asm) -> Result<u16, String> {
    u16::try_from(asm.code.len()).map_err(|_| "generated code too long".to_string())
}

/// One proxied method (ProxyGenerator.ProxyMethod.generateMethod).
fn proxy_method(
    pool: &mut Pool,
    this: &str,
    field: &str,
    m: &ProxyMethod,
    by_name: &HashMap<&str, &ClassFile>,
) -> Result<Member, String> {
    let (params, ret) =
        split_method(&m.descriptor).ok_or_else(|| format!("{}: malformed descriptor", m.name))?;
    let mut asm = Asm::default();
    let h = pool.field(PROXY, "h", HANDLER_FIELD);
    let method = pool.field(this, field, METHOD_FIELD);
    asm.op(0x2a).op(0xb4).u16(h).op(0x2a).op(0xb2).u16(method);
    let mut slot: u16 = 1;
    if params.is_empty() {
        asm.op(0x01);
    } else {
        let count = i32::try_from(params.len()).map_err(|_| "too many parameters".to_string())?;
        let object = pool.class(OBJECT);
        asm.int(pool, count).op(0xbd).u16(object);
        for (i, ty) in params.iter().enumerate() {
            asm.op(0x59).int(pool, i as i32).load(ty, slot);
            slot = slot
                .checked_add(slots(ty))
                .ok_or_else(|| "too many parameter slots".to_string())?;
            if let Some(wrapper) = wrapper_of(ty) {
                let value_of =
                    pool.method(wrapper, "valueOf", &format!("({ty})L{wrapper};"), false);
                asm.invoke(0xb8, value_of, slots(ty));
            }
            asm.op(0x53);
        }
    }
    let invoke = pool.method(HANDLER, "invoke", INVOKE, true);
    asm.invoke(0xb9, invoke, 3);
    if ret == "V" {
        asm.op(0x57).op(0xb1);
    } else if let Some(wrapper) = wrapper_of(&ret) {
        asm.checkcast(pool, wrapper);
        let unbox = pool.method(wrapper, unbox_name(&ret), &format!("(){ret}"), false);
        asm.invoke(0xb6, unbox, 0).ret(&ret);
    } else {
        if !is_reference(&ret) {
            return Err(format!("{}: malformed return type", m.name));
        }
        if ret != "Ljava/lang/Object;" {
            asm.checkcast(pool, &reference_class(&ret));
        }
        asm.op(0xb0);
    }
    let mut handlers = Vec::new();
    let catches = catch_list(&m.exceptions, by_name)?;
    if !catches.is_empty() {
        // try { ... } catch (Error | RuntimeException | declared e) { throw e; }
        // catch (Throwable t) { throw new UndeclaredThrowableException(t); }
        let end = offset(&asm)?;
        asm.op(0xbf);
        for c in &catches {
            let catch_type = pool.class(c);
            handlers.push(Handler {
                start_pc: 0,
                end_pc: end,
                handler_pc: end,
                catch_type,
            });
        }
        let wrap = offset(&asm)?;
        let undeclared = pool.class(UNDECLARED);
        let init = pool.method(UNDECLARED, "<init>", "(Ljava/lang/Throwable;)V", false);
        asm.op(0xbb)
            .u16(undeclared)
            .op(0x5a)
            .op(0x5f)
            .invoke(0xb7, init, 1)
            .op(0xbf);
        handlers.push(Handler {
            start_pc: 0,
            end_pc: end,
            handler_pc: wrap,
            catch_type: 0,
        });
    }
    code_member(
        ACC_PUBLIC | ACC_FINAL,
        &m.name,
        &m.descriptor,
        asm,
        handlers,
        slot,
        m.exceptions.clone(),
    )
}

/// Push the Class of a parameter type: `Integer.TYPE` for int, a class constant otherwise.
fn push_type(asm: &mut Asm, pool: &mut Pool, ty: &str) {
    if let Some(wrapper) = wrapper_of(ty) {
        let field = pool.field(wrapper, "TYPE", "Ljava/lang/Class;");
        asm.op(0xb2).u16(field);
    } else {
        let class = pool.class(&reference_class(ty));
        asm.ldc(class, false);
    }
}

/// <clinit>: every mN = From.class.getMethod(name, parameter types), with a
/// NoSuchMethodException turned into NoSuchMethodError (ProxyGenerator.generateStaticInitializer).
fn static_initializer(
    pool: &mut Pool,
    this: &str,
    methods: &[ProxyMethod],
) -> Result<Member, String> {
    let mut asm = Asm::default();
    let get_method = pool.method(
        CLASS,
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        false,
    );
    for (i, m) in methods.iter().enumerate() {
        let (params, _) = split_method(&m.descriptor)
            .ok_or_else(|| format!("{}: malformed descriptor", m.name))?;
        let from = pool.class(&m.from);
        let name = pool.string(m.name.encode_utf16().collect());
        asm.ldc(from, false).ldc(name, false);
        let count = i32::try_from(params.len()).map_err(|_| "too many parameters".to_string())?;
        let class = pool.class(CLASS);
        asm.int(pool, count).op(0xbd).u16(class);
        for (j, ty) in params.iter().enumerate() {
            asm.op(0x59).int(pool, j as i32);
            push_type(&mut asm, pool, ty);
            asm.op(0x53);
        }
        let field = pool.field(this, &format!("m{i}"), METHOD_FIELD);
        asm.invoke(0xb6, get_method, 2).op(0xb3).u16(field);
        // As in proxy_class: one method's entries stay well inside the margin below 65535.
        if pool.entries.len() > MAX_POOL || asm.code.len() > MAX_CODE {
            return Err("static initializer too large for one proxy class".to_string());
        }
    }
    asm.op(0xb1);
    let end = offset(&asm)?;
    let missing = pool.class("java/lang/NoSuchMethodException");
    let error = pool.class("java/lang/NoSuchMethodError");
    let message = pool.method(THROWABLE, "getMessage", "()Ljava/lang/String;", false);
    let init = pool.method(
        "java/lang/NoSuchMethodError",
        "<init>",
        "(Ljava/lang/String;)V",
        false,
    );
    asm.op(0xbb)
        .u16(error)
        .op(0x5a)
        .op(0x5f)
        .invoke(0xb6, message, 0)
        .invoke(0xb7, init, 1)
        .op(0xbf);
    let handlers = vec![Handler {
        start_pc: 0,
        end_pc: end,
        handler_pc: end,
        catch_type: missing,
    }];
    code_member(ACC_STATIC, "<clinit>", "()V", asm, handlers, 0, Vec::new())
}

/// The proxy class for one interface list.
fn proxy_class(
    name: &str,
    access: u16,
    list: &[String],
    by_name: &HashMap<&str, &ClassFile>,
) -> Result<ClassFile, String> {
    let methods = proxy_methods(list, by_name)?;
    let mut pool = Pool::new();
    let mut members = Vec::new();
    // public $ProxyN(InvocationHandler h) { super(h); }
    let mut ctor = Asm::default();
    let super_init = pool.method(PROXY, "<init>", CONSTRUCTOR, false);
    ctor.op(0x2a).op(0x2b).invoke(0xb7, super_init, 1).op(0xb1);
    members.push(code_member(
        ACC_PUBLIC,
        "<init>",
        CONSTRUCTOR,
        ctor,
        Vec::new(),
        2,
        Vec::new(),
    )?);
    let mut fields = Vec::new();
    for (i, m) in methods.iter().enumerate() {
        let field = format!("m{i}");
        fields.push(Member {
            access: ACC_PRIVATE | ACC_STATIC | ACC_FINAL,
            name: field.clone(),
            descriptor: METHOD_FIELD.to_string(),
            code: None,
            constant_value: None,
            annotations: Vec::new(),
            reflect: Default::default(),
        });
        members.push(proxy_method(&mut pool, name, &field, m, by_name)?);
        // Checked as it grows: Pool indexes entries with u16, and one method adds far fewer
        // than the margin between MAX_POOL and 65535.
        if pool.entries.len() > MAX_POOL {
            return Err("constant pool too large for one proxy class".to_string());
        }
    }
    members.push(static_initializer(&mut pool, name, &methods)?);
    if pool.entries.len() > MAX_POOL {
        return Err("constant pool too large for one proxy class".to_string());
    }
    Ok(ClassFile {
        minor: 0,
        major: 52,
        pool: pool.entries,
        access,
        name: name.to_string(),
        super_name: Some(PROXY.to_string()),
        interfaces: list.to_vec(),
        fields,
        methods: members,
        string_units: pool.string_units,
        bootstrap_methods: Vec::new(),
        reflect: Default::default(),
    })
}

/// For each class, the classes annotation parsing of its attributes loads (see
/// [`annotation_loaded_classes`]); classes whose annotations name nothing are left out.
pub fn loaded_by_annotations(classes: &[ClassFile]) -> BTreeMap<String, BTreeSet<String>> {
    classes
        .iter()
        .map(|c| (c.name.clone(), annotation_loaded_classes(c)))
        .filter(|(_, names)| !names.is_empty())
        .collect()
}
