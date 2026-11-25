//! Reflection: the metadata java.lang.reflect needs, the JDK layouts and methods the runtime's
//! reflection code (runtime/jabs_reflect.c) uses, and the hooks that put reflection into a
//! program. The C side of the encoding is documented in runtime/jabs_reflect.h.
//!
//! Only classes with a runtime record get metadata: an object can only exist, and a Class can
//! only be asked about its members, for classes the program has a record for (`add_records`
//! adds every superclass and superinterface of a recorded class).

use super::*;
use crate::classfile::{AnnotationAttribute, annotation_pool_indices};

/// HotSpot's JVM_RECOGNIZED_CLASS_MODIFIERS, applied to a class's access flags at load.
const CLASS_MODIFIERS: u16 = 0x7631;
/// RECOGNIZED_INNER_CLASS_MODIFIERS: class modifiers plus private, protected and static.
const INNER_CLASS_MODIFIERS: u16 = 0x763F;
const ACC_SUPER: u16 = 0x0020;
const ACC_ENUM: u16 = 0x4000;

/// The value kinds in JABS_RK_* order: descriptor code, box class, the word in the JDK's
/// accessor class name (MethodHandle<word>FieldAccessorImpl) and in its own get/set methods.
const KINDS: [(&str, &str, &str, &str); 9] = [
    ("Z", "java/lang/Boolean", "Boolean", "Boolean"),
    ("B", "java/lang/Byte", "Byte", "Byte"),
    ("C", "java/lang/Character", "Character", "Char"),
    ("S", "java/lang/Short", "Short", "Short"),
    ("I", "java/lang/Integer", "Integer", "Int"),
    ("J", "java/lang/Long", "Long", "Long"),
    ("F", "java/lang/Float", "Float", "Float"),
    ("D", "java/lang/Double", "Double", "Double"),
    ("Ljava/lang/Object;", OBJECT, "Object", ""),
];

const METHOD: &str = "java/lang/reflect/Method";
const FIELD: &str = "java/lang/reflect/Field";
const CONSTRUCTOR: &str = "java/lang/reflect/Constructor";
const CONSTANT_POOL: &str = "jdk/internal/reflect/ConstantPool";
const ACCESSOR_BASE: &str = "jdk/internal/reflect/MethodHandleFieldAccessorImpl";
const ACCESSOR_FACTORY: &str = "jdk/internal/reflect/MethodHandleAccessorFactory";
const IAE: &str = "java/lang/IllegalArgumentException";
const ITE: &str = "java/lang/reflect/InvocationTargetException";
const ACCESSOR_INIT: &str =
    "(Ljava/lang/reflect/Field;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;ZZ)V";

fn accessor_class(word: &str) -> String {
    format!("jdk/internal/reflect/MethodHandle{word}FieldAccessorImpl")
}

/// JABS_RR_* classes, in enum order.
fn record_classes() -> Vec<String> {
    let mut out = vec![
        METHOD.to_string(),
        FIELD.to_string(),
        CONSTRUCTOR.to_string(),
        CONSTANT_POOL.to_string(),
        IAE.to_string(),
        ITE.to_string(),
    ];
    out.extend(KINDS[..8].iter().map(|k| k.1.to_string()));
    out.extend(KINDS.iter().map(|k| accessor_class(k.2)));
    out
}

/// JABS_RF_* fields, in enum order: (declaring class, name, descriptor).
fn layout_fields() -> Vec<(String, &'static str, &'static str)> {
    let class = "Ljava/lang/Class;";
    let classes = "[Ljava/lang/Class;";
    let string = "Ljava/lang/String;";
    let mut out: Vec<(String, &str, &str)> = Vec::new();
    for (name, descriptor) in [
        ("clazz", class),
        ("slot", "I"),
        ("name", string),
        ("returnType", class),
        ("parameterTypes", classes),
        ("exceptionTypes", classes),
        ("modifiers", "I"),
        ("signature", string),
        ("annotations", "[B"),
        ("parameterAnnotations", "[B"),
        ("annotationDefault", "[B"),
    ] {
        out.push((METHOD.to_string(), name, descriptor));
    }
    for (name, descriptor) in [
        ("clazz", class),
        ("slot", "I"),
        ("parameterTypes", classes),
        ("exceptionTypes", classes),
        ("modifiers", "I"),
        ("signature", string),
        ("annotations", "[B"),
        ("parameterAnnotations", "[B"),
    ] {
        out.push((CONSTRUCTOR.to_string(), name, descriptor));
    }
    for (name, descriptor) in [
        ("clazz", class),
        ("slot", "I"),
        ("name", string),
        ("type", class),
        ("modifiers", "I"),
        ("trustedFinal", "Z"),
        ("signature", string),
        ("annotations", "[B"),
    ] {
        out.push((FIELD.to_string(), name, descriptor));
    }
    out.push((
        CONSTANT_POOL.to_string(),
        "constantPoolOop",
        "Ljava/lang/Object;",
    ));
    out.push((
        "jdk/internal/reflect/FieldAccessorImpl".to_string(),
        "field",
        "Ljava/lang/reflect/Field;",
    ));
    out.push((
        "java/lang/reflect/AccessibleObject".to_string(),
        "override",
        "Z",
    ));
    for k in &KINDS[..8] {
        out.push((k.1.to_string(), "value", k.0));
    }
    out
}

/// The method `name descriptor` as declared by `class` or its nearest superclass.
fn declared_up(unit: &Unit, class: &str, name: &str, descriptor: &str) -> Option<MethodKey> {
    let chain = unit.chain(class).ok()?;
    chain.iter().find_map(|c| {
        c.methods
            .iter()
            .any(|m| m.name == name && m.descriptor == descriptor)
            .then(|| (c.name.clone(), name.to_string(), descriptor.to_string()))
    })
}

