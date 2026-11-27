//! Differential tests for `java.lang.invoke.MethodHandleNatives`, `MemberName`, and
//! `MethodHandles.lookup()` behavior against JDK 25.
//!
//! - `tests/fixtures/MethodHandleCases.java` tests public `MethodHandles.lookup()` operations
//!   (`findStatic`, `findVirtual`, `findConstructor`, `findSpecial`, `findGetter`/`Setter`/
//!   `StaticGetter`/`StaticSetter`, `unreflect`, and lookup failure modes) on `--release 8` with
//!   `jdk_modules`.
//! - A second generated fixture (`MethodHandleInternalCases`) tests `MethodHandleNatives` natives
//!   directly (`verifyConstants`, `init`, `expand`, `resolve` found/speculative/errors,
//!   `objectFieldOffset`, `staticFieldOffset`, `staticFieldBase`, and `getMemberVMInfo`) against
//!   JDK 25 with `--add-opens`.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip locally and fail when
//! `JABS_REQUIRE_TOOLS=1`.

mod common;

use common::{Built, built_once, clang_target_args, outcome, run, same_as_jvm};
use jabs::{cgen, classfile::ClassFile};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const FIXTURE: &str = "tests/fixtures/MethodHandleCases.java";
const LOOK: &str = "src/cgen/method_handles.rs";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    built_once(&BUILT, FIXTURE, "MethodHandleCases")
}

#[test]
fn lookup_find_static() {
    same_as_jvm(built(), "find_static", LOOK);
}

#[test]
fn lookup_find_virtual() {
    same_as_jvm(built(), "find_virtual", LOOK);
}

#[test]
fn lookup_find_constructor_and_special() {
    same_as_jvm(built(), "find_constructor_and_special", LOOK);
}

#[test]
fn lookup_find_fields() {
    same_as_jvm(built(), "find_fields", LOOK);
}

#[test]
fn lookup_unreflect() {
    same_as_jvm(built(), "unreflect", LOOK);
}

#[test]
fn lookup_missing_and_access_errors() {
    same_as_jvm(built(), "missing_and_access_errors", LOOK);
}

// ---- Direct MethodHandleNatives & MemberName tests ----

const INTERNAL_FIXTURE: &str = r#"import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

class MethodHandleNonNestPeer {
    private static void secretStatic() {}
}

class MethodHandleInternalCases extends mhp.MhPubSub {
    interface Face {
        void absOp();
        default int defOp() { return 9; }
        static int statOp() { return 10; }
        private int privIfaceOp() { return 42; }
    }

    static class Base {
        public int baseVal = 101;
        public static int baseStat = 202;
        public void virt() {}
        public final void fin() {}
        public static void stat() {}
    }

    static class Derived extends Base implements Face {
        private int privVal = 303;
        public volatile long volVal = 404L;
        public static int derStat = 505;
        public Derived() {}
        public void absOp() {}
        public static void varargsOp(String... args) {}
    }

    record Rec(int x) {}

    static Class<?> mhnCls;
    static Class<?> mnCls;
    static Class<?> rmnCls;
    static Class<?> unsafeCls;
    static Object unsafe;

    static Method mInit;
    static Method mExpand;
    static Method mResolve;
    static Method mObjOff;
    static Method mStatOff;
    static Method mStatBase;
    static Method mVmInfo;
    static Method mVerifyConstants;
    static Method mGetNamedCon;

    static Constructor<?> mnCtorMethod;
    static Constructor<?> mnCtorCtor;
    static Constructor<?> mnCtorLookupM;
    static Constructor<?> mnCtorLookupF;

    static Field fClazz;
    static Field fName;
    static Field fType;
    static Field fFlags;
    static Field fMethod;
    static Field fRes;

