import java.lang.reflect.Field;
import java.lang.reflect.InaccessibleObjectException;
import java.lang.reflect.Method;
import java.nio.Buffer;

// Module encapsulation as JDK 25 enforces it for a class-path program: JDK classes are in their
// named modules (java.base, ...), this program in an unnamed module, and nothing of java.base is
// opened to it. One mechanism per argument (tests/modules.rs runs each on the JVM and natively).
class ModulesCases {
    private int secret = 7;

    static void print(Object o) {
        System.out.println(o);
    }

    // Module.toString of an unnamed module ends with "@" and its identity hash, which differs
    // from run to run: everything before it is compared.
    static String withoutIdentity(String message) {
        if (message == null) return "null";
        int at = message.indexOf(" @");
        return at < 0 ? message : message.substring(0, at);
    }

    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "setAccessibleJdkInternal": setAccessibleJdkInternal(); break;
            case "trySetAccessible": trySetAccessible(); break;
            case "ownPrivateField": ownPrivateField(); break;
            case "moduleQueries": moduleQueries(); break;
            case "descriptor": descriptor(); break;
            case "nettyProbe": nettyProbe(); break;
            case "invokeNotExported": invokeNotExported(); break;
            default: throw new IllegalArgumentException(args[0]);
        }
    }

    // java.nio is exported but not opened: a private field of Buffer cannot be made accessible.
    static void setAccessibleJdkInternal() throws Exception {
        Field address = Buffer.class.getDeclaredField("address");
        try {
            address.setAccessible(true);
            print("accessible");
        } catch (InaccessibleObjectException e) {
            print(e.getClass().getName());
            print(withoutIdentity(e.getMessage()));
        }
    }

    static void trySetAccessible() throws Exception {
        print("Buffer.address: " + Buffer.class.getDeclaredField("address").trySetAccessible());
        print("Integer.MAX_VALUE: " + Integer.class.getField("MAX_VALUE").trySetAccessible());
        print("String.value: " + String.class.getDeclaredField("value").trySetAccessible());
    }

    static void ownPrivateField() throws Exception {
        Field secret = ModulesCases.class.getDeclaredField("secret");
        secret.setAccessible(true);
        print("secret = " + secret.get(new ModulesCases()));
    }

    static void moduleQueries() {
        Module base = Object.class.getModule();
        Module self = ModulesCases.class.getModule();
        print("Object: " + base.getName() + " named " + base.isNamed());
        print("String: same module " + (String.class.getModule() == base));
        print("int: " + int.class.getModule().getName());
        print("String[]: " + String[].class.getModule().getName());
        print("long[][]: " + long[][].class.getModule().getName());
        print("exported java.lang " + base.isExported("java.lang"));
        print("open java.lang " + base.isOpen("java.lang"));
        print("exported jdk.internal.misc " + base.isExported("jdk.internal.misc"));
        print("exported jdk.internal.misc to us " + base.isExported("jdk.internal.misc", self));
        print("open java.nio to us " + base.isOpen("java.nio", self));
        print("open java.nio to itself " + base.isOpen("java.nio", base));
        print("packages has java.lang " + base.getPackages().contains("java.lang"));
        print("packages has jdk.internal.misc " + base.getPackages().contains("jdk.internal.misc"));
        print("ours: named " + self.isNamed() + " name " + self.getName());
        print("ours array: named " + ModulesCases[].class.getModule().isNamed());
        print("we read java.base " + self.canRead(base));
        print("java.base reads us " + base.canRead(self));
        print("java.base reads itself " + base.canRead(base));
        print("toString " + base);
    }

    // The descriptor is the JDK's own parse of java.base's module-info.class.
    static void descriptor() {
        java.lang.module.ModuleDescriptor d = Object.class.getModule().getDescriptor();
        print("name " + d.name() + " open " + d.isOpen() + " automatic " + d.isAutomatic());
        print("requires " + d.requires().size());
        int unqualified = 0, qualified = 0;
        for (java.lang.module.ModuleDescriptor.Exports e : d.exports()) {
            if (e.isQualified()) qualified++;
            else unqualified++;
        }
        print("exports " + unqualified + " unqualified, " + qualified + " qualified");
        print("opens " + d.opens().size());
        print("packages " + d.packages().size());
        print("uses java.nio.file.spi.FileSystemProvider "
            + d.uses().contains("java.nio.file.spi.FileSystemProvider"));
        print("version present " + d.version().isPresent());
        print("ours has no descriptor " + (ModulesCases.class.getModule().getDescriptor() == null));
    }

    // Netty's PlatformDependent0: try to make Buffer.address accessible; on failure, remember the
    // Throwable and take the no-Unsafe branch.
    static void nettyProbe() throws Exception {
        Object maybeAddressField;
        try {
            Field field = Buffer.class.getDeclaredField("address");
            field.setAccessible(true);
            maybeAddressField = field;
        } catch (Throwable cause) {
            maybeAddressField = cause;
        }
        if (maybeAddressField instanceof Throwable) {
            print("direct buffer address unavailable: " + maybeAddressField.getClass().getName());
        } else {
            print("direct buffer address field: " + ((Field) maybeAddressField).getName());
        }
    }

    // A public method of a public class in a package java.base does not export: Method.invoke
    // refuses (Reflection.verifyModuleAccess), even though the class is public.
    static void invokeNotExported() throws Exception {
        Class<?> vm = Class.forName("jdk.internal.misc.VM");
        Method booted = vm.getMethod("isBooted");
        try {
            print("invoked: " + booted.invoke(null));
        } catch (IllegalAccessException e) {
            print(e.getClass().getName());
            print(withoutIdentity(e.getMessage()));
        }
    }
}