/// JABS_RJ_* methods, in enum order, resolved in the unit (None when absent).
fn runtime_calls(unit: &Unit) -> Vec<(String, Option<MethodKey>)> {
    let key = |c: &str, n: &str, d: &str| Some((c.to_string(), n.to_string(), d.to_string()));
    let mut out: Vec<(String, Option<MethodKey>)> = Vec::new();
    for k in &KINDS[..8] {
        let descriptor = format!("({})L{};", k.0, k.1);
        out.push((
            format!("{}.valueOf{descriptor}", k.1),
            key(k.1, "valueOf", &descriptor),
        ));
    }
    out.push((
        format!("{IAE}.<init>(String)"),
        key(IAE, "<init>", "(Ljava/lang/String;)V"),
    ));
    out.push((
        format!("{IAE}.<init>(Throwable)"),
        key(IAE, "<init>", "(Ljava/lang/Throwable;)V"),
    ));
    out.push((
        format!("{ITE}.<init>(Throwable)"),
        key(ITE, "<init>", "(Ljava/lang/Throwable;)V"),
    ));
    for k in &KINDS {
        let class = accessor_class(k.2);
        out.push((
            format!("{class}.<init>"),
            key(&class, "<init>", ACCESSOR_INIT),
        ));
    }
    for (name, descriptor) in [
        ("ensureObj", "(Ljava/lang/Object;)V"),
        ("isReadOnly", "()Z"),
        (
            "newGetIllegalArgumentException",
            "(Ljava/lang/Object;)Ljava/lang/IllegalArgumentException;",
        ),
        (
            "newSetIllegalArgumentException",
            "(Ljava/lang/Object;)Ljava/lang/IllegalArgumentException;",
        ),
        ("throwSetIllegalArgumentException", "(Ljava/lang/Object;)V"),
    ] {
        out.push((
            format!("{ACCESSOR_BASE}.{name}{descriptor}"),
            declared_up(unit, ACCESSOR_BASE, name, descriptor),
        ));
    }
    for k in &KINDS {
        let descriptor = format!("({})V", k.0);
        let name = "throwFinalFieldIllegalAccessException";
        out.push((
            format!("{ACCESSOR_BASE}.{name}{descriptor}"),
            declared_up(unit, ACCESSOR_BASE, name, &descriptor),
        ));
    }
    out
}

const PROXY: &str = "java/lang/reflect/Proxy";
const PROXY_BUILDER: &str = "java/lang/reflect/Proxy$ProxyBuilder";
const GET_PROXY_CONSTRUCTOR: &str =
    "(Ljava/lang/ClassLoader;[Ljava/lang/Class;)Ljava/lang/reflect/Constructor;";
/// Every annotation the JDK parses goes through this class (Class, Field, Executable and
/// Method.getDefaultValue all call into it).
const ANNOTATION_PARSER: &str = "sun/reflect/annotation/AnnotationParser";

/// Whether the program uses reflection: one of the natives that hand out members, invoke them
/// or make field accessors is reachable, or the replaced Proxy.getProxyConstructor (which
/// hands out a proxy class's Constructor).
fn reflection_used(methods: &BTreeSet<MethodKey>) -> bool {
    const TRIGGERS: [(&str, &str); 8] = [
        (PROXY, "getProxyConstructor"),
        (CLASS, "getDeclaredMethods0"),
        (CLASS, "getDeclaredFields0"),
        (CLASS, "getDeclaredConstructors0"),
        (CLASS, "getConstantPool"),
        (
            "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
            "invoke0",
        ),
        (
            "jdk/internal/reflect/DirectConstructorHandleAccessor$NativeAccessor",
            "newInstance0",
        ),
        (ACCESSOR_FACTORY, "newFieldAccessor"),
    ];
    methods
        .iter()
        .any(|m| TRIGGERS.iter().any(|t| m.0 == t.0 && m.1 == t.1))
}

/// What reflection adds to the reachable set in one round.
#[derive(Default)]
pub(super) struct Reached {
    /// Classes the runtime makes objects of.
    pub instantiated: Vec<String>,
    /// Classes loaded by name with no object made: the types annotation parsing loads.
    pub recorded: Vec<String>,
    /// Compiled methods the runtime calls.
    pub methods: Vec<MethodKey>,
}

impl Reached {
    pub fn is_empty(&self) -> bool {
        self.instantiated.is_empty() && self.recorded.is_empty() && self.methods.is_empty()
    }
}

/// What reach needs beyond the reachable set: decided once per unit.
pub(super) struct ReachInputs<'a> {
    /// The proxy classes src/proxy.rs generated.
    pub proxies: &'a [crate::proxy::Generated],
    /// For each class, the classes parsing its annotations loads (proxy::loaded_by_annotations).
    pub annotation_loads: &'a BTreeMap<String, BTreeSet<String>>,
}