    static void setup() throws Exception {
        mhnCls = Class.forName("java.lang.invoke.MethodHandleNatives");
        mnCls = Class.forName("java.lang.invoke.MemberName");
        rmnCls = Class.forName("java.lang.invoke.ResolvedMethodName");
        unsafeCls = Class.forName("jdk.internal.misc.Unsafe");

        Method mGetUnsafe = unsafeCls.getDeclaredMethod("getUnsafe");
        mGetUnsafe.setAccessible(true);
        unsafe = mGetUnsafe.invoke(null);

        mInit = mhnCls.getDeclaredMethod("init", mnCls, Object.class);
        mInit.setAccessible(true);
        mExpand = mhnCls.getDeclaredMethod("expand", mnCls);
        mExpand.setAccessible(true);
        mResolve = mhnCls.getDeclaredMethod("resolve", mnCls, Class.class, int.class, boolean.class);
        mResolve.setAccessible(true);
        mObjOff = mhnCls.getDeclaredMethod("objectFieldOffset", mnCls);
        mObjOff.setAccessible(true);
        mStatOff = mhnCls.getDeclaredMethod("staticFieldOffset", mnCls);
        mStatOff.setAccessible(true);
        mStatBase = mhnCls.getDeclaredMethod("staticFieldBase", mnCls);
        mStatBase.setAccessible(true);
        mVmInfo = mhnCls.getDeclaredMethod("getMemberVMInfo", mnCls);
        mVmInfo.setAccessible(true);
        mVerifyConstants = mhnCls.getDeclaredMethod("verifyConstants");
        mVerifyConstants.setAccessible(true);
        mGetNamedCon = mhnCls.getDeclaredMethod("getNamedCon", int.class, Object[].class);
        mGetNamedCon.setAccessible(true);

        mnCtorMethod = mnCls.getDeclaredConstructor(Method.class);
        mnCtorMethod.setAccessible(true);
        mnCtorCtor = mnCls.getDeclaredConstructor(Constructor.class);
        mnCtorCtor.setAccessible(true);
        mnCtorLookupM = mnCls.getDeclaredConstructor(Class.class, String.class, MethodType.class, byte.class);
        mnCtorLookupM.setAccessible(true);
        mnCtorLookupF = mnCls.getDeclaredConstructor(Class.class, String.class, Class.class, byte.class);
        mnCtorLookupF.setAccessible(true);

        fClazz = mnCls.getDeclaredField("clazz"); fClazz.setAccessible(true);
        fName = mnCls.getDeclaredField("name"); fName.setAccessible(true);
        fType = mnCls.getDeclaredField("type"); fType.setAccessible(true);
        fFlags = mnCls.getDeclaredField("flags"); fFlags.setAccessible(true);
        fMethod = mnCls.getDeclaredField("method"); fMethod.setAccessible(true);
        fRes = mnCls.getDeclaredField("resolution"); fRes.setAccessible(true);
    }

    public static void main(String[] args) throws Throwable {
        setup();
        switch (args[0]) {
            case "verify_constants":
                verifyConstants();
                break;
            case "init_and_expand":
                initAndExpand();
                break;
            case "resolve_found":
                resolveFound();
                break;
            case "resolve_errors_and_speculative":
                resolveErrorsAndSpeculative();
                break;
            case "field_offsets_and_bases":
                fieldOffsetsAndBases();
                break;
            case "member_vm_info":
                memberVmInfo();
                break;
            default:
                throw new IllegalArgumentException("unknown mechanism: " + args[0]);
        }
    }

    static void verifyConstants() throws Throwable {
        System.out.println("verifyConstants=" + mVerifyConstants.invoke(null));
        // Out-of-bounds index always returns 0 and leaves box[0] null.
        Object[] box = new Object[] { "sentinel" };
        int out = (Integer) mGetNamedCon.invoke(null, 9999, box);
        System.out.println("oob=" + out + ":" + box[0]);
    }

    static Object initFieldMn(Field f) throws Throwable {
        Object mn = mnCtorLookupF.newInstance(f.getDeclaringClass(), f.getName(), f.getType(), (byte) 1);
        fClazz.set(mn, null);
        fRes.set(mn, null);
        mInit.invoke(null, mn, f);
        return mn;
    }

    static String hex8(int v) {
        String s = Integer.toHexString(v);
        while (s.length() < 8) {
            s = "0" + s;
        }
        return s;
    }

    static void printMn(String label, Object mn) throws Exception {
        int fl = fFlags.getInt(mn);
        System.out.println(
            label
                + " clazz=" + ((Class<?>) fClazz.get(mn)).getName()
                + " refKind=" + ((fl >>> 24) & 0xF)
                + " flags=0x" + hex8(fl)
                + " hasMethod=" + (fMethod.get(mn) != null)
                + " resolved=" + (fRes.get(mn) == null)
        );
    }

