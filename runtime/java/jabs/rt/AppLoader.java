package jabs.rt;

import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.net.MalformedURLException;
import java.net.URL;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.jar.JarFile;

import sun.net.www.ParseUtil;

/**
 * The application class loader of a native JABS program. Classes are compiled into the program,
 * so loading one means finding it in the runtime's registry.
 *
 * <p>Resources come from the program's .jab when it has one (runtime/jabs_jab.c): a lookup is a
 * {@link JarFile} lookup, and the URL is the {@code jar:file:...!/name} URL the JVM's class path
 * (URLClassPath.JarLoader) gives, so code that opens it, or reads the jar it names, gets the
 * JDK's own jar handling. Without a .jab the input jars' resources are embedded in the program
 * and served through {@code jabs:} URLs.
 *
 * <p>Compile with {@code javac -XDstringConcat=inline --add-exports
 * java.base/sun.net.www=ALL-UNNAMED} (the native compiler has no invokedynamic; ParseUtil makes
 * the URLs exactly as URLClassPath does).
 */
public final class AppLoader extends ClassLoader {
    private static final Handler HANDLER = new Handler();
    static final AppLoader INSTANCE = new AppLoader();

    /** The .jab, opened on the first resource lookup; null without one. */
    private JarFile jab;
    private URL jabBase;

    private AppLoader() {
        super(null);
    }

    @Override
    protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
        Class<?> c = Natives.classNamed(name);
        if (c == null) {
            throw new ClassNotFoundException(name);
        }
        return c;
    }

    @Override
    protected Class<?> findClass(String name) throws ClassNotFoundException {
        return loadClass(name, false);
    }

    /** The .jab as a JarFile, opened once; null when the program has none. A .jab that cannot
     * be opened is an error, not "no resources": the runtime checked it at startup. */
    private synchronized JarFile jab() {
        if (jab != null) {
            return jab;
        }
        String path = Natives.jabPath();
        if (path == null) {
            return null;
        }
        try {
            File file = new File(path);
            JarFile opened = new JarFile(file);
            // As URLClassPath: the canonical file's URL, then "!/".
            jabBase = new URL("jar", "", -1, ParseUtil.fileToEncodedURL(file.getCanonicalFile()) + "!/");
            jab = opened;
        } catch (IOException e) {
            throw new IllegalStateException("the program's .jab cannot be opened: " + path, e);
        }
        return jab;
    }

    /** The jar: URL of a .jab entry (URLClassPath.JarLoader.createResource), or null. */
    private URL jabResource(JarFile file, String name) {
        if (file.getJarEntry(name) == null) {
            return null;
        }
        try {
            return new URL(jabBase, ParseUtil.encodePath(name, false));
        } catch (MalformedURLException e) {
            return null;
        }
    }

    @Override
    public URL getResource(String name) {
        JarFile file = jab();
        if (file != null) {
            return jabResource(file, name);
        }
        return Natives.resourceCount(name) == 0 ? null : url(name, 0);
    }

    @Override
    public Enumeration<URL> getResources(String name) {
        JarFile file = jab();
        if (file != null) {
            URL url = jabResource(file, name);
            ArrayList<URL> urls = new ArrayList<>(1);
            if (url != null) {
                urls.add(url);
            }
            return Collections.enumeration(urls);
        }
        int count = Natives.resourceCount(name);
        ArrayList<URL> urls = new ArrayList<>(count);
        for (int i = 0; i < count; i++) {
            urls.add(url(name, i));
        }
        return Collections.enumeration(urls);
    }

    @Override
    public InputStream getResourceAsStream(String name) {
        if (jab() != null) {
            // As the JVM's class loaders: open the resource's URL.
            return super.getResourceAsStream(name);
        }
        byte[] data = Natives.resource(name, 0);
        return data == null ? null : new ByteArrayInputStream(data);
    }

    /** {@code jabs:/<index>/<name>}: the index-th embedded resource with that name. */
    private static URL url(String name, int index) {
        try {
            return new URL("jabs", "", -1, "/" + index + "/" + name, HANDLER);
        } catch (MalformedURLException e) {
            throw new IllegalStateException(e);
        }
    }

    /** Opens {@code jabs:} URLs. */
    static final class Handler extends URLStreamHandler {
        @Override
        protected URLConnection openConnection(URL u) {
            return new Connection(u);
        }
    }

    /** A connection to one embedded resource. */
    static final class Connection extends URLConnection {
        Connection(URL u) {
            super(u);
        }

        @Override
        public void connect() {
            connected = true;
        }

        @Override
        public InputStream getInputStream() throws IOException {
            String path = url.getFile();
            int slash = path.indexOf('/', 1);
            byte[] data = slash < 0 ? null
                    : Natives.resource(path.substring(slash + 1), Integer.parseInt(path.substring(1, slash)));
            if (data == null) {
                throw new FileNotFoundException(url.toString());
            }
            return new ByteArrayInputStream(data);
        }
    }
}
