package jabfix;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileNotFoundException;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.RandomAccessFile;
import java.net.JarURLConnection;
import java.net.URL;
import java.net.URLConnection;
import java.net.URLDecoder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.Security;
import java.time.Instant;
import java.time.ZoneId;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.TimeZone;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarInputStream;

/**
 * What a program reads from its class path and java.home, one mechanism per argument (tests/jab.rs
 * runs each on the JVM from a real jar and natively from the program's .jab). Output never
 * contains a path, since the JVM's jar and the native .jab live in different places.
 */
public class JabFiles {
    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "scan": scan(); break;
            case "jarurl": jarUrl(); break;
            case "stream": stream(); break;
            case "resources": resources(); break;
            case "home": home(); break;
            case "missing": missing(); break;
            case "readonly": readOnly(); break;
            default: throw new IllegalArgumentException(args[0]);
        }
    }

    /** InputStream.readAllBytes, which release 8 (the class files Minecraft 1.7.10 has) lacks. */
    static byte[] readAll(InputStream in) throws IOException {
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        byte[] buffer = new byte[4096];
        for (int n; (n = in.read(buffer)) > 0;) out.write(buffer, 0, n);
        return out.toByteArray();
    }

    /** log4j 2.0-beta9's ResolverUtil.findInPackage: getResources(package), the jar's path from
     * the URL, then every entry of that jar read sequentially through JarInputStream. */
    static void scan() throws IOException {
        Enumeration<URL> urls = JabFiles.class.getClassLoader().getResources("jabfix/plugins");
        List<String> found = new ArrayList<>();
        while (urls.hasMoreElements()) {
            URL url = urls.nextElement();
            System.out.println("protocol " + url.getProtocol());
            String path = URLDecoder.decode(url.getFile(), "UTF-8");
            if (path.startsWith("file:")) path = path.substring(5);
            if (path.indexOf('!') > 0) path = path.substring(0, path.indexOf('!'));
            File file = new File(path);
            System.out.println("directory " + file.isDirectory() + " file " + file.isFile());
            try (JarInputStream jar = new JarInputStream(new FileInputStream(file))) {
                System.out.println("manifest " + (jar.getManifest() != null));
                for (JarEntry e; (e = jar.getNextJarEntry()) != null;) {
                    if (!e.isDirectory() && e.getName().startsWith("jabfix/plugins")) found.add(e.getName());
                }
            }
        }
        Collections.sort(found);
        for (String name : found) System.out.println(name);
        for (String name : found) {
            String binary = name.substring(0, name.length() - ".class".length()).replace('/', '.');
            try {
                System.out.println("loaded " + Class.forName(binary, false, JabFiles.class.getClassLoader()).getName());
            } catch (ClassNotFoundException e) {
                System.out.println("not loadable " + binary);
            }
        }
    }

    /** The same package through the jar: URL's own connection (JarURLConnection, JarFile). */
    static void jarUrl() throws IOException {
        URL url = JabFiles.class.getClassLoader().getResource("jabfix/plugins/");
        URLConnection c = url.openConnection();
        System.out.println("connection is a JarURLConnection: " + (c instanceof JarURLConnection));
        JarURLConnection jar = (JarURLConnection) c;
        System.out.println("entry name " + jar.getEntryName());
        JarFile file = jar.getJarFile();
        List<String> names = new ArrayList<>();
        for (Enumeration<JarEntry> e = file.entries(); e.hasMoreElements();) {
            JarEntry entry = e.nextElement();
            if (entry.getName().startsWith("jabfix/")) names.add(entry.getName() + (entry.isDirectory() ? " dir" : " " + entry.getSize()));
        }
        Collections.sort(names);
        for (String n : names) System.out.println(n);
        JarEntry data = file.getJarEntry("jabfix/data/hello.txt");
        try (InputStream in = file.getInputStream(data)) {
            System.out.println("hello.txt via JarFile: " + new String(readAll(in), "UTF-8").trim());
        }
        System.out.println("manifest main class " + file.getManifest().getMainAttributes().getValue("Main-Class"));
    }

    /** getResourceAsStream and getResource, present and missing, from the class and its loader. */
    static void stream() throws IOException {
        ClassLoader loader = JabFiles.class.getClassLoader();
        try (InputStream in = loader.getResourceAsStream("jabfix/data/hello.txt")) {
            System.out.println("loader stream: " + new String(readAll(in), "UTF-8").trim());
        }
        try (InputStream in = JabFiles.class.getResourceAsStream("data/hello.txt")) {
            System.out.println("class-relative stream: " + new String(readAll(in), "UTF-8").trim());
        }
        System.out.println("missing stream null: " + (loader.getResourceAsStream("jabfix/data/none.txt") == null));
        System.out.println("missing url null: " + (loader.getResource("jabfix/data/none.txt") == null));
        URL url = loader.getResource("jabfix/data/hello.txt");
        System.out.println("url protocol " + url.getProtocol() + ", ends " + url.toString().substring(url.toString().indexOf("!/")));
        try (InputStream in = url.openStream()) {
            System.out.println("url stream bytes " + readAll(in).length);
        }
        byte[] own = readAll(JabFiles.class.getResourceAsStream("JabFiles.class"));
        System.out.println("own class bytes start CAFEBABE: " + ((own[0] & 0xff) == 0xca && (own[1] & 0xff) == 0xfe));
    }

    /** getResources over several names: directories, files and missing ones. */
    static void resources() throws IOException {
        ClassLoader loader = JabFiles.class.getClassLoader();
        String[] names = {"jabfix", "jabfix/", "jabfix/data", "jabfix/data/hello.txt", "jabfix/data/other.txt", "jabfix/nothing", "META-INF/MANIFEST.MF"};
        for (String name : names) {
            int count = 0;
            for (Enumeration<URL> e = loader.getResources(name); e.hasMoreElements(); e.nextElement()) count++;
            System.out.println(name + ": " + count);
        }
    }

    /** java.home reads: the default zone (tzmappings on Windows), the tzdb rules, the security
     * properties, and the same files through java.io, RandomAccessFile and java.nio.file. */
    static void home() throws IOException {
        System.out.println("default zone " + TimeZone.getDefault().getID());
        System.out.println("Paris offset 2020-07-01 " + ZoneId.of("Europe/Paris").getRules().getOffset(Instant.parse("2020-07-01T00:00:00Z")));
        System.out.println("securerandom.source " + Security.getProperty("securerandom.source"));
        File home = new File(System.getProperty("java.home"));
        File lib = new File(home, "lib");
        File tzdb = new File(lib, "tzdb.dat");
        System.out.println("home is a directory " + home.isDirectory() + ", lib " + lib.isDirectory());
        System.out.println("tzdb exists " + tzdb.exists() + " file " + tzdb.isFile() + " length " + tzdb.length());
        try (FileInputStream in = new FileInputStream(tzdb)) {
            byte[] head = new byte[4];
            int n = in.read(head);
            System.out.println("tzdb head " + n + " " + head[0] + " " + head[1] + " " + head[2] + " " + head[3] + " available " + in.available());
            System.out.println("skip " + in.skip(10) + " then " + in.read());
        }
        try (RandomAccessFile raf = new RandomAccessFile(tzdb, "r")) {
            raf.seek(raf.length() - 8);
            byte[] tail = new byte[8];
            raf.readFully(tail);
            System.out.println("tzdb tail at " + raf.getFilePointer() + " last byte " + tail[7]);
        }
        Path security = Paths.get(home.getPath(), "conf", "security", "java.security");
        System.out.println("java.security nio exists " + Files.exists(security) + " size " + Files.size(security) + " read " + Files.readAllBytes(security).length);
        String[] listed = new File(home, "conf" + File.separator + "security").list();
        System.out.println("conf/security lists java.security " + (listed != null && java.util.Arrays.asList(listed).contains("java.security")));
    }

    /** A java.home file the program never read: missing natively (it is not in the .jab). */
    static void missing() throws IOException {
        File cfg = new File(new File(System.getProperty("java.home"), "lib"), "jvm.cfg");
        System.out.println("jvm.cfg exists " + cfg.exists());
        try (FileInputStream in = new FileInputStream(cfg)) {
            System.out.println("opened " + in.read());
        } catch (FileNotFoundException e) {
            String m = e.getMessage();
            System.out.println("FileNotFoundException" + m.substring(m.lastIndexOf(" (")));
        }
        try {
            Files.readAllBytes(cfg.toPath());
            System.out.println("nio read");
        } catch (java.nio.file.NoSuchFileException e) {
            System.out.println("NoSuchFileException");
        }
    }

    /** Writing under java.home: refused, as on a read-only file system. */
    static void readOnly() throws IOException {
        // Never against a real JDK: this tries to write, make a directory and delete tzdb.dat.
        if (!System.getProperty("java.home").endsWith("jabs-home")) throw new IllegalStateException("readonly runs only with a .jab's java.home");
        File target = new File(new File(System.getProperty("java.home"), "lib"), "written.dat");
        try (FileOutputStream out = new FileOutputStream(target)) {
            System.out.println("opened for writing");
        } catch (FileNotFoundException e) {
            String m = e.getMessage();
            System.out.println("FileNotFoundException" + m.substring(m.lastIndexOf(" (")));
        }
        System.out.println("mkdir " + new File(target.getParentFile(), "newdir").mkdir() + ", delete " + new File(target.getParentFile(), "tzdb.dat").delete());
    }
}