/// What reflection adds to a program's reachable set, given what is reachable so far: the
/// classes the runtime instantiates (Method, the boxes, the field accessors, the exceptions it
/// constructs) and the compiled methods it calls, and the `values()` of every enum with a
/// record (EnumMap, EnumSet and Enum.valueOf find the constants by calling it reflectively).
/// With annotation parsing reachable, the types the annotations of recorded classes name get
/// records (AnnotationParser loads them by name); with proxies reachable, the generated proxy
/// of every list whose interfaces all have records is instantiated (its constructor is called
/// through Constructor.newInstance, which no instruction names).
/// Returns only what is not already reachable; the caller loops until nothing is added.
pub(super) fn reach(
    unit: &Unit,
    records: &BTreeSet<String>,
    instantiated: &BTreeSet<String>,
    methods: &BTreeSet<MethodKey>,
    compiled: &dyn Fn(&MethodKey) -> bool,
    inputs: &ReachInputs,
) -> Reached {
    let mut reached = Reached::default();
    if methods.iter().any(|m| m.0 == ANNOTATION_PARSER) {
        let mut added: BTreeSet<&str> = BTreeSet::new();
        for name in records {
            let Some(loads) = inputs.annotation_loads.get(name) else {
                continue;
            };
            for load in loads {
                if unit.get(load).is_some() && !records.contains(load) && added.insert(load) {
                    reached.recorded.push(load.clone());
                }
            }
        }
    }
    let proxy_key = (
        PROXY.to_string(),
        "getProxyConstructor".to_string(),
        GET_PROXY_CONSTRUCTOR.to_string(),
    );
    if methods.contains(&proxy_key) {
        for proxy in inputs.proxies {
            let usable = proxy.interfaces.iter().all(|i| records.contains(i))
                && unit.get(&proxy.name).is_some()
                && unit.chain(&proxy.name).is_ok();
            if !usable {
                continue;
            }
            if !instantiated.contains(&proxy.name) {
                reached.instantiated.push(proxy.name.clone());
            }
            let init = (
                proxy.name.clone(),
                "<init>".to_string(),
                crate::proxy::CONSTRUCTOR.to_string(),
            );
            if compiled(&init) && !methods.contains(&init) {
                reached.methods.push(init);
            }
        }
    }
    let classes = &mut reached.instantiated;
    let keys = &mut reached.methods;
    for name in records {
        let Some(class) = unit.get(name) else {
            continue;
        };
        if class.access & ACC_ENUM == 0 || class.super_name.as_deref() != Some("java/lang/Enum") {
            continue;
        }
        let values = (name.clone(), "values".to_string(), format!("()[L{name};"));
        if compiled(&values) && !methods.contains(&values) {
            keys.push(values);
        }
    }
    if !reflection_used(methods) {
        return reached;
    }
    for name in record_classes() {
        if unit.get(&name).is_some() && unit.chain(&name).is_ok() && !instantiated.contains(&name) {
            classes.push(name);
        }
    }
    // Constructor.newInstance can make an object of any recorded concrete class whose
    // constructor is compiled in, without a `new` anywhere: such a class is instantiated, so its
    // vtable must hold its real methods (RTA would otherwise leave stubs there).
    for name in records {
        let Some(class) = unit.get(name) else {
            continue;
        };
        if class.access & (ACC_INTERFACE | ACC_ABSTRACT) != 0 || instantiated.contains(name) {
            continue;
        }
        let constructible = class.methods.iter().any(|m| {
            m.name == "<init>"
                && methods.contains(&(name.clone(), m.name.clone(), m.descriptor.clone()))
        });
        if constructible && !classes.contains(name) {
            classes.push(name.clone());
        }
    }
    for (_, key) in runtime_calls(unit) {
        let Some(key) = key else { continue };
        if compiled(&key) && !methods.contains(&key) {
            keys.push(key);
        }
    }
    reached
}

// ---- Natives and replaced methods ----

/// Natives reflection implements: (class, name, descriptor, C body).
const NATIVES: &[(&str, &str, &str, &str)] = &[
    (
        CLASS,
        "getDeclaredMethods0",
        "(Z)[Ljava/lang/reflect/Method;",
        "return jabs_reflect_declared_methods(self, a0);",
    ),
    (
        CLASS,
        "getDeclaredFields0",
        "(Z)[Ljava/lang/reflect/Field;",
        "return jabs_reflect_declared_fields(self, a0);",
    ),
    (
        CLASS,
        "getDeclaredConstructors0",
        "(Z)[Ljava/lang/reflect/Constructor;",
        "return jabs_reflect_declared_constructors(self, a0);",
    ),
    (
        CLASS,
        "getInterfaces0",
        "()[Ljava/lang/Class;",
        "return jabs_reflect_interfaces(self);",
    ),
    (
        CLASS,
        "getDeclaredClasses0",
        "()[Ljava/lang/Class;",
        "return jabs_reflect_declared_classes(self);",
    ),
    (
        CLASS,
        "getDeclaringClass0",
        "()Ljava/lang/Class;",
        "return jabs_reflect_declaring_class(self);",
    ),
    (
        CLASS,
        "getEnclosingMethod0",
        "()[Ljava/lang/Object;",
        "return jabs_reflect_enclosing_method(self);",
    ),
    (
        CLASS,
        "getSimpleBinaryName0",
        "()Ljava/lang/String;",
        "return jabs_reflect_simple_binary_name(self);",
    ),
    (
        CLASS,
        "getGenericSignature0",
        "()Ljava/lang/String;",
        "return jabs_reflect_generic_signature(self);",
    ),
    (
        CLASS,
        "getRawAnnotations",
        "()[B",
        "return jabs_reflect_raw_annotations(self);",
    ),
    (
        CLASS,
        "isRecord0",
        "()Z",
        "return jabs_reflect_is_record(self);",
    ),
    (
        CLASS,
        "getNestHost0",
        "()Ljava/lang/Class;",
        "return jabs_reflect_nest_host(self);",
    ),
    (
        CLASS,
        "getConstantPool",
        "()Ljdk/internal/reflect/ConstantPool;",
        "return jabs_reflect_constant_pool(self);",
    ),
    (
        "jdk/internal/reflect/Reflection",
        "areNestMates",
        "(Ljava/lang/Class;Ljava/lang/Class;)Z",
        "if (!a0 || !a1) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_reflect_nest_mates(a0, a1);",
    ),
    (
        CONSTANT_POOL,
        "getSize0",
        "(Ljava/lang/Object;)I",
        "(void)self; return jabs_reflect_cp_size(a0);",
    ),
    (
        CONSTANT_POOL,
        "getIntAt0",
        "(Ljava/lang/Object;I)I",
        "(void)self; return jabs_reflect_cp_int(a0, a1);",
    ),
    (
        CONSTANT_POOL,
        "getLongAt0",
        "(Ljava/lang/Object;I)J",
        "(void)self; return jabs_reflect_cp_long(a0, a1);",
    ),
    (
        CONSTANT_POOL,
        "getFloatAt0",
        "(Ljava/lang/Object;I)F",
        "(void)self; return jabs_reflect_cp_float(a0, a1);",
    ),
    (
        CONSTANT_POOL,
        "getDoubleAt0",
        "(Ljava/lang/Object;I)D",
        "(void)self; return jabs_reflect_cp_double(a0, a1);",
    ),
    (
        CONSTANT_POOL,
        "getUTF8At0",
        "(Ljava/lang/Object;I)Ljava/lang/String;",
        "(void)self; return jabs_reflect_cp_utf8(a0, a1);",
    ),
    (
        "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
        "invoke0",
        "(Ljava/lang/reflect/Method;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        "return jabs_reflect_invoke(a0, a1, a2);",
    ),
    (
        "jdk/internal/reflect/DirectConstructorHandleAccessor$NativeAccessor",
        "newInstance0",
        "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object;",
        "return jabs_reflect_new_instance(a0, a1);",
    ),
];