    static void initAndExpand() throws Throwable {
        Object mnVirt = mnCtorMethod.newInstance(Base.class.getDeclaredMethod("virt"));
        Object mnFin = mnCtorMethod.newInstance(Base.class.getDeclaredMethod("fin"));
        Object mnStat = mnCtorMethod.newInstance(Base.class.getDeclaredMethod("stat"));
        Object mnIface = mnCtorMethod.newInstance(Face.class.getDeclaredMethod("absOp"));
        Object mnDef = mnCtorMethod.newInstance(Face.class.getDeclaredMethod("defOp"));
        Object mnNative = mnCtorMethod.newInstance(System.class.getDeclaredMethod("nanoTime"));
        Object mnVarargs = mnCtorMethod.newInstance(Derived.class.getDeclaredMethod("varargsOp", String[].class));
        Object mnCS = mnCtorMethod.newInstance(MethodHandles.class.getDeclaredMethod("lookup"));
        Object mnCtor = mnCtorCtor.newInstance(Derived.class.getDeclaredConstructor());
        Object mnPrivF = initFieldMn(Derived.class.getDeclaredField("privVal"));
        Object mnRecF = initFieldMn(Rec.class.getDeclaredField("x"));
        Object mnStatF = initFieldMn(Derived.class.getDeclaredField("derStat"));

        printMn("init.virt", mnVirt);
        printMn("init.fin", mnFin);
        printMn("init.stat", mnStat);
        printMn("init.iface", mnIface);
        printMn("init.def", mnDef);
        printMn("init.native", mnNative);
        printMn("init.varargs", mnVarargs);
        printMn("init.CS", mnCS);
        printMn("init.ctor", mnCtor);
        printMn("init.privF", mnPrivF);
        printMn("init.recF", mnRecF);
        printMn("init.statF", mnStatF);

        // MethodHandle.invokeExact leaves clazz == null in init
        Object mnMh = mnCtorLookupM.newInstance(MethodHandle.class, "invokeExact", MethodType.methodType(Object.class, Object[].class), (byte) 5);
        fClazz.set(mnMh, null);
        mInit.invoke(null, mnMh, MethodHandle.class.getDeclaredMethod("invokeExact", Object[].class));
        System.out.println("init.mhInvokeExact.clazz=" + fClazz.get(mnMh));

        // expand restores clazz, name, and type descriptor on resolved methods & constructors
        fClazz.set(mnVarargs, null);
        fName.set(mnVarargs, null);
        fType.set(mnVarargs, null);
        mExpand.invoke(null, mnVarargs);
        System.out.println("expand.varargs=" + ((Class<?>) fClazz.get(mnVarargs)).getName()
            + "." + fName.get(mnVarargs) + ":" + fType.get(mnVarargs));

        fClazz.set(mnCtor, null);
        fName.set(mnCtor, null);
        fType.set(mnCtor, null);
        mExpand.invoke(null, mnCtor);
        System.out.println("expand.ctor=" + ((Class<?>) fClazz.get(mnCtor)).getName()
            + "." + fName.get(mnCtor) + ":" + fType.get(mnCtor));
    }

