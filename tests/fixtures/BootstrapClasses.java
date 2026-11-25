// The boot class loader, as the JDK reaches it (Class.forName(Module, String) goes through
// BootLoader to ClassLoader.findBootstrapClass): JDK classes are found without being
// initialized and missing ones are null rather than an exception.
//
// Not checked here, and why:
// - DecimalFormat's locale data comes through the same path, but it also loads
//   CLDRLocaleProviderAdapter by name, which a unit without a recorded class list does not
//   keep; the native server boot covers that path.
// - Application classes are invisible to the boot loader, but the differential harness marks
//   no fixture class as the application's, so every fixture class counts as a boot class.
public class BootstrapClasses {
    public static void main(String[] args) {
        Module base = Object.class.getModule();
        Class<?> found = Class.forName(base, "java.util.ArrayList");
        System.out.println(found == null ? "null" : found.getName());
        System.out.println(Class.forName(base, "java.util.NoSuchClassAnywhere"));
    }
}