/// The C body of a reflection native, if reflection implements it. All of them use the class
/// registry.
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if !classes_available {
        return None;
    }
    NATIVES
        .iter()
        .find(|n| n.0 == key.0 && n.1 == key.1 && n.2 == key.2)
        .map(|n| (n.3.to_string(), true))
}

/// Java methods reflection replaces, with why:
/// - `MethodHandleAccessorFactory.useNativeAccessor`: JABS has no method handles, so every
///   Method and Constructor uses the JDK's native accessor (the JVM's own reflection path,
///   DirectMethodHandleAccessor$NativeAccessor), whose natives call compiled code.
/// - `MethodHandleAccessorFactory.newFieldAccessor`: builds the JDK's own field accessor for
///   the field's type with no method handles; JABS replaces the accessor methods that would
///   call those handles (below), and the JDK's conversion and final-field rules stay in Java.
/// - `MethodHandle<T>FieldAccessorImpl.get<T>/set<T>` (and get/set for references): the leaf
///   accesses, which read or write the field's storage directly.
pub(super) fn replacement(key: &MethodKey) -> Option<(String, bool)> {
    // Proxy classes are generated at build time (src/proxy.rs): the JDK's lookup would spin
    // one at run time (and, before the module system is up, throws InternalError instead).
    // `Proxy.getProxyConstructor` finds the generated class of the list in a table, and
    // `ProxyBuilder.isProxyClass` asks the same table (the JDK's answer lives in a cache
    // that only its own spinning fills).
    if key.0 == PROXY && key.1 == "getProxyConstructor" && key.2 == GET_PROXY_CONSTRUCTOR {
        return Some((
            "return jabs_reflect_proxy_constructor(a0, a1);".to_string(),
            true,
        ));
    }
    if key.0 == PROXY_BUILDER && key.1 == "isProxyClass" && key.2 == "(Ljava/lang/Class;)Z" {
        return Some(("return jabs_reflect_is_proxy_class(a0);".to_string(), true));
    }
    if key.0 == ACCESSOR_FACTORY {
        return match (key.1.as_str(), key.2.as_str()) {
            ("useNativeAccessor", "(Ljava/lang/reflect/Executable;)Z") => {
                Some(("(void)a0; return 1;".to_string(), true))
            }
            (
                "newFieldAccessor",
                "(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessorImpl;",
            ) => Some((
                "return jabs_reflect_new_field_accessor(a0, a1);".to_string(),
                true,
            )),
            _ => None,
        };
    }
    let (index, kind) = KINDS
        .iter()
        .enumerate()
        .find(|(_, k)| accessor_class(k.2) == key.0)?;
    let (code, _, _, word) = *kind;
    let is_reference = index == 8;
    let get_name = format!("get{word}");
    let set_name = format!("set{word}");
    let get_descriptor = format!("(Ljava/lang/Object;){code}");
    let set_descriptor = format!("(Ljava/lang/Object;{code})V");
    let ty = storage_type(code).unwrap_or("jabs_object *");
    if key.1 == get_name && key.2 == get_descriptor {
        let value = match code {
            "Z" | "B" | "C" | "S" | "I" => "(int32_t)v",
            _ => "v",
        };
        return Some((
            format!(
                "int32_t is_volatile = 0; {ty} *p = ({ty} *)jabs_reflect_field_slot(self, a0, 0, &is_volatile); if (!p) return 0; {ty} v; if (is_volatile) __atomic_load(p, &v, __ATOMIC_SEQ_CST); else v = *p; return {value};"
            ),
            true,
        ));
    }
    if key.1 == set_name && key.2 == set_descriptor {
        if is_reference {
            return Some((
                "jabs_reflect_field_set_object(self, a0, a1);".to_string(),
                true,
            ));
        }
        let (member, stored) = match code {
            "Z" => ("i", "(uint8_t)(a1 & 1)"),
            "B" => ("i", "(int8_t)a1"),
            "C" => ("i", "(uint16_t)a1"),
            "S" => ("i", "(int16_t)a1"),
            "I" => ("i", "a1"),
            "J" => ("j", "a1"),
            "F" => ("f", "a1"),
            _ => ("d", "a1"),
        };
        return Some((
            format!(
                "jabs_value value; value.{member} = a1; if (jabs_reflect_field_refuse_set(self, a0, {index}, value)) return; int32_t is_volatile = 0; {ty} *p = ({ty} *)jabs_reflect_field_slot(self, a0, 1, &is_volatile); if (!p) return; {ty} v = {stored}; if (is_volatile) __atomic_store(p, &v, __ATOMIC_SEQ_CST); else *p = v;"
            ),
            true,
        ));
    }
    None
}