    static void resolveFound() throws Throwable {
        Class<?> caller = MethodHandleInternalCases.class;
        Object r1 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "virt", MethodType.methodType(void.class), (byte) 5), caller, -1, false);
        Object r2 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "fin", MethodType.methodType(void.class), (byte) 5), caller, -1, false);
        Object r3 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "stat", MethodType.methodType(void.class), (byte) 6), caller, -1, false);
        Object r4 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "defOp", MethodType.methodType(int.class), (byte) 5), caller, -1, false);
        Object r5 = mResolve.invoke(null, mnCtorLookupM.newInstance(Face.class, "toString", MethodType.methodType(String.class), (byte) 9), caller, -1, false);
        Object r6 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "<init>", MethodType.methodType(void.class), (byte) 8), caller, -1, false);
        Object r7 = mResolve.invoke(null, mnCtorLookupF.newInstance(Derived.class, "baseVal", int.class, (byte) 1), caller, -1, false);
        Object r8 = mResolve.invoke(null, mnCtorLookupF.newInstance(Derived.class, "baseStat", int.class, (byte) 2), caller, -1, false);
        Object r9 = mResolve.invoke(null, mnCtorLookupM.newInstance(MethodHandles.class, "lookup", MethodType.methodType(MethodHandles.Lookup.class), (byte) 6), caller, -1, false);
        Object r10 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "varargsOp", MethodType.methodType(void.class, String[].class), (byte) 6), caller, -1, false);
        Object r11 = mResolve.invoke(null, mnCtorLookupM.newInstance(int[].class, "toString", MethodType.methodType(String.class), (byte) 5), caller, -1, false);

        Object r12 = mResolve.invoke(null, mnCtorLookupF.newInstance(Derived.class, "derStat", int.class, (byte) 1), caller, -1, false);
        Object r13 = mResolve.invoke(null, mnCtorLookupF.newInstance(Derived.class, "privVal", int.class, (byte) 2), caller, -1, false);
        Object r14 = mResolve.invoke(null, mnCtorLookupM.newInstance(Face.class, "privIfaceOp", MethodType.methodType(int.class), (byte) 7), Face.class, -1, false);
        Object r15 = mResolve.invoke(null, mnCtorLookupM.newInstance(Derived.class, "toString", MethodType.methodType(String.class), (byte) 5), caller, -1, false);
        Object r16 = mResolve.invoke(null, mnCtorLookupM.newInstance(String.class, "isLatin1", MethodType.methodType(boolean.class), (byte) 5), null, 0, false);

        printMn("res.virt", r1);
        printMn("res.fin", r2);
        printMn("res.stat", r3);
        printMn("res.defOp", r4);
        printMn("res.ifaceToString", r5);
        printMn("res.ctor", r6);
        printMn("res.baseVal", r7);
        printMn("res.baseStat", r8);
        printMn("res.lookup", r9);
        printMn("res.varargs", r10);
        printMn("res.arrayToString", r11);
        printMn("res.getFieldOnStatic", r12);
        printMn("res.getStaticOnInst", r13);
        printMn("res.privIfaceOp", r14);
        printMn("res.derivedToString", r15);
        printMn("res.callerNullLm0", r16);

        MethodHandles.Lookup full = MethodHandles.lookup();
        System.out.println("crossPkg.findSpecial=" + (full.findSpecial(mhp.MhPubSub.class, "protOp", MethodType.methodType(int.class), caller) != null));
        System.out.println("crossPkg.pkgBasePub=" + (full.findVirtual(mhp.MhPubSub.class, "pkgBasePub", MethodType.methodType(int.class)) != null));
    }

    static void checkResolveCase(String label, Object mn, Class<?> caller) throws Throwable {
        try {
            Object r = mResolve.invoke(null, mn, caller, -1, false);
            System.out.println(label + ".strict=UNEXPECTED:" + r);
        } catch (InvocationTargetException e) {
            System.out.println(label + ".strict=" + e.getCause().getClass().getName());
        }
        try {
            Object spec = mResolve.invoke(null, mn, caller, -1, true);
            System.out.println(label + ".spec=" + spec);
        } catch (InvocationTargetException e) {
            System.out.println(label + ".spec=" + e.getCause().getClass().getName());
        }
    }

    static void resolveErrorsAndSpeculative() throws Throwable {
        Class<?> caller = MethodHandleInternalCases.class;
        checkResolveCase("missingMethod",
            mnCtorLookupM.newInstance(Derived.class, "nope", MethodType.methodType(void.class), (byte) 5), caller);
        checkResolveCase("missingField",
            mnCtorLookupF.newInstance(Derived.class, "nope", int.class, (byte) 1), caller);
        checkResolveCase("ifaceAsVirtual",
            mnCtorLookupM.newInstance(Face.class, "absOp", MethodType.methodType(void.class), (byte) 5), caller);
        checkResolveCase("classAsIface",
            mnCtorLookupM.newInstance(Derived.class, "virt", MethodType.methodType(void.class), (byte) 9), caller);
        checkResolveCase("staticMismatchM",
            mnCtorLookupM.newInstance(Derived.class, "stat", MethodType.methodType(void.class), (byte) 5), caller);
        checkResolveCase("privatePeer",
            mnCtorLookupM.newInstance(MethodHandleNonNestPeer.class, "secretStatic", MethodType.methodType(void.class), (byte) 6), caller);
        checkResolveCase("mhInvokeBasicAsIface",
            mnCtorLookupM.newInstance(MethodHandle.class, "invokeBasic", MethodType.methodType(Object.class, Object[].class), (byte) 9), caller);
        checkResolveCase("mhExactAsStatic",
            mnCtorLookupM.newInstance(MethodHandle.class, "invokeExact", MethodType.methodType(void.class), (byte) 6), caller);
        checkResolveCase("mhInvokeBasicExternal",
            mnCtorLookupM.newInstance(MethodHandle.class, "invokeBasic", MethodType.methodType(Object.class, Object[].class), (byte) 5), caller);
        Object nullNameMn = mnCtorLookupM.newInstance(Derived.class, "virt", MethodType.methodType(void.class), (byte) 5);
        fName.set(nullNameMn, null);
        checkResolveCase("mnNameNull", nullNameMn, caller);

        MethodHandles.Lookup full = MethodHandles.lookup();
        MethodHandles.Lookup noPriv = full.dropLookupMode(MethodHandles.Lookup.PRIVATE);
        try {
            noPriv.findVirtual(Face.class, "privIfaceOp", MethodType.methodType(int.class));
            System.out.println("dropPrivIface=UNEXPECTED");
        } catch (IllegalAccessException e) {
            System.out.println("dropPrivIface=IllegalAccessException");
        }
        MethodHandles.Lookup noPkg = full.dropLookupMode(MethodHandles.Lookup.PACKAGE);
        try {
            noPkg.findStatic(MethodHandleNonNestPeer.class, "secretStatic", MethodType.methodType(void.class));
            System.out.println("dropPkgPeer=UNEXPECTED");
        } catch (IllegalAccessException e) {
            System.out.println("dropPkgPeer=IllegalAccessException");
        }
        try {
            noPkg.findVirtual(Derived.class, "virt", MethodType.methodType(void.class));
            System.out.println("dropPkgPkgClass=UNEXPECTED");
        } catch (IllegalAccessException e) {
            System.out.println("dropPkgPkgClass=IllegalAccessException");
        }
    }

    static void fieldOffsetsAndBases() throws Throwable {
        Field fPriv = Derived.class.getDeclaredField("privVal");
        Field fVol = Derived.class.getDeclaredField("volVal");
        Field fStat = Derived.class.getDeclaredField("derStat");

        Method mUnsafeObjOff = unsafeCls.getDeclaredMethod("objectFieldOffset", Field.class);
        Method mUnsafeGetInt = unsafeCls.getDeclaredMethod("getInt", Object.class, long.class);
        Method mUnsafePutInt = unsafeCls.getDeclaredMethod("putInt", Object.class, long.class, int.class);
        Method mUnsafeGetLong = unsafeCls.getDeclaredMethod("getLong", Object.class, long.class);

        Object mnPriv = initFieldMn(fPriv);
        Object mnVol = initFieldMn(fVol);
        Object mnStat = initFieldMn(fStat);

        long offPriv = (Long) mObjOff.invoke(null, mnPriv);
        long offVol = (Long) mObjOff.invoke(null, mnVol);
        long unsafeOffPriv = (Long) mUnsafeObjOff.invoke(unsafe, fPriv);
        long unsafeOffVol = (Long) mUnsafeObjOff.invoke(unsafe, fVol);

        Derived d = new Derived();
        System.out.println("objOffMatchPriv=" + (offPriv == unsafeOffPriv && offPriv > 0));
        System.out.println("objOffMatchVol=" + (offVol == unsafeOffVol && offVol > 0));
        System.out.println("readPriv=" + mUnsafeGetInt.invoke(unsafe, d, offPriv));
        mUnsafePutInt.invoke(unsafe, d, offPriv, 777);
        System.out.println("afterPutPriv=" + d.privVal);
        System.out.println("readVol=" + mUnsafeGetLong.invoke(unsafe, d, offVol));

        long statOff = (Long) mStatOff.invoke(null, mnStat);
        Object statBase = mStatBase.invoke(null, mnStat);
        System.out.println("readStat=" + mUnsafeGetInt.invoke(unsafe, statBase, statOff));
        mUnsafePutInt.invoke(unsafe, statBase, statOff, 888);
        System.out.println("afterPutStat=" + Derived.derStat);

        try {
            mObjOff.invoke(null, mnStat);
            System.out.println("objOffOnStatic=UNEXPECTED");
        } catch (InvocationTargetException e) {
            System.out.println("objOffOnStatic=" + e.getCause().getClass().getName());
        }
        try {
            mStatOff.invoke(null, mnPriv);
            System.out.println("statOffOnInst=UNEXPECTED");
        } catch (InvocationTargetException e) {
            System.out.println("statOffOnInst=" + e.getCause().getClass().getName());
        }
    }

    static void printVmInfo(String label, Object mn, boolean expectNonNegative, boolean expectClass) throws Throwable {
        Object[] info = (Object[]) mVmInfo.invoke(null, mn);
        long idx = (Long) info[0];
        Object target = info[1];
        boolean idxOk = expectNonNegative ? idx >= 0 : idx < 0;
        boolean targetOk = expectClass ? (target instanceof Class) : mnCls.isInstance(target);
        System.out.println(label + " len=" + info.length + " idxOk=" + idxOk + " targetOk=" + targetOk);
    }

    static void memberVmInfo() throws Throwable {
        printVmInfo("vm.virt", mnCtorMethod.newInstance(Base.class.getDeclaredMethod("virt")), true, false);
        printVmInfo("vm.iface", mnCtorMethod.newInstance(Face.class.getDeclaredMethod("absOp")), true, false);
        printVmInfo("vm.fin", mnCtorMethod.newInstance(Base.class.getDeclaredMethod("fin")), false, false);
        printVmInfo("vm.stat", mnCtorMethod.newInstance(Base.class.getDeclaredMethod("stat")), false, false);
        printVmInfo("vm.ctor", mnCtorCtor.newInstance(Derived.class.getDeclaredConstructor()), false, false);
        printVmInfo("vm.instF", initFieldMn(Derived.class.getDeclaredField("privVal")), true, true);
        printVmInfo("vm.statF", initFieldMn(Derived.class.getDeclaredField("derStat")), true, true);
        Constructor<?> mnCtorType = mnCls.getDeclaredConstructor(Class.class);
        mnCtorType.setAccessible(true);
        Object mnType = mnCtorType.newInstance(String.class);
        Object[] typeInfo = (Object[]) mVmInfo.invoke(null, mnType);
        System.out.println("vm.type len=" + typeInfo.length + " idx=" + typeInfo[0] + " same=" + (typeInfo[1] == mnType));
    }
}
"#;

