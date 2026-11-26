// java.net and java.nio on the reference JVM and natively (tests/networking.rs): each mechanism
// is one run, `NetworkingCases <mechanism>`, loopback only. Nothing machine- or run-specific is
// printed unless the JVM on the same machine prints the same (host names, the machine's own
// addresses); ephemeral port numbers never are, only facts about them.
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.BindException;
import java.net.ConnectException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.net.StandardSocketOptions;
import java.net.UnknownHostException;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.DirectoryStream;
import java.nio.file.Files;
import java.nio.file.NoSuchFileException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

class NetworkingCases {
    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("localHostName")) localHostName();
        else if (mechanism.equals("localhostByName")) localhostByName();
        else if (mechanism.equals("ownAddresses")) ownAddresses();
        else if (mechanism.equals("unknownHost")) unknownHost();
        else if (mechanism.equals("literalAddresses")) literalAddresses();
        else if (mechanism.equals("hostByAddress")) hostByAddress();
        else if (mechanism.equals("blockingEcho")) blockingEcho();
        else if (mechanism.equals("bindInUse")) bindInUse();
        else if (mechanism.equals("connectRefused")) connectRefused();
        else if (mechanism.equals("shutdownOutputIsEndOfStream")) shutdownOutputIsEndOfStream();
        else if (mechanism.equals("socketOptions")) socketOptions();
        else if (mechanism.equals("selectorEcho")) selectorEcho();
        else if (mechanism.equals("selectorWakeup")) selectorWakeup();
        else if (mechanism.equals("classicEcho")) classicEcho();
        else if (mechanism.equals("classicBindInUse")) classicBindInUse();
        else if (mechanism.equals("classicReadTimeout")) classicReadTimeout();
        else if (mechanism.equals("networkInterfaces")) networkInterfaces();
        else if (mechanism.equals("nioFiles")) nioFiles();
        else throw new IllegalArgumentException("no mechanism " + mechanism);
    }

    // ---- The resolver ----

    // InetAddress.getLocalHost: the machine's name (Inet6AddressImpl.getLocalHostName), then a
    // lookup of it. log4j's NetUtils.getLocalHostname runs exactly this at startup.
    static void localHostName() throws Exception {
        InetAddress local = InetAddress.getLocalHost();
        System.out.println(local.getHostName());
        System.out.println(local.getClass().getName());
    }

    // getByName/getAllByName("localhost"): the resolver's answer, IPv4 first (the default
    // java.net.preferIPv6Addresses=false), each address named "localhost".
    static void localhostByName() throws Exception {
        System.out.println(InetAddress.getByName("localhost"));
        for (InetAddress a : InetAddress.getAllByName("localhost")) {
            System.out.println(a + " " + a.getClass().getName() + " loopback=" + a.isLoopbackAddress());
        }
    }

    // Every address of the machine's own name, in the order the JDK builds them.
    static void ownAddresses() throws Exception {
        String name = InetAddress.getLocalHost().getHostName();
        for (InetAddress a : InetAddress.getAllByName(name)) System.out.println(a);
    }

    // A name no resolver knows (.invalid is reserved): UnknownHostException with the OS's text.
    static void unknownHost() {
        try {
            InetAddress.getByName("nosuchhost.invalid");
            System.out.println("resolved?");
        } catch (UnknownHostException e) {
            System.out.println(e.getClass().getName() + ": " + e.getMessage());
        }
    }

    // Literals never reach the resolver; the loopback address depends on which loopback
    // addresses an interface has (NetworkInterface.boundInetAddress0).
    static void literalAddresses() throws Exception {
        System.out.println(InetAddress.getByName("127.0.0.1"));
        System.out.println(InetAddress.getByName("::1"));
        System.out.println(InetAddress.getLoopbackAddress());
    }

    // A reverse lookup of 127.0.0.1 (getHostByAddr), answered from the machine's own tables.
    static void hostByAddress() throws Exception {
        InetAddress a = InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        System.out.println(a.getHostName());
    }

    // ---- NIO sockets, blocking ----

    static InetSocketAddress anyLoopbackPort() throws Exception {
        return new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0);
    }

    static String text(ByteBuffer b) {
        b.flip();
        byte[] bytes = new byte[b.remaining()];
        b.get(bytes);
        return new String(bytes, StandardCharsets.US_ASCII);
    }

    // Read exactly n bytes from a blocking channel.
    static String readFully(SocketChannel c, int n) throws Exception {
        ByteBuffer b = ByteBuffer.allocate(n);
        while (b.hasRemaining()) {
            if (c.read(b) < 0) return "early end after " + b.position();
        }
        return text(b);
    }

    // A ServerSocketChannel on 127.0.0.1:0, a client SocketChannel connecting to it, bytes
    // both ways, then the client closes and the server side reads the end of the stream.
    static void blockingEcho() throws Exception {
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(anyLoopbackPort());
            InetSocketAddress bound = (InetSocketAddress) server.getLocalAddress();
            System.out.println("bound " + bound.getAddress() + " ephemeral=" + (bound.getPort() > 0));
            SocketChannel client = SocketChannel.open(bound);
            SocketChannel accepted = server.accept();
            System.out.println("client remote is the server: " + client.getRemoteAddress().equals(bound));
            System.out.println("accepted remote is the client: " + accepted.getRemoteAddress().equals(client.getLocalAddress()));
            System.out.println("accepted local is the server: " + accepted.getLocalAddress().equals(bound));
            client.write(ByteBuffer.wrap("ping".getBytes(StandardCharsets.US_ASCII)));
            System.out.println("server got " + readFully(accepted, 4));
            accepted.write(ByteBuffer.wrap("pong!".getBytes(StandardCharsets.US_ASCII)));
            System.out.println("client got " + readFully(client, 5));
            client.close();
            System.out.println("after close: " + accepted.read(ByteBuffer.allocate(8)));
            accepted.close();
            System.out.println("open: " + client.isOpen() + " " + accepted.isOpen());
        }
    }

    // A second bind to an address already bound: BindException with the OS's message.
    static void bindInUse() throws Exception {
        try (ServerSocketChannel first = ServerSocketChannel.open(); ServerSocketChannel second = ServerSocketChannel.open()) {
            first.bind(anyLoopbackPort());
            try {
                second.bind(first.getLocalAddress());
                System.out.println("bound twice?");
            } catch (BindException e) {
                System.out.println(e.getClass().getName() + ": " + e.getMessage());
            }
        }
    }

    // A connect to a port nothing listens on (one just closed): ConnectException.
    static void connectRefused() throws Exception {
        InetSocketAddress gone;
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(anyLoopbackPort());
            gone = (InetSocketAddress) server.getLocalAddress();
        }
        try (SocketChannel client = SocketChannel.open()) {
            client.connect(gone);
            System.out.println("connected?");
        } catch (ConnectException e) {
            System.out.println(e.getClass().getName() + ": " + e.getMessage());
        }
    }

    // shutdownOutput on one side is the end of the stream on the other, which can still write.
    static void shutdownOutputIsEndOfStream() throws Exception {
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(anyLoopbackPort());
            try (SocketChannel client = SocketChannel.open(server.getLocalAddress()); SocketChannel accepted = server.accept()) {
                client.shutdownOutput();
                System.out.println("server reads " + accepted.read(ByteBuffer.allocate(4)));
                accepted.write(ByteBuffer.wrap("late".getBytes(StandardCharsets.US_ASCII)));
                System.out.println("client still reads " + readFully(client, 4));
            }
        }
    }

    // Socket options through Net.setIntOption0/getIntOption0, values as the OS reports them.
    static void socketOptions() throws Exception {
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.setOption(StandardSocketOptions.SO_REUSEADDR, true);
            server.bind(anyLoopbackPort());
            System.out.println("server reuseaddr " + server.getOption(StandardSocketOptions.SO_REUSEADDR));
            try (SocketChannel client = SocketChannel.open(server.getLocalAddress())) {
                client.setOption(StandardSocketOptions.TCP_NODELAY, true);
                client.setOption(StandardSocketOptions.SO_KEEPALIVE, true);
                client.setOption(StandardSocketOptions.SO_LINGER, 5);
                client.setOption(StandardSocketOptions.SO_RCVBUF, 65536);
                System.out.println("nodelay " + client.getOption(StandardSocketOptions.TCP_NODELAY));
                System.out.println("keepalive " + client.getOption(StandardSocketOptions.SO_KEEPALIVE));
                System.out.println("linger " + client.getOption(StandardSocketOptions.SO_LINGER));
                System.out.println("rcvbuf " + client.getOption(StandardSocketOptions.SO_RCVBUF));
                client.setOption(StandardSocketOptions.SO_LINGER, -1);
                System.out.println("linger off " + client.getOption(StandardSocketOptions.SO_LINGER));
            }
        }
    }

    // ---- NIO selector ----

    // Select until `key` is ready for `ops`, at most ten seconds; the ready ops, or -1.
    static int awaitReady(Selector selector, SelectionKey key, int ops) throws Exception {
        long deadline = System.currentTimeMillis() + 10000;
        while (System.currentTimeMillis() < deadline) {
            selector.select(200);
            boolean ready = selector.selectedKeys().remove(key) && (key.readyOps() & ops) != 0;
            selector.selectedKeys().clear();
            if (ready) return key.readyOps() & ops;
        }
        return -1;
    }

    // Non-blocking server and client on one Selector: OP_ACCEPT, OP_CONNECT and OP_READ
    // readiness, one step at a time so the printed order does not depend on timing.
    static void selectorEcho() throws Exception {
        try (Selector selector = Selector.open(); ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(anyLoopbackPort());
            server.configureBlocking(false);
            SelectionKey acceptKey = server.register(selector, SelectionKey.OP_ACCEPT);
            System.out.println("accept before any client: " + server.accept());
            SocketChannel client = SocketChannel.open();
            client.configureBlocking(false);
            boolean immediate = client.connect(server.getLocalAddress());
            SelectionKey connectKey = client.register(selector, immediate ? SelectionKey.OP_READ : SelectionKey.OP_CONNECT);
            System.out.println("accept ready: " + (awaitReady(selector, acceptKey, SelectionKey.OP_ACCEPT) == SelectionKey.OP_ACCEPT));
            SocketChannel accepted = server.accept();
            System.out.println("accepted: " + (accepted != null));
            accepted.configureBlocking(false);
            if (!immediate) {
                System.out.println("connect ready: " + (awaitReady(selector, connectKey, SelectionKey.OP_CONNECT) == SelectionKey.OP_CONNECT));
                System.out.println("finishConnect: " + client.finishConnect());
                connectKey.interestOps(SelectionKey.OP_READ);
            } else {
                System.out.println("connect ready: true");
                System.out.println("finishConnect: true");
            }
            SelectionKey readKey = accepted.register(selector, SelectionKey.OP_READ);
            System.out.println("read before data: " + accepted.read(ByteBuffer.allocate(4)));
            client.write(ByteBuffer.wrap("ping".getBytes(StandardCharsets.US_ASCII)));
            System.out.println("read ready: " + (awaitReady(selector, readKey, SelectionKey.OP_READ) == SelectionKey.OP_READ));
            ByteBuffer got = ByteBuffer.allocate(4);
            while (got.hasRemaining() && awaitReady(selector, readKey, SelectionKey.OP_READ) > 0) accepted.read(got);
            System.out.println("server got " + text(got));
            accepted.write(ByteBuffer.wrap("pong".getBytes(StandardCharsets.US_ASCII)));
            System.out.println("client read ready: " + (awaitReady(selector, connectKey, SelectionKey.OP_READ) == SelectionKey.OP_READ));
            ByteBuffer back = ByteBuffer.allocate(4);
            while (back.hasRemaining() && awaitReady(selector, connectKey, SelectionKey.OP_READ) > 0) client.read(back);
            System.out.println("client got " + text(back));
            client.close();
            System.out.println("end of stream ready: " + (awaitReady(selector, readKey, SelectionKey.OP_READ) == SelectionKey.OP_READ));
            System.out.println("after close: " + accepted.read(ByteBuffer.allocate(4)));
            accepted.close();
            System.out.println("keys left after closing: " + selector.keys().size());
        }
    }

    // Selector.wakeup before select makes the next select return at once; selectNow has nothing.
    static void selectorWakeup() throws Exception {
        try (Selector selector = Selector.open()) {
            selector.wakeup();
            long start = System.nanoTime();
            int n = selector.select(30000);
            System.out.println("woken: " + n + " fast=" + (System.nanoTime() - start < 10_000_000_000L));
            System.out.println("selectNow: " + selector.selectNow());
            System.out.println("timed: " + selector.select(50));
        }
        System.out.println("closed");
    }

    // ---- java.net.ServerSocket and Socket (NioSocketImpl in JDK 25) ----

    static void classicEcho() throws Exception {
        try (ServerSocket server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
            System.out.println("bound " + server.getInetAddress() + " ephemeral=" + (server.getLocalPort() > 0));
            try (Socket client = new Socket(server.getInetAddress(), server.getLocalPort()); Socket accepted = server.accept()) {
                System.out.println("ports match: " + (accepted.getPort() == client.getLocalPort()));
                OutputStream out = client.getOutputStream();
                out.write("hello".getBytes(StandardCharsets.US_ASCII));
                out.flush();
                InputStream in = accepted.getInputStream();
                byte[] got = new byte[5];
                int n = 0;
                while (n < 5) {
                    int r = in.read(got, n, 5 - n);
                    if (r < 0) break;
                    n += r;
                }
                System.out.println("server got " + new String(got, 0, n, StandardCharsets.US_ASCII));
                accepted.getOutputStream().write('!');
                System.out.println("client got " + (char) client.getInputStream().read());
                client.close();
                System.out.println("after close: " + in.read());
            }
        }
    }

    static void classicBindInUse() throws Exception {
        try (ServerSocket first = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
            try (ServerSocket second = new ServerSocket(first.getLocalPort(), 50, InetAddress.getByName("127.0.0.1"))) {
                System.out.println("bound twice?");
            } catch (BindException e) {
                System.out.println(e.getClass().getName() + ": " + e.getMessage());
            }
        }
    }

    // A read with SO_TIMEOUT and no data: SocketTimeoutException (NioSocketImpl polls).
    static void classicReadTimeout() throws Exception {
        try (ServerSocket server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
            try (Socket client = new Socket(server.getInetAddress(), server.getLocalPort()); Socket accepted = server.accept()) {
                accepted.setSoTimeout(100);
                try {
                    accepted.getInputStream().read();
                    System.out.println("read?");
                } catch (SocketTimeoutException e) {
                    System.out.println(e.getClass().getName() + ": " + e.getMessage());
                }
                System.out.println("still open: " + !accepted.isClosed());
            }
        }
    }

    // ---- NetworkInterface (Windows: SecureRandom's seed generator hashes this list) ----

    static void networkInterfaces() throws Exception {
        for (NetworkInterface n : Collections.list(NetworkInterface.getNetworkInterfaces())) {
            byte[] mac = n.getHardwareAddress();
            StringBuilder hex = new StringBuilder();
            if (mac != null) for (byte b : mac) hex.append(String.format("%02x", b));
            System.out.println(n.getName() + " | " + n.getDisplayName() + " | " + n.getIndex()
                + " | loopback=" + n.isLoopback() + " | mac=" + (mac == null ? "none" : hex)
                + " | " + Collections.list(n.getInetAddresses()) + " | " + n.getInterfaceAddresses());
        }
    }

    // ---- java.nio.file: what java.security and the Windows selector's pipe use ----

    // A file written with java.io, then looked at, read, listed and deleted with java.nio.file.
    static void nioFiles() throws Exception {
        File dir = new File(System.getProperty("java.io.tmpdir"), "jabs-nio-" + Long.toHexString(System.nanoTime()));
        System.out.println("mkdir " + dir.mkdir());
        File file = new File(dir, "note.txt");
        try (FileOutputStream out = new FileOutputStream(file)) {
            out.write("hello nio\n".getBytes(StandardCharsets.US_ASCII));
        }
        Path path = file.toPath();
        System.out.println("exists " + Files.exists(path) + " regular " + Files.isRegularFile(path) + " directory " + Files.isDirectory(dir.toPath()));
        System.out.println("size " + Files.size(path));
        try (InputStream in = Files.newInputStream(path)) {
            byte[] buffer = new byte[64];
            int n = 0, r;
            while ((r = in.read(buffer, n, buffer.length - n)) > 0) n += r;
            System.out.print("read " + new String(buffer, 0, n, StandardCharsets.US_ASCII));
        }
        System.out.println("real name " + path.toRealPath().getFileName());
        List<String> names = new ArrayList<>();
        try (DirectoryStream<Path> entries = Files.newDirectoryStream(dir.toPath())) {
            for (Path p : entries) names.add(p.getFileName().toString());
        }
        System.out.println("listed " + names);
        Files.delete(path);
        System.out.println("after delete exists " + Files.exists(path));
        try {
            Files.newInputStream(path).close();
            System.out.println("opened a deleted file?");
        } catch (NoSuchFileException e) {
            System.out.println(e.getClass().getName());
        }
        System.out.println("rmdir " + dir.delete());
    }
}