// ---- Metadata tables ----

/// The shared string, byte and list tables (see runtime/jabs_reflect.h for the encoding).
struct Tables {
    strings: Vec<u8>,
    string_at: HashMap<String, u32>,
    bytes: Vec<u8>,
    lists: Vec<u32>,
}

impl Tables {
    fn new() -> Self {
        // Offset 0 means absent, offset 1 is the empty string.
        let mut string_at = HashMap::new();
        string_at.insert(String::new(), 1);
        Tables {
            strings: vec![0, 0],
            string_at,
            bytes: vec![0],
            lists: vec![0],
        }
    }

    fn offset(len: usize, what: &str) -> u32 {
        u32::try_from(len).unwrap_or_else(|_| panic!("reflection {what} table exceeds 4 GiB"))
    }

    /// Offset of `text` (stored once). A string containing NUL cannot be a C string: callers
    /// that may see one check first.
    fn string(&mut self, text: &str) -> u32 {
        if let Some(&at) = self.string_at.get(text) {
            return at;
        }
        let at = Self::offset(self.strings.len(), "string");
        self.strings.extend_from_slice(text.as_bytes());
        self.strings.push(0);
        self.string_at.insert(text.to_string(), at);
        at
    }

    fn optional(&mut self, text: Option<&str>) -> u32 {
        text.map_or(0, |t| self.string(t))
    }

    fn blob(&mut self, data: Option<&[u8]>) -> u32 {
        let Some(data) = data else { return 0 };
        let at = Self::offset(self.bytes.len(), "byte");
        self.bytes
            .extend_from_slice(&Self::offset(data.len(), "byte").to_be_bytes());
        self.bytes.extend_from_slice(data);
        at
    }

    fn list(&mut self, items: &[&str]) -> u32 {
        if items.is_empty() {
            return 0;
        }
        let offsets: Vec<u32> = items.iter().map(|i| self.string(i)).collect();
        let at = Self::offset(self.lists.len(), "list");
        self.lists.push(Self::offset(offsets.len(), "list"));
        self.lists.extend(offsets);
        at
    }
}

/// Class.getModifiers as HotSpot computes it (InstanceKlass::compute_modifier_flags): the
/// InnerClasses flags when the class has an entry for itself, else its access flags, without
/// ACC_SUPER.
fn class_modifiers(class: &ClassFile) -> u16 {
    let own = class
        .reflect
        .inner_classes
        .iter()
        .find(|i| i.inner == class.name);
    let access = match own {
        Some(entry) => entry.access & INNER_CLASS_MODIFIERS,
        None => class.access & CLASS_MODIFIERS,
    };
    access & !ACC_SUPER
}

/// The C shape of a call: static or not, then the kinds of the return and parameters.
fn call_shape(is_static: bool, sig: &Signature) -> String {
    let kind = |c: char| match kind_of(c) {
        Some(k) => field(k),
        None => "v",
    };
    let mut shape = String::from(if is_static { "s" } else { "i" });
    shape.push_str(kind(sig.ret.code));
    for p in &sig.params {
        shape.push_str(kind(p.code));
    }
    shape
}

/// One caller function for a shape (see jabs_rcaller in runtime/jabs_reflect.h).
fn caller_function(name: &str, is_static: bool, sig: &Signature) -> String {
    let mut args: Vec<String> = Vec::new();
    if !is_static {
        args.push("self".to_string());
    }
    for (i, p) in sig.params.iter().enumerate() {
        args.push(format!("a[{i}].{}", field(kind_of(p.code).unwrap())));
    }
    let call = format!(
        "(({} (*)({}))fn)({})",
        c_type(sig.ret.code),
        c_params(is_static, sig, false),
        args.join(", ")
    );
    let store = match kind_of(sig.ret.code) {
        Some(k) => format!("r->{} = {call};", field(k)),
        None => format!("{call}; (void)r;"),
    };
    format!(
        "static void {name}(void (*fn)(void), jabs_object *self, const jabs_value *a, jabs_value *r) {{ (void)self; (void)a; {store} }}\n"
    )
}

/// Every constant-pool index the class's annotation attributes name, sorted and unique.
fn annotation_indices(class: &ClassFile) -> Vec<u16> {
    let mut out = BTreeSet::new();
    let mut add = |data: Option<&Vec<u8>>, kind: AnnotationAttribute| {
        let Some(data) = data else { return };
        // Validated when the class was parsed (ClassFile::parse), so this cannot fail.
        let indices = annotation_pool_indices(data, kind)
            .expect("annotation attribute validated by ClassFile::parse");
        out.extend(indices);
    };
    add(
        class.reflect.annotations.as_ref(),
        AnnotationAttribute::Annotations,
    );
    for m in class.methods.iter().chain(&class.fields) {
        add(
            m.reflect.annotations.as_ref(),
            AnnotationAttribute::Annotations,
        );
        add(
            m.reflect.parameter_annotations.as_ref(),
            AnnotationAttribute::ParameterAnnotations,
        );
        add(
            m.reflect.annotation_default.as_ref(),
            AnnotationAttribute::Default,
        );
    }
    out.into_iter().collect()
}

