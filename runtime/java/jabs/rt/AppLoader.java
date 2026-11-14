package jabs.rt;

import java.io.ByteArrayInputStream;
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

/**
 * The application class loader of a native JABS program. Classes are compiled into the program,
 * so loading one means finding it in the runtime's registry; resources from the input jars are
 * embedded in the program and served through {@code jabs:} URLs, without file or zip code.
 *
 * <p>Compile with {@code javac -XDstringConcat=inline} (the native compiler has no invokedynamic).
 */
public final class AppLoader extends ClassLoader {
    private static final Handler HANDLER = new Handler();
    static final AppLoader INSTANCE = new AppLoader();

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

    @Override
    public URL getResource(String name) {
        return Natives.resourceCount(name) == 0 ? null : url(name, 0);
    }

    @Override
    public Enumeration<URL> getResources(String name) {
        int count = Natives.resourceCount(name);
        ArrayList<URL> urls = new ArrayList<>(count);
        for (int i = 0; i < count; i++) {
            urls.add(url(name, i));
        }
        return Collections.enumeration(urls);
    }

    @Override
    public InputStream getResourceAsStream(String name) {
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
