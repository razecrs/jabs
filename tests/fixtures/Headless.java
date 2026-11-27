import java.awt.GraphicsEnvironment;

/** GraphicsEnvironment.isHeadless, decided by java.awt.headless or, when it is unset, by the
 *  platform (on Windows, sun.awt.PlatformGraphicsInfo.hasDisplays0). Each mechanism runs in its
 *  own process because GraphicsEnvironment decides once and keeps the answer. */
public class Headless {
    public static void main(String[] args) {
        switch (args[0]) {
            case "platformDefault":
                break;
            case "propertyTrue":
                System.setProperty("java.awt.headless", "true");
                break;
            case "propertyFalse":
                System.setProperty("java.awt.headless", "false");
                break;
            default:
                throw new IllegalArgumentException(args[0]);
        }
        System.out.println(args[0] + ": headless=" + GraphicsEnvironment.isHeadless());
        System.out.println(args[0] + ": again=" + GraphicsEnvironment.isHeadless());
    }
}