/// What the metadata needs from the rest of the compilation.
pub(super) struct MetadataInputs<'a> {
    pub recorded: &'a [&'a ClassFile],
    pub interfaces: &'a BTreeSet<&'a str>,
    pub methods: &'a BTreeSet<MethodKey>,
    pub inits: &'a BTreeSet<String>,
    /// The proxy classes src/proxy.rs generated; those compiled in get a jabs_rproxies row.
    pub proxies: &'a [crate::proxy::Generated],
}

/// The metadata tables and the program-specific support functions (runtime/jabs_reflect.h).
pub(super) fn functions(unit: &Unit, inputs: &MetadataInputs) -> String {
    let mut out = String::from("\n/* ---- Reflection metadata (src/cgen/reflect.rs) ---- */\n");
    let mut by_name: BTreeMap<&str, &ClassFile> = BTreeMap::new();
    for class in inputs.recorded {
        by_name.insert(&class.name, class);
    }
    for name in inputs.interfaces {
        if let Some(class) = unit.get(name) {
            by_name.insert(&class.name, class);
        }
    }
    let mut tables = Tables::new();
    let mut classes = Vec::new();
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    let mut inners = Vec::new();
    let mut consts = Vec::new();
    let mut shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut callers = String::new();
    let mut externs = String::new();
    for (name, class) in &by_name {
        let is_interface = class.access & ACC_INTERFACE != 0;
        let first_method = methods.len();
        let vtable = if is_interface {
            None
        } else {
            unit.vtable(name).ok()
        };
        for m in &class.methods {
            let key = (name.to_string(), m.name.clone(), m.descriptor.clone());
            let sig = parse_signature(&m.descriptor).ok();
            let is_static = m.access & ACC_STATIC != 0;
            let caller = match &sig {
                Some(sig) => {
                    let shape = call_shape(is_static, sig);
                    let next = shapes.len();
                    let index = *shapes.entry(shape).or_insert(next);
                    if index == next {
                        callers.push_str(&caller_function(
                            &format!("jabs_rc_{index}"),
                            is_static,
                            sig,
                        ));
                    }
                    u16::try_from(index).unwrap_or(u16::MAX)
                }
                None => u16::MAX,
            };
            // Dispatch as HotSpot's Reflection::invoke: private methods and constructors are
            // called directly, interface methods through the interface table, overridable
            // methods through the vtable slot their declaring class gives them.
            let fixed = is_static
                || m.access & (ACC_PRIVATE | ACC_FINAL) != 0
                || m.name == "<init>"
                || m.name == "<clinit>"
                || (!is_interface && class.access & ACC_FINAL != 0);
            let mut vtable_slot: i64 = -1;
            let mut selector_value: u64 = 0;
            if !fixed && is_interface {
                selector_value = selector(&m.name, &m.descriptor);
            } else if !fixed
                && let Some(slots) = &vtable
                && let Some(at) = slots.iter().position(|s| {
                    s.name == m.name && s.descriptor == m.descriptor && s.owner == **name
                })
            {
                vtable_slot = at as i64;
            }
            let code = if inputs.methods.contains(&key) && sig.is_some() {
                format!("(void (*)(void))&{}", symbol(name, &m.name, &m.descriptor))
            } else {
                "NULL".to_string()
            };
            let exceptions: Vec<&str> = m.reflect.exceptions.iter().map(String::as_str).collect();
            let row = format!(
                "    {{ {}, {}, {}, {}, {}, {}, {}, 0x{:04x}, {caller}, {vtable_slot}, UINT64_C(0x{selector_value:016x}), {code} }},",
                tables.string(&m.name),
                tables.string(&m.descriptor),
                tables.optional(m.reflect.signature.as_deref()),
                tables.list(&exceptions),
                tables.blob(m.reflect.annotations.as_deref()),
                tables.blob(m.reflect.parameter_annotations.as_deref()),
                tables.blob(m.reflect.annotation_default.as_deref()),
                m.access,
            );
            methods.push(row);
        }
        let first_field = fields.len();
        let statics = inputs.inits.contains(*name);
        for f in &class.fields {
            let is_static = f.access & ACC_STATIC != 0;
            let (offset, address) = if is_static {
                match storage_type(&f.descriptor) {
                    Some(ty) if statics => {
                        let sym = field_symbol(name, &f.name);
                        // Indented: local to this file (declarations_for skips indented lines).
                        let _ = writeln!(externs, " extern {ty} {sym};");
                        ("0".to_string(), format!("(void *)&{sym}"))
                    }
                    _ => ("0".to_string(), "NULL".to_string()),
                }
            } else if is_interface {
                ("0".to_string(), "NULL".to_string())
            } else {
                (
                    format!(
                        "offsetof({}, {})",
                        object_struct(name),
                        instance_field(&f.name)
                    ),
                    "NULL".to_string(),
                )
            };
            fields.push(format!(
                "    {{ {}, {}, {}, {}, 0x{:04x}, {offset}, {address} }},",
                tables.string(&f.name),
                tables.string(&f.descriptor),
                tables.optional(f.reflect.signature.as_deref()),
                tables.blob(f.reflect.annotations.as_deref()),
                f.access,
            ));
        }
        let first_inner = inners.len();
        for i in &class.reflect.inner_classes {
            inners.push(format!(
                "    {{ {}, {}, {}, 0x{:04x} }},",
                tables.string(&i.inner),
                tables.optional(i.outer.as_deref()),
                tables.optional(i.simple_name.as_deref()),
                i.access
            ));
        }
        let first_const = consts.len();
        for index in annotation_indices(class) {
            let (tag, value) = match class.pool.get(index as usize) {
                // A NUL cannot be in a C string: that constant is marked unrepresentable, and
                // the runtime stops if it is ever asked for.
                Some(Constant::Utf8(text)) if !text.contains('\0') => {
                    (1, u64::from(tables.string(text)))
                }
                Some(Constant::Integer(v)) => (3, u64::from(*v as u32)),
                Some(Constant::Float(bits)) => (4, u64::from(*bits)),
                Some(Constant::Long(v)) => (5, *v as u64),
                Some(Constant::Double(bits)) => (6, *bits),
                _ => (0, 0),
            };
            consts.push(format!(
                "    {{ {index}, {tag}, UINT64_C(0x{value:016x}) }},"
            ));
        }
        let interfaces: Vec<&str> = class.interfaces.iter().map(String::as_str).collect();
        let (enclosing_class, enclosing_name, enclosing_descriptor) =
            match &class.reflect.enclosing_method {
                Some((c, m)) => (
                    tables.string(c),
                    m.as_ref().map_or(0, |(n, _)| tables.string(n)),
                    m.as_ref().map_or(0, |(_, d)| tables.string(d)),
                ),
                None => (0, 0, 0),
            };
        let pool_size = u16::try_from(class.pool.len()).unwrap_or(u16::MAX);
        classes.push(format!(
            "    {{ {}, {first_method}, {}, {first_field}, {}, {}, {}, {}, {first_inner}, {}, {enclosing_class}, {enclosing_name}, {enclosing_descriptor}, {}, {first_const}, {}, {pool_size}, 0x{:04x}, 0x{:04x}, {} }},",
            tables.string(name),
            methods.len() - first_method,
            fields.len() - first_field,
            tables.list(&interfaces),
            tables.optional(class.reflect.signature.as_deref()),
            tables.blob(class.reflect.annotations.as_deref()),
            inners.len() - first_inner,
            tables.optional(class.reflect.nest_host.as_deref()),
            consts.len() - first_const,
            class.access,
            class_modifiers(class),
            u8::from(class.reflect.is_record),
        ));
    }
    // NestHost of every class in the unit that has one, sorted: the caller in
    // Reflection.verifyMemberAccess may be a class without a record (and so without a row).
    let mut nests: Vec<(&str, &str)> = unit
        .classes
        .values()
        .filter_map(|c| Some((c.name.as_str(), c.reflect.nest_host.as_deref()?)))
        .collect();
    nests.sort_unstable();
    let mut nest_rows = String::new();
    for (class, host) in &nests {
        let _ = writeln!(
            nest_rows,
            "    {{ {}, {} }},",
            tables.string(class),
            tables.string(host)
        );
    }
    // Proxy classes a program can make: those with metadata (a record) and a compiled
    // constructor. Nothing else can be made, so nothing else is listed.
    let mut proxy_rows = String::new();
    let mut proxy_count = 0usize;
    for proxy in inputs.proxies {
        let init = (
            proxy.name.clone(),
            "<init>".to_string(),
            crate::proxy::CONSTRUCTOR.to_string(),
        );
        if !by_name.contains_key(proxy.name.as_str()) || !inputs.methods.contains(&init) {
            continue;
        }
        let interfaces: Vec<&str> = proxy.interfaces.iter().map(String::as_str).collect();
        let _ = writeln!(
            proxy_rows,
            "    {{ {}, {} }},",
            tables.string(&proxy.name),
            tables.list(&interfaces)
        );
        proxy_count += 1;
    }
    out.push_str(&externs);
    out.push_str(&callers);
    // Every array keeps a sentinel element: C has no empty arrays.
    out.push_str("const char jabs_rstrings[] =\n");
    for piece in tables.strings.split(|b| *b == 0) {
        let _ = writeln!(
            out,
            "    \"{}\\0\"",
            c_escape(&String::from_utf8_lossy(piece))
        );
    }
    out.push_str("    ;\nconst unsigned char jabs_rbytes[] = {\n");
    for line in tables.bytes.chunks(32) {
        let bytes: Vec<String> = line.iter().map(|b| format!("0x{b:02x}")).collect();
        let _ = writeln!(out, "    {},", bytes.join(", "));
    }
    out.push_str("};\nconst uint32_t jabs_rlists[] = {\n");
    for chunk in tables.lists.chunks(16) {
        let items: Vec<String> = chunk.iter().map(u32::to_string).collect();
        let _ = writeln!(out, "    {},", items.join(", "));
    }
    let _ = write!(
        out,
        "}};\nconst jabs_rclass jabs_rclasses[] = {{\n{}\n    {{ 0 }}\n}};\nconst uint32_t jabs_rclass_count = {};\n",
        classes.join("\n"),
        classes.len()
    );
    let _ = write!(
        out,
        "const jabs_rmethod jabs_rmethods[] = {{\n{}\n    {{ 0 }}\n}};\n",
        methods.join("\n")
    );
    let _ = write!(
        out,
        "const jabs_rfield jabs_rfields[] = {{\n{}\n    {{ 0 }}\n}};\n",
        fields.join("\n")
    );
    let _ = write!(
        out,
        "const jabs_rinner jabs_rinners[] = {{\n{}\n    {{ 0 }}\n}};\n",
        inners.join("\n")
    );
    let _ = write!(
        out,
        "const jabs_rconst jabs_rconsts[] = {{\n{}\n    {{ 0 }}\n}};\n",
        consts.join("\n")
    );
    let _ = write!(
        out,
        "const jabs_rnest jabs_rnests[] = {{\n{nest_rows}    {{ 0, 0 }}\n}};\nconst uint32_t jabs_rnest_count = {};\n",
        nests.len()
    );
    // The constructor cache is the only table written at run time: each slot goes from NULL to
    // one Constructor, once, by compare-and-swap (jabs_reflect_proxy_constructor).
    let _ = write!(
        out,
        "const jabs_rproxy jabs_rproxies[] = {{\n{proxy_rows}    {{ 0, 0 }}\n}};\nconst uint32_t jabs_rproxy_count = {proxy_count};\njabs_object *jabs_rproxy_constructors[{}];\n",
        proxy_count + 1
    );
    let names: Vec<String> = (0..shapes.len()).map(|i| format!("jabs_rc_{i}")).collect();
    let _ = write!(
        out,
        "const jabs_rcaller jabs_rcallers[] = {{ {}NULL }};\nconst uint32_t jabs_rcaller_count = {};\n",
        names.iter().map(|n| format!("{n}, ")).collect::<String>(),
        names.len()
    );
    out.push_str(&support(unit, inputs));
    out
}

