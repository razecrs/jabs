// String constants through the JDK's own String code: compact Latin-1 and UTF-16 literals,
// interning within and across classes, and String methods that compile today.
public class Strings {
    static final String SHARED = "shared";
    static int length() { return "hello".length(); }
    static int lengthUtf16() { return "h€llo 😀".length(); }
    static int emptyLength() { return "".length(); }
    static int same() { return "abc" == "abc" ? 1 : 0; }
    static int different() { return "abc" == "abd" ? 1 : 0; }
    static int isEmpty(int i) { return (i > 0 ? "" : "x").isEmpty() ? 1 : 0; }
    static int sameAcross() { return StringSource.get() == SHARED ? 1 : 0; }
    static int loneSurrogate() { return "\ud800".length(); }
}

class StringSource {
    static String get() { return "shared"; }
}