fn build_internal() -> Option<Built> {
    if !common::tools_ready("MethodHandleInternalCases") {
        return None;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-MethodHandleInternalCases"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let pkg_dir = dir.join("mhp");
    fs::create_dir_all(&pkg_dir).unwrap();
    let pkg_source = pkg_dir.join("MhPubSub.java");
    fs::write(
        &pkg_source,
        r#"package mhp;
class MhPkgBase {
    public int pkgBasePub() { return 7; }
}
public class MhPubSub extends MhPkgBase {
    protected int protOp() { return 11; }
}
"#,
    )
    .unwrap();
    let source = dir.join("MethodHandleInternalCases.java");
    fs::write(&source, INTERNAL_FIXTURE).unwrap();

    run(Command::new("javac")
        .args(["--release", "25", "-nowarn", "-d"])
        .arg(&classes)
        .arg(&pkg_source)
        .arg(&source));

    let mut units: Vec<ClassFile> = Vec::new();
    let mut stack = vec![classes.clone()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "class") {
                units.push(ClassFile::parse(&fs::read(p).unwrap()).unwrap());
            }
        }
    }
    let roots: BTreeSet<String> = units.iter().map(|c| c.name.clone()).collect();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, unparsable) = jabs::jdk::read_module(&home, "java.base").unwrap();
    assert_eq!(unparsable, 0, "java.base classes ClassFile::parse rejects");
    units.extend(library);

    let extras = cgen::UnitExtras {
        startup: true,
        java_home: Some(home.display().to_string()),
        ..Default::default()
    };
    let compiled = cgen::compile_unit_with(&units, Some(&roots), &extras);
    let unsupported: Vec<_> = compiled
        .methods
        .iter()
        .filter(|m| {
            roots.contains(&m.class) && matches!(m.status, cgen::MethodStatus::Unsupported { .. })
        })
        .collect();
    assert!(
        unsupported.is_empty(),
        "MethodHandleInternalCases methods JABS could not compile: {unsupported:#?}"
    );

    let mut sources = Vec::new();
    for (name, contents) in &compiled.c_files {
        fs::write(dir.join(name), contents).unwrap();
        if name.ends_with(".c") {
            sources.push(dir.join(name));
        }
    }
    let record = cgen::class_record("java/lang/String");
    let main_c = dir.join("harness.c");
    fs::write(
        &main_c,
        format!(
            r#"#include "jabs_unit.h"
extern const jabs_class {record};
int main(int argc, char **argv) {{
    if (jabs_vm_startup() != 0) {{ fprintf(stderr, "startup failed: %s\n", jabs_pending->cls->name); return 2; }}
    jabs_object *args = jabs_new_array(argc - 1, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{record}, "[Ljava/lang/String;", NULL);
    if (!args) {{ fprintf(stderr, "out of memory\n"); return 2; }}
    for (int i = 1; i < argc; i++) {{
        jabs_object *s = jabs_string_from_utf8(argv[i]);
        if (!s) {{ fprintf(stderr, "out of memory\n"); return 2; }}
        ((jabs_object **)jabs_array_data((jabs_array *)args))[i - 1] = s;
    }}
    if ({init}() == 0) {main}(args);
    fflush(stdout);
    if (jabs_pending) {{ fprintf(stderr, "uncaught %s\n", jabs_pending->cls->name); return 1; }}
    return 0;
}}
"#,
            init = cgen::init_symbol("MethodHandleInternalCases"),
            main = cgen::symbol("MethodHandleInternalCases", "main", "([Ljava/lang/String;)V"),
        ),
    )
    .unwrap();
    sources.push(main_c);

    let target = clang_target_args(&dir);
    let flags = [
        "-std=c11",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wno-unused-function",
        "-Wno-unused-label",
        "-D_CRT_SECURE_NO_WARNINGS",
    ];
    let objects: Vec<PathBuf> = std::thread::scope(|scope| {
        let jobs: Vec<_> = sources
            .iter()
            .map(|src| {
                let obj = src.with_extension("o");
                let target = &target;
                scope.spawn(move || {
                    run(Command::new("clang")
                        .args(target)
                        .args(flags)
                        .arg("-c")
                        .arg(src)
                        .arg("-o")
                        .arg(&obj));
                    obj
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    let native = dir.join(if cfg!(windows) {
        "fixture.exe"
    } else {
        "fixture"
    });
    run(Command::new("clang")
        .args(&target)
        .args(&objects)
        .arg("-o")
        .arg(&native)
        .args(if cfg!(windows) {
            &["-lws2_32", "-liphlpapi"][..]
        } else {
            &["-lm"][..]
        }));
    Some(Built {
        classes,
        native,
        main_class: "MethodHandleInternalCases",
        fixture: "MethodHandleInternalCases (internal)",
        executed_roots: compiled.executed_roots,
    })
}

fn built_internal() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_internal).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building MethodHandleInternalCases failed:\n{message}"),
    }
}

fn internal_same_as_jvm(mechanism: &str) {
    let Some(built) = built_internal() else {
        return;
    };
    let (jvm, _) = outcome(
        Command::new("java")
            .args([
                "--add-opens=java.base/java.lang.invoke=ALL-UNNAMED",
                "--add-opens=java.base/jdk.internal.misc=ALL-UNNAMED",
                "-cp",
            ])
            .arg(&built.classes)
            .args([built.main_class, mechanism]),
        "Exception in thread \"main\" ",
    );
    assert!(
        jvm.status == Some(0) || jvm.uncaught.is_some(),
        "{mechanism}: JVM failed: {jvm:?}"
    );
    let (native, stop) = common::native_run(built, mechanism);
    assert_eq!(
        native,
        jvm,
        "{mechanism} differs from JVM\nlook at: {LOOK}\njabs stop: {}\njvm: {jvm:#?}\nnative: {native:#?}",
        stop.as_deref().unwrap_or("none")
    );
}

#[test]
fn natives_verify_constants() {
    internal_same_as_jvm("verify_constants");
}

#[test]
fn natives_init_and_expand() {
    internal_same_as_jvm("init_and_expand");
}

#[test]
fn natives_resolve_found() {
    internal_same_as_jvm("resolve_found");
}

#[test]
fn natives_resolve_errors_and_speculative() {
    internal_same_as_jvm("resolve_errors_and_speculative");
}

#[test]
fn natives_field_offsets_and_bases() {
    internal_same_as_jvm("field_offsets_and_bases");
}

#[test]
fn natives_member_vm_info() {
    internal_same_as_jvm("member_vm_info");
}