/// The program-specific functions runtime/jabs_reflect.c calls: records, layouts, calls.
fn support(unit: &Unit, inputs: &MetadataInputs) -> String {
    let recorded = |name: &str| inputs.recorded.iter().any(|c| c.name == name);
    let mut out = String::new();
    // Records and allocation.
    let mut records = String::new();
    let mut news = String::new();
    for (i, name) in record_classes().iter().enumerate() {
        if !recorded(name) {
            let _ = writeln!(records, "    case {i}: return NULL; /* {name} */");
            let _ = writeln!(
                news,
                "    case {i}: jabs_not_compiled(\"{} (reflection allocates it) has no record in this program\");",
                c_escape(name)
            );
            continue;
        }
        let _ = writeln!(records, "    case {i}: return &{};", class_record(name));
        let init = if inputs.inits.contains(name) {
            format!("if ({}() != 0) return NULL; ", init_symbol(name))
        } else {
            String::new()
        };
        let _ = writeln!(
            news,
            "    case {i}: {init}return jabs_new(&{});",
            class_record(name)
        );
    }
    let _ = write!(
        out,
        // Indented so declarations_for does not mistake it for a record (jabs_reflect.h declares it).
        " const jabs_class *jabs_rrecord(int which) {{\n    switch (which) {{\n{records}    default: return NULL;\n    }}\n}}\n\
jabs_object *jabs_rnew(int which) {{\n    switch (which) {{\n{news}    default: break;\n    }}\n    jabs_not_compiled(\"jabs_rnew: unknown reflection class\");\n}}\n"
    );
    // Layouts.
    let mut layouts = String::new();
    for (i, (class, name, descriptor)) in layout_fields().iter().enumerate() {
        let declared = unit.get(class).is_some_and(|c| {
            c.access & ACC_INTERFACE == 0
                && unit.chain(class).is_ok()
                && c.fields.iter().any(|f| {
                    f.name == *name && f.descriptor == *descriptor && f.access & ACC_STATIC == 0
                })
        });
        if declared {
            let _ = writeln!(
                layouts,
                "    case {i}: return &(({} *)o)->{};",
                object_struct(class),
                instance_field(name)
            );
        } else {
            let _ = writeln!(
                layouts,
                "    case {i}: jabs_not_compiled(\"{}.{name} ({descriptor}) is not in this JDK's layout (reflection uses it)\");",
                c_escape(class)
            );
        }
    }
    let _ = write!(
        out,
        "void *jabs_rf(jabs_object *o, int which) {{\n    (void)o;\n    switch (which) {{\n{layouts}    default: break;\n    }}\n    jabs_not_compiled(\"jabs_rf: unknown reflection field\");\n}}\n"
    );
    // Calls into compiled Java.
    let mut calls = String::new();
    for (i, (what, key)) in runtime_calls(unit).iter().enumerate() {
        let emitted = key.as_ref().filter(|k| inputs.methods.contains(*k));
        let Some(key) = emitted else {
            let _ = writeln!(
                calls,
                "    case {i}: jabs_not_compiled(\"{} (reflection calls it) is not compiled into this program\");",
                c_escape(what)
            );
            continue;
        };
        let Ok(sig) = parse_signature(&key.2) else {
            continue;
        };
        let is_static = unit
            .get(&key.0)
            .and_then(|c| {
                c.methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
            })
            .is_some_and(|m| m.access & ACC_STATIC != 0);
        let mut args: Vec<String> = Vec::new();
        if !is_static {
            args.push("self".to_string());
        }
        for (n, p) in sig.params.iter().enumerate() {
            args.push(format!("args[{n}].{}", field(kind_of(p.code).unwrap())));
        }
        let call = format!("{}({})", symbol(&key.0, &key.1, &key.2), args.join(", "));
        let store = match kind_of(sig.ret.code) {
            Some(k) => format!("result->{} = {call};", field(k)),
            None => format!("{call};"),
        };
        let _ = writeln!(calls, "    case {i}: {store} break;");
    }
    let _ = write!(
        out,
        "int jabs_rcall(int which, jabs_object *self, const jabs_value *args, jabs_value *result) {{\n    (void)self; (void)args; (void)result;\n    switch (which) {{\n{calls}    default: jabs_not_compiled(\"jabs_rcall: unknown reflection call\");\n    }}\n    return jabs_pending ? -1 : 0;\n}}\n\
const char *jabs_reflect_class_name(jabs_object *c) {{ jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL; return e ? e->name : NULL; }}\n\
\x20const jabs_class *jabs_reflect_class_record(jabs_object *c) {{ jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL; return e ? e->record : NULL; }}\n"
    );
    out
}
