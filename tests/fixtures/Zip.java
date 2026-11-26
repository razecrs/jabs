// java.util.zip differential fixtures for tests/zip.rs: `ZipCases <mechanism>` runs one
// mechanism on the reference JVM and natively so stdout, exit code, and uncaught exceptions
// can be compared line by line. Never print implementation-dependent compressed byte counts.
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Random;
import java.util.zip.Adler32;
import java.util.zip.CRC32;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.DeflaterOutputStream;
import java.util.zip.GZIPInputStream;
import java.util.zip.GZIPOutputStream;
import java.util.zip.Inflater;
import java.util.zip.InflaterInputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

class ZipCases {
    // Known-good payloads for "Hello from standard zlib! 1234567890" compressed at default level.
    private static final byte[] ZLIB_KNOWN = {
        (byte) 0x78, (byte) 0x9c, (byte) 0xf3, (byte) 0x48, (byte) 0xcd, (byte) 0xc9, (byte) 0xc9,
        (byte) 0x57, (byte) 0x48, (byte) 0x2b, (byte) 0xca, (byte) 0xcf, (byte) 0x55, (byte) 0x28,
        (byte) 0x2e, (byte) 0x49, (byte) 0xcc, (byte) 0x4b, (byte) 0x49, (byte) 0x2c, (byte) 0x4a,
        (byte) 0x51, (byte) 0xa8, (byte) 0xca, (byte) 0xc9, (byte) 0x4c, (byte) 0x52, (byte) 0x54,
        (byte) 0x30, (byte) 0x34, (byte) 0x32, (byte) 0x36, (byte) 0x31, (byte) 0x35, (byte) 0x33,
        (byte) 0xb7, (byte) 0xb0, (byte) 0x34, (byte) 0x00, (byte) 0x00, (byte) 0xea, (byte) 0x3e,
        (byte) 0x0b, (byte) 0x59
    };

    private static final byte[] RAW_KNOWN = {
        (byte) 0xf3, (byte) 0x48, (byte) 0xcd, (byte) 0xc9, (byte) 0xc9, (byte) 0x57, (byte) 0x48,
        (byte) 0x2b, (byte) 0xca, (byte) 0xcf, (byte) 0x55, (byte) 0x28, (byte) 0x2e, (byte) 0x49,
        (byte) 0xcc, (byte) 0x4b, (byte) 0x49, (byte) 0x2c, (byte) 0x4a, (byte) 0x51, (byte) 0xa8,
        (byte) 0xca, (byte) 0xc9, (byte) 0x4c, (byte) 0x52, (byte) 0x54, (byte) 0x30, (byte) 0x34,
        (byte) 0x32, (byte) 0x36, (byte) 0x31, (byte) 0x35, (byte) 0x33, (byte) 0xb7, (byte) 0xb0,
        (byte) 0x34, (byte) 0x00, (byte) 0x00
    };

    private static final byte[] GZIP_KNOWN = {
        (byte) 0x1f, (byte) 0x8b, (byte) 0x08, (byte) 0x00, (byte) 0x00, (byte) 0x00, (byte) 0x00,
        (byte) 0x00, (byte) 0x00, (byte) 0xff, (byte) 0xf3, (byte) 0x48, (byte) 0xcd, (byte) 0xc9,
        (byte) 0xc9, (byte) 0x57, (byte) 0x48, (byte) 0x2b, (byte) 0xca, (byte) 0xcf, (byte) 0x55,
        (byte) 0x28, (byte) 0x2e, (byte) 0x49, (byte) 0xcc, (byte) 0x4b, (byte) 0x49, (byte) 0x2c,
        (byte) 0x4a, (byte) 0x51, (byte) 0xa8, (byte) 0xca, (byte) 0xc9, (byte) 0x4c, (byte) 0x52,
        (byte) 0x54, (byte) 0x30, (byte) 0x34, (byte) 0x32, (byte) 0x36, (byte) 0x31, (byte) 0x35,
        (byte) 0x33, (byte) 0xb7, (byte) 0xb0, (byte) 0x34, (byte) 0x00, (byte) 0x00, (byte) 0xd2,
        (byte) 0x09, (byte) 0xed, (byte) 0xce, (byte) 0x24, (byte) 0x00, (byte) 0x00, (byte) 0x00
    };

    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("adler32")) adler32();
        else if (mechanism.equals("deflaterInflaterZlib")) deflaterInflaterZlib();
        else if (mechanism.equals("deflaterInflaterRaw")) deflaterInflaterRaw();
        else if (mechanism.equals("deflaterFlushes")) deflaterFlushes();
        else if (mechanism.equals("deflaterParamsChange")) deflaterParamsChange();
        else if (mechanism.equals("dictionary")) dictionary();
        else if (mechanism.equals("directBuffers")) directBuffers();
        else if (mechanism.equals("gzipRoundTrip")) gzipRoundTrip();
        else if (mechanism.equals("zipArchiveRoundTrip")) zipArchiveRoundTrip();
        else if (mechanism.equals("corruptInput")) corruptInput();
        else if (mechanism.equals("closedState")) closedState();
        else throw new IllegalArgumentException("no mechanism " + mechanism);
    }

    // ---- Shared helpers ----

    private static long crc32Of(byte[] data) {
        CRC32 c = new CRC32();
        c.update(data);
        return c.getValue();
    }

    private static long adler32Of(byte[] data) {
        Adler32 a = new Adler32();
        a.update(data);
        return a.getValue();
    }

    private static byte[] makeRepetitive(int size) {
        byte[] out = new byte[size];
        byte[] pattern = "alpha-beta-gamma-delta-0123456789\n".getBytes(StandardCharsets.UTF_8);
        for (int i = 0; i < size; i++) {
            out[i] = pattern[i % pattern.length];
        }
        return out;
    }

    private static byte[] makePrng(int size, long seed) {
        byte[] out = new byte[size];
        new Random(seed).nextBytes(out);
        return out;
    }

    private static byte[] makeMixed(int size, long seed) {
        byte[] out = new byte[size];
        byte[] rep = makeRepetitive(size / 2);
        byte[] rnd = makePrng(size - rep.length, seed);
        System.arraycopy(rep, 0, out, 0, rep.length);
        System.arraycopy(rnd, 0, out, rep.length, rnd.length);
        return out;
    }

    private static byte[] readAll(InputStream in, int chunkSize) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[chunkSize];
        int n;
        while ((n = in.read(buf)) != -1) {
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    private static byte[] deflateSliced(
            Deflater def, byte[] input, int inSlice, int outSlice) {
        ByteArrayOutputStream compressed = new ByteArrayOutputStream();
        byte[] outBuf = new byte[outSlice];
        int off = 0;
        while (off < input.length) {
            if (def.needsInput()) {
                int len = Math.min(inSlice, input.length - off);
                def.setInput(input, off, len);
                off += len;
            }
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }
        while (!def.needsInput()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
            else break;
        }
        def.finish();
        while (!def.finished()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }
        return compressed.toByteArray();
    }

    private static byte[] inflateSliced(
            Inflater inf, byte[] compressed, int inSlice, int outSlice)
            throws DataFormatException {
        ByteArrayOutputStream plain = new ByteArrayOutputStream();
        byte[] outBuf = new byte[outSlice];
        int off = 0;
        while (!inf.finished()) {
            if (inf.needsInput() && off < compressed.length) {
                int len = Math.min(inSlice, compressed.length - off);
                inf.setInput(compressed, off, len);
                off += len;
            }
            int n = inf.inflate(outBuf);
            if (n > 0) {
                plain.write(outBuf, 0, n);
            } else if (inf.needsInput() && !inf.finished() && off >= compressed.length) {
                throw new IllegalStateException("inflater needs input at EOF");
            }
        }
        return plain.toByteArray();
    }

    private static void drainFlush(
            Deflater def, int flushMode, int outSlice, ByteArrayOutputStream out) {
        byte[] buf = new byte[outSlice];
        while (true) {
            int n = def.deflate(buf, 0, buf.length, flushMode);
            if (n > 0) out.write(buf, 0, n);
            if (n < buf.length) break;
        }
    }

    private static byte[] drainAvailableInflate(Inflater inf, int outSlice)
            throws DataFormatException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[outSlice];
        while (!inf.finished()) {
            int n = inf.inflate(buf);
            if (n > 0) {
                out.write(buf, 0, n);
            } else if (inf.needsInput() || inf.needsDictionary()) {
                break;
            }
        }
        return out.toByteArray();
    }

    // ---- 1. Adler32 ----

    static void adler32() {
        Adler32 a = new Adler32();
        System.out.println("init=" + a.getValue());
        a.update('a');
        System.out.println("after_a=" + a.getValue());
        a.update(0xff);
        long vFF = a.getValue();
        a.reset();
        System.out.println("after_reset=" + a.getValue());

        a.update('a');
        a.update(-1);
        System.out.println("sign_mask_match=" + (a.getValue() == vFF));
        a.update(0x1234);
        System.out.println("high_bits_masked=" + a.getValue());

        byte[] data = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".getBytes(StandardCharsets.UTF_8);
        a.reset();
        a.update(data, 5, 0);
        System.out.println("empty_slice=" + a.getValue());
        a.update(data, 3, 14);
        long sliceVal = a.getValue();
        System.out.println("slice_val=" + sliceVal);

        a.reset();
        a.update(data);
        System.out.println("full_array=" + a.getValue());

        // Heap ByteBuffer slice with arrayOffset > 0 and position > 0.
        ByteBuffer backed = ByteBuffer.wrap(data, 1, 20).slice();
        backed.position(2);
        backed.limit(16);
        a.reset();
        a.update(backed);
        System.out.println(
                "heap_slice arrayOffset="
                        + backed.arrayOffset()
                        + " posAfter="
                        + backed.position()
                        + " limit="
                        + backed.limit()
                        + " match="
                        + (a.getValue() == sliceVal));

        // Direct ByteBuffer with non-zero position and limit < capacity.
        ByteBuffer direct = ByteBuffer.allocateDirect(data.length + 4);
        direct.position(2);
        direct.put(data);
        direct.position(5);
        direct.limit(19);
        a.reset();
        a.update(direct);
        System.out.println(
                "direct posAfter="
                        + direct.position()
                        + " limit="
                        + direct.limit()
                        + " match="
                        + (a.getValue() == sliceVal));

        // Empty ByteBuffer leaves value unchanged.
        long beforeEmpty = a.getValue();
        ByteBuffer emptyBb = ByteBuffer.allocateDirect(8);
        emptyBb.limit(0);
        a.update(emptyBb);
        System.out.println("empty_bb_unchanged=" + (a.getValue() == beforeEmpty));

        checkAdlerBounds(a, data);
    }

    private static void checkAdlerBounds(Adler32 a, byte[] data) {
        try {
            a.update((byte[]) null);
            System.out.println("null_arr=no_throw");
        } catch (NullPointerException e) {
            System.out.println("null_arr=NPE");
        }
        try {
            a.update((byte[]) null, 0, 0);
            System.out.println("null_arr_range=no_throw");
        } catch (NullPointerException e) {
            System.out.println("null_arr_range=NPE");
        }
        try {
            a.update((ByteBuffer) null);
            System.out.println("null_bb=no_throw");
        } catch (NullPointerException e) {
            System.out.println("null_bb=NPE");
        }
        int[][] bad = {
            {-1, 1},
            {0, -1},
            {data.length, 1},
            {2, Integer.MAX_VALUE}
        };
        for (int i = 0; i < bad.length; i++) {
            try {
                a.update(data, bad[i][0], bad[i][1]);
                System.out.println("oob_" + i + "=no_throw");
            } catch (IndexOutOfBoundsException e) {
                System.out.println("oob_" + i + "=IOOBE");
            }
        }
    }

    // ---- 2. Deflater / Inflater (zlib) ----

    static void deflaterInflaterZlib() throws DataFormatException {
        byte[][] payloads = {
            new byte[0],
            "short ASCII zlib round-trip payload 0123456789!".getBytes(StandardCharsets.UTF_8),
            makeRepetitive(48 * 1024),
            makePrng(32 * 1024, 0x5A1B2026L),
            makeMixed(80 * 1024, 0x7069784CL)
        };
        String[] names = {"empty", "short", "repeat48k", "prng32k", "mixed80k"};
        int[] levels = {
            Deflater.NO_COMPRESSION,
            Deflater.BEST_SPEED,
            Deflater.DEFAULT_COMPRESSION,
            Deflater.BEST_COMPRESSION
        };
        int[] strategies = {
            Deflater.DEFAULT_STRATEGY,
            Deflater.FILTERED,
            Deflater.HUFFMAN_ONLY
        };

        for (int level : levels) {
            for (int strategy : strategies) {
                Deflater def = new Deflater(level);
                def.setStrategy(strategy);
                Inflater inf = new Inflater();
                for (int p = 0; p < payloads.length; p++) {
                    byte[] src = payloads[p];
                    byte[] compressed = deflateSliced(def, src, 5, 7);
                    boolean defDone = def.finished() && def.needsInput();
                    long defRead = def.getBytesRead();
                    long defWritten = def.getBytesWritten();
                    int defAdler = def.getAdler();

                    byte[] restored = inflateSliced(inf, compressed, 5, 7);
                    boolean infDone = inf.finished() && inf.needsInput();
                    long infRead = inf.getBytesRead();
                    long infWritten = inf.getBytesWritten();
                    int infAdler = inf.getAdler();

                    boolean ok =
                            defDone
                                    && infDone
                                    && Arrays.equals(src, restored)
                                    && crc32Of(src) == crc32Of(restored)
                                    && defRead == src.length
                                    && infWritten == src.length
                                    && defWritten == compressed.length
                                    && infRead == compressed.length
                                    && defAdler == infAdler
                                    && ((long) defAdler & 0xffffffffL) == adler32Of(src);
                    System.out.println(
                            "lvl="
                                    + level
                                    + " strat="
                                    + strategy
                                    + " "
                                    + names[p]
                                    + " ok="
                                    + ok
                                    + " crc="
                                    + crc32Of(restored)
                                    + " adler="
                                    + ((long) infAdler & 0xffffffffL)
                                    + " read="
                                    + defRead
                                    + " written="
                                    + infWritten);

                    def.reset();
                    inf.reset();
                    if (def.getBytesRead() != 0
                            || def.getBytesWritten() != 0
                            || def.getAdler() != 1
                            || def.finished()
                            || !def.needsInput()) {
                        throw new AssertionError("def.reset() left stale state");
                    }
                    if (inf.getBytesRead() != 0
                            || inf.getBytesWritten() != 0
                            || inf.getAdler() != 1
                            || inf.finished()
                            || !inf.needsInput()) {
                        throw new AssertionError("inf.reset() left stale state");
                    }
                }
                def.end();
                inf.end();
            }
        }

        Inflater known = new Inflater(false);
        byte[] knownOut = inflateSliced(known, ZLIB_KNOWN, 5, 4);
        System.out.println(
                "known_zlib=\""
                        + new String(knownOut, StandardCharsets.UTF_8)
                        + "\" fin="
                        + known.finished()
                        + " read="
                        + known.getBytesRead()
                        + " written="
                        + known.getBytesWritten()
                        + " adler="
                        + ((long) known.getAdler() & 0xffffffffL));
        known.end();

        // 1-byte-in / 1-byte-out streaming and explicit 0-length buffer calls on > 64 KiB payload.
        byte[] big = payloads[4];
        Deflater d1 = new Deflater();
        int zeroDef = d1.deflate(new byte[0]);
        byte[] c1 = deflateSliced(d1, big, 1, 1);
        Inflater i1 = new Inflater();
        int zeroInf = i1.inflate(new byte[0]);
        byte[] r1 = inflateSliced(i1, c1, 1, 1);
        System.out.println(
                "zlib_1x1 eq="
                        + Arrays.equals(big, r1)
                        + " zeroDef="
                        + zeroDef
                        + " zeroInf="
                        + zeroInf
                        + " read="
                        + d1.getBytesRead()
                        + " written="
                        + i1.getBytesWritten()
                        + " adler="
                        + ((long) i1.getAdler() & 0xffffffffL));
        d1.end();
        i1.end();
    }

    // ---- 3. Deflater / Inflater (raw) ----

    static void deflaterInflaterRaw() throws DataFormatException {
        byte[][] payloads = {
            new byte[0],
            "raw deflate payload check".getBytes(StandardCharsets.UTF_8),
            makeRepetitive(48 * 1024),
            makePrng(32 * 1024, 0x13572468L),
            makeMixed(80 * 1024, 0x72617738L)
        };
        String[] names = {"empty", "short", "repeat48k", "prng32k", "mixed80k"};

        Deflater def = new Deflater(6, true);
        Inflater inf = new Inflater(true);
        for (int p = 0; p < payloads.length; p++) {
            byte[] src = payloads[p];
            byte[] compressed = deflateSliced(def, src, 5, 7);
            long defRead = def.getBytesRead();
            long defWritten = def.getBytesWritten();
            int defAdler = def.getAdler();

            byte[] restored = inflateSliced(inf, compressed, 5, 7);
            long infRead = inf.getBytesRead();
            long infWritten = inf.getBytesWritten();
            int infAdler = inf.getAdler();

            boolean ok =
                    def.finished()
                            && def.needsInput()
                            && inf.finished()
                            && inf.needsInput()
                            && Arrays.equals(src, restored)
                            && defRead == src.length
                            && infWritten == src.length
                            && defWritten == compressed.length
                            && infRead == compressed.length;
            System.out.println(
                    "raw "
                            + names[p]
                            + " ok="
                            + ok
                            + " crc="
                            + crc32Of(restored)
                            + " defAdler="
                            + defAdler
                            + " infAdler="
                            + infAdler
                            + " read="
                            + defRead
                            + " written="
                            + infWritten);
            def.reset();
            inf.reset();
        }
        def.end();
        inf.end();

        Inflater known = new Inflater(true);
        byte[] knownOut = inflateSliced(known, RAW_KNOWN, 5, 4);
        System.out.println(
                "known_raw=\""
                        + new String(knownOut, StandardCharsets.UTF_8)
                        + "\" fin="
                        + known.finished()
                        + " read="
                        + known.getBytesRead()
                        + " written="
                        + known.getBytesWritten()
                        + " adler="
                        + known.getAdler());
        known.end();

        // 1-byte-in / 1-byte-out streaming and explicit 0-length buffer calls on > 64 KiB raw payload.
        byte[] bigRaw = payloads[4];
        Deflater d1 = new Deflater(6, true);
        int zeroDef = d1.deflate(new byte[0]);
        byte[] c1 = deflateSliced(d1, bigRaw, 1, 1);
        Inflater i1 = new Inflater(true);
        int zeroInf = i1.inflate(new byte[0]);
        byte[] r1 = inflateSliced(i1, c1, 1, 1);
        System.out.println(
                "raw_1x1 eq="
                        + Arrays.equals(bigRaw, r1)
                        + " zeroDef="
                        + zeroDef
                        + " zeroInf="
                        + zeroInf
                        + " read="
                        + d1.getBytesRead()
                        + " written="
                        + i1.getBytesWritten());
        d1.end();
        i1.end();
    }

    // ---- 4. Deflater flushes ----

    static void deflaterFlushes() throws DataFormatException {
        byte[] part1a = "chunk1-head:".getBytes(StandardCharsets.UTF_8);
        byte[] part1b = "alpha-beta-gamma;".getBytes(StandardCharsets.UTF_8);
        byte[] chunk1 = new byte[part1a.length + part1b.length];
        System.arraycopy(part1a, 0, chunk1, 0, part1a.length);
        System.arraycopy(part1b, 0, chunk1, part1a.length, part1b.length);
        byte[] chunk2 = "chunk2-full-flush:delta-epsilon-zeta;".getBytes(StandardCharsets.UTF_8);
        byte[] chunk3 = "chunk3-finish:omega!".getBytes(StandardCharsets.UTF_8);

        Deflater def = new Deflater();
        Inflater inf = new Inflater();
        ByteArrayOutputStream stageCompressed = new ByteArrayOutputStream();

        // Feed part1a with NO_FLUSH, then part1b and SYNC_FLUSH.
        def.setInput(part1a);
        drainFlush(def, Deflater.NO_FLUSH, 9, stageCompressed);
        def.setInput(part1b);
        drainFlush(def, Deflater.SYNC_FLUSH, 9, stageCompressed);

        byte[] wire1 = stageCompressed.toByteArray();
        stageCompressed.reset();
        inf.setInput(wire1);
        byte[] out1 = drainAvailableInflate(inf, 7);
        System.out.println(
                "stage1=\""
                        + new String(out1, StandardCharsets.UTF_8)
                        + "\" eq="
                        + Arrays.equals(chunk1, out1)
                        + " crc="
                        + crc32Of(out1)
                        + " defFin="
                        + def.finished()
                        + " infFin="
                        + inf.finished());

        // Chunk 2 with FULL_FLUSH.
        def.setInput(chunk2);
        drainFlush(def, Deflater.FULL_FLUSH, 9, stageCompressed);
        byte[] wire2 = stageCompressed.toByteArray();
        stageCompressed.reset();
        inf.setInput(wire2);
        byte[] out2 = drainAvailableInflate(inf, 7);
        System.out.println(
                "stage2=\""
                        + new String(out2, StandardCharsets.UTF_8)
                        + "\" eq="
                        + Arrays.equals(chunk2, out2)
                        + " crc="
                        + crc32Of(out2)
                        + " defFin="
                        + def.finished()
                        + " infFin="
                        + inf.finished());

        // Chunk 3 and finish().
        def.setInput(chunk3);
        def.finish();
        byte[] buf = new byte[9];
        while (!def.finished()) {
            int n = def.deflate(buf, 0, buf.length, Deflater.NO_FLUSH);
            if (n > 0) stageCompressed.write(buf, 0, n);
        }
        byte[] wire3 = stageCompressed.toByteArray();
        inf.setInput(wire3);
        byte[] out3 = drainAvailableInflate(inf, 7);
        long totalLen = (long) chunk1.length + chunk2.length + chunk3.length;
        System.out.println(
                "stage3=\""
                        + new String(out3, StandardCharsets.UTF_8)
                        + "\" eq="
                        + Arrays.equals(chunk3, out3)
                        + " crc="
                        + crc32Of(out3)
                        + " defFin="
                        + def.finished()
                        + " infFin="
                        + inf.finished()
                        + " adlerMatch="
                        + (def.getAdler() == inf.getAdler())
                        + " defRead="
                        + def.getBytesRead()
                        + " infWritten="
                        + inf.getBytesWritten()
                        + " totalMatch="
                        + (def.getBytesRead() == totalLen && inf.getBytesWritten() == totalLen));

        def.end();
        inf.end();

        // Raw mode (nowrap=true) repeated SYNC_FLUSH / FULL_FLUSH checks.
        Deflater rawDef = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        Inflater rawInf = new Inflater(true);
        byte[][] rawChunks = {
            "raw-sync-1;".getBytes(StandardCharsets.UTF_8),
            "raw-full-2;".getBytes(StandardCharsets.UTF_8),
            "raw-sync-3;".getBytes(StandardCharsets.UTF_8),
            "raw-end-4!".getBytes(StandardCharsets.UTF_8)
        };
        int[] rawModes = {Deflater.SYNC_FLUSH, Deflater.FULL_FLUSH, Deflater.SYNC_FLUSH};
        for (int i = 0; i < rawModes.length; i++) {
            rawDef.setInput(rawChunks[i]);
            stageCompressed.reset();
            drainFlush(rawDef, rawModes[i], 9, stageCompressed);
            rawInf.setInput(stageCompressed.toByteArray());
            byte[] part = drainAvailableInflate(rawInf, 4);
            System.out.println(
                    "raw_flush["
                            + i
                            + "]=\""
                            + new String(part, StandardCharsets.UTF_8)
                            + "\" eq="
                            + Arrays.equals(rawChunks[i], part)
                            + " crc="
                            + crc32Of(part)
                            + " infFin="
                            + rawInf.finished());
        }
        rawDef.setInput(rawChunks[3]);
        rawDef.finish();
        stageCompressed.reset();
        while (!rawDef.finished()) {
            int n = rawDef.deflate(buf, 0, buf.length, Deflater.NO_FLUSH);
            if (n > 0) stageCompressed.write(buf, 0, n);
        }
        rawInf.setInput(stageCompressed.toByteArray());
        byte[] rawLast = drainAvailableInflate(rawInf, 4);
        System.out.println(
                "raw_flush[3]=\""
                        + new String(rawLast, StandardCharsets.UTF_8)
                        + "\" eq="
                        + Arrays.equals(rawChunks[3], rawLast)
                        + " defFin="
                        + rawDef.finished()
                        + " infFin="
                        + rawInf.finished());
        rawDef.end();
        rawInf.end();
    }

    // ---- 5. Deflater mid-stream params change ----

    static void deflaterParamsChange() throws DataFormatException {
        byte[] chunkA = makeRepetitive(4096);
        byte[] chunkB = new byte[4096];
        for (int i = 0; i < chunkB.length; i++) {
            chunkB[i] = (byte) ((i * 7) & 0xff);
        }
        byte[] chunkC = makePrng(4096, 0xCAFEBABEL);

        byte[] expected = new byte[chunkA.length + chunkB.length + chunkC.length];
        System.arraycopy(chunkA, 0, expected, 0, chunkA.length);
        System.arraycopy(chunkB, 0, expected, chunkA.length, chunkB.length);
        System.arraycopy(chunkC, 0, expected, chunkA.length + chunkB.length, chunkC.length);

        Deflater def = new Deflater(Deflater.NO_COMPRESSION);
        ByteArrayOutputStream compressed = new ByteArrayOutputStream();
        byte[] outBuf = new byte[11];

        def.setInput(chunkA);
        while (!def.needsInput()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }

        def.setLevel(Deflater.BEST_COMPRESSION);
        def.setStrategy(Deflater.FILTERED);
        def.setInput(chunkB);
        while (!def.needsInput()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }

        def.setStrategy(Deflater.HUFFMAN_ONLY);
        def.setInput(chunkC);
        while (!def.needsInput()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }

        def.finish();
        while (!def.finished()) {
            int n = def.deflate(outBuf);
            if (n > 0) compressed.write(outBuf, 0, n);
        }

        Inflater inf = new Inflater();
        byte[] restored = inflateSliced(inf, compressed.toByteArray(), 13, 11);
        System.out.println(
                "params eq="
                        + Arrays.equals(expected, restored)
                        + " len="
                        + restored.length
                        + " crc="
                        + crc32Of(restored)
                        + " adlerMatch="
                        + (def.getAdler() == inf.getAdler())
                        + " adler="
                        + ((long) def.getAdler() & 0xffffffffL)
                        + " defRead="
                        + def.getBytesRead()
                        + " infWritten="
                        + inf.getBytesWritten());

        try {
            def.setLevel(10);
            System.out.println("bad_level=no_throw");
        } catch (IllegalArgumentException e) {
            System.out.println("bad_level=IAE");
        }
        try {
            def.setStrategy(99);
            System.out.println("bad_strategy=no_throw");
        } catch (IllegalArgumentException e) {
            System.out.println("bad_strategy=IAE");
        }

        def.end();
        inf.end();

        // Pending-input param change with a 1-byte output buffer to stress setParams drain.
        // 80 KiB incompressible PRNG exceeds zlib's internal pending buffer at NO_COMPRESSION,
        // so deflate(new byte[1]) leaves unconsumed input (!needsInput()) across setLevel/setStrategy.
        byte[] pendA = makePrng(80 * 1024, 0x11223344L);
        byte[] pendB = makePrng(80 * 1024, 0x55667788L);
        byte[] pendExpected = new byte[pendA.length + pendB.length + chunkC.length];
        System.arraycopy(pendA, 0, pendExpected, 0, pendA.length);
        System.arraycopy(pendB, 0, pendExpected, pendA.length, pendB.length);
        System.arraycopy(chunkC, 0, pendExpected, pendA.length + pendB.length, chunkC.length);

        Deflater pendDef = new Deflater(Deflater.NO_COMPRESSION);
        ByteArrayOutputStream pendCompressed = new ByteArrayOutputStream();
        byte[] oneByte = new byte[1];
        pendDef.setInput(pendA);
        for (int i = 0; i < 5 && !pendDef.needsInput(); i++) {
            int n = pendDef.deflate(oneByte);
            if (n > 0) pendCompressed.write(oneByte, 0, n);
        }
        boolean stillPending1 = !pendDef.needsInput();
        pendDef.setLevel(Deflater.BEST_COMPRESSION);
        pendDef.setStrategy(Deflater.FILTERED);
        while (!pendDef.needsInput()) {
            int n = pendDef.deflate(oneByte);
            if (n > 0) pendCompressed.write(oneByte, 0, n);
        }
        pendDef.setInput(pendB);
        for (int i = 0; i < 5 && !pendDef.needsInput(); i++) {
            int n = pendDef.deflate(oneByte);
            if (n > 0) pendCompressed.write(oneByte, 0, n);
        }
        boolean stillPending2 = !pendDef.needsInput();
        pendDef.setStrategy(Deflater.HUFFMAN_ONLY);
        while (!pendDef.needsInput()) {
            int n = pendDef.deflate(oneByte);
            if (n > 0) pendCompressed.write(oneByte, 0, n);
        }
        pendDef.setInput(chunkC);
        pendDef.finish();
        while (!pendDef.finished()) {
            int n = pendDef.deflate(oneByte);
            if (n > 0) pendCompressed.write(oneByte, 0, n);
        }

        Inflater pendInf = new Inflater();
        byte[] pendRestored = inflateSliced(pendInf, pendCompressed.toByteArray(), 1, 1);
        System.out.println(
                "pending_1b pend1="
                        + stillPending1
                        + " pend2="
                        + stillPending2
                        + " eq="
                        + Arrays.equals(pendExpected, pendRestored)
                        + " crc="
                        + crc32Of(pendRestored)
                        + " adlerMatch="
                        + (pendDef.getAdler() == pendInf.getAdler())
                        + " read="
                        + pendDef.getBytesRead()
                        + " written="
                        + pendInf.getBytesWritten());
        pendDef.end();
        pendInf.end();
    }

    // ---- 6. Dictionary ----

    static void dictionary() throws DataFormatException {
        byte[] dict =
                "HEADER-KEY:shared-dictionary-vocabulary:FOOTER-VAL".getBytes(
                        StandardCharsets.UTF_8);
        byte[] wrongDict =
                "WRONG-KEY:completely-different-vocabulary:WRONG".getBytes(
                        StandardCharsets.UTF_8);
        byte[] payload =
                "HEADER-KEY:shared-dictionary-vocabulary:payload-1;HEADER-KEY:shared-dictionary-vocabulary:FOOTER-VAL"
                        .getBytes(StandardCharsets.UTF_8);

        // Zlib mode (nowrap=false).
        Deflater def = new Deflater();
        def.setDictionary(dict);
        byte[] compressed = deflateSliced(def, payload, 7, 9);
        int defAdler = def.getAdler();

        Inflater inf = new Inflater();
        inf.setInput(compressed);
        byte[] outBuf = new byte[11];
        int first = inf.inflate(outBuf);
        int dictAdler = inf.getAdler();
        System.out.println(
                "zlib first="
                        + first
                        + " needsDict="
                        + inf.needsDictionary()
                        + " fin="
                        + inf.finished()
                        + " dictAdlerMatch="
                        + (((long) dictAdler & 0xffffffffL) == adler32Of(dict)));

        try {
            inf.setDictionary(wrongDict);
            System.out.println("wrong_dict=no_throw");
        } catch (IllegalArgumentException e) {
            System.out.println("wrong_dict=IAE stillNeedsDict=" + inf.needsDictionary());
        }

        inf.setDictionary(dict);
        int afterDictAdler = inf.getAdler();
        int zeroAfterDict = inf.inflate(new byte[0]);
        System.out.println(
                "after_right_dict needsDict="
                        + inf.needsDictionary()
                        + " adler="
                        + ((long) afterDictAdler & 0xffffffffL)
                        + " zeroInf="
                        + zeroAfterDict
                        + " adlerAfterZero="
                        + ((long) inf.getAdler() & 0xffffffffL));
        byte[] rest = drainAvailableInflate(inf, 11);
        System.out.println(
                "zlib eq="
                        + Arrays.equals(payload, rest)
                        + " fin="
                        + inf.finished()
                        + " defAdler="
                        + ((long) defAdler & 0xffffffffL)
                        + " infAdler="
                        + ((long) inf.getAdler() & 0xffffffffL));

        // Reuse the same Deflater and Inflater via reset() with offset/length dictionary slices.
        byte[] paddedDict = new byte[dict.length + 7];
        System.arraycopy(dict, 0, paddedDict, 4, dict.length);
        def.reset();
        def.setDictionary(paddedDict, 4, dict.length);
        byte[] compressed2 = deflateSliced(def, payload, 11, 13);
        def.end();

        inf.reset();
        inf.setInput(compressed2);
        inf.inflate(outBuf);
        boolean needed2 = inf.needsDictionary();
        inf.setDictionary(paddedDict, 4, dict.length);
        byte[] rest2 = drainAvailableInflate(inf, 13);
        System.out.println(
                "zlib_slice_dict needed="
                        + needed2
                        + " eq="
                        + Arrays.equals(payload, rest2)
                        + " fin="
                        + inf.finished());
        inf.end();

        // Raw mode (nowrap=true).
        Deflater rawDef = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        rawDef.setDictionary(dict);
        byte[] rawCompressed = deflateSliced(rawDef, payload, 7, 9);
        int rawDefAdler = rawDef.getAdler();
        rawDef.end();

        Inflater rawInf = new Inflater(true);
        rawInf.setInput(rawCompressed);
        rawInf.setDictionary(dict);
        int rawAfterDictAdler = rawInf.getAdler();
        int rawZeroAfterDict = rawInf.inflate(new byte[0]);
        byte[] rawRest = drainAvailableInflate(rawInf, 11);
        System.out.println(
                "raw eq="
                        + Arrays.equals(payload, rawRest)
                        + " needsDict="
                        + rawInf.needsDictionary()
                        + " fin="
                        + rawInf.finished()
                        + " rawDefAdler="
                        + rawDefAdler
                        + " rawDictAdler="
                        + rawAfterDictAdler
                        + " rawZeroInf="
                        + rawZeroAfterDict
                        + " adler="
                        + rawInf.getAdler());
        rawInf.end();
    }

    // ---- 7. Direct ByteBuffers ----

    static void directBuffers() {
        byte[] backing = makePrng(64, 0x99887766L);
        ByteBuffer direct = ByteBuffer.allocateDirect(64);
        direct.put(backing);
        direct.position(7);
        direct.limit(45);
        ByteBuffer slice = direct.slice();
        slice.position(3);
        slice.limit(29); // corresponds to backing[10..36)

        CRC32 crcRef = new CRC32();
        crcRef.update(backing, 10, 26);
        Adler32 adlRef = new Adler32();
        adlRef.update(backing, 10, 26);

        CRC32 crc = new CRC32();
        Adler32 adl = new Adler32();
        crc.update(slice);
        System.out.println(
                "crc_slice pos="
                        + slice.position()
                        + " limit="
                        + slice.limit()
                        + " match="
                        + (crc.getValue() == crcRef.getValue())
                        + " val="
                        + crc.getValue());

        slice.position(3);
        adl.update(slice);
        System.out.println(
                "adl_slice pos="
                        + slice.position()
                        + " limit="
                        + slice.limit()
                        + " match="
                        + (adl.getValue() == adlRef.getValue())
                        + " val="
                        + adl.getValue());

        ByteBuffer ro = direct.asReadOnlyBuffer();
        ro.position(10);
        ro.limit(36);
        crc.reset();
        adl.reset();
        crc.update(ro);
        ro.position(10);
        adl.update(ro);
        System.out.println(
                "ro pos="
                        + ro.position()
                        + " crcMatch="
                        + (crc.getValue() == crcRef.getValue())
                        + " adlMatch="
                        + (adl.getValue() == adlRef.getValue()));

        // Empty direct slice leaves checksums unchanged.
        long crcBefore = crc.getValue();
        long adlBefore = adl.getValue();
        ByteBuffer empty = ByteBuffer.allocateDirect(16);
        empty.position(5);
        empty.limit(5);
        crc.update(empty);
        adl.update(empty);
        System.out.println(
                "empty_unchanged="
                        + (crc.getValue() == crcBefore && adl.getValue() == adlBefore));

        try {
            crc.update((ByteBuffer) null);
            System.out.println("crc_null=no_throw");
        } catch (NullPointerException e) {
            System.out.println("crc_null=NPE");
        }
        try {
            adl.update((ByteBuffer) null);
            System.out.println("adl_null=no_throw");
        } catch (NullPointerException e) {
            System.out.println("adl_null=NPE");
        }
    }

    // ---- 8. GZIP round-trip ----

    static void gzipRoundTrip() throws IOException {
        byte[][] payloads = {
            new byte[0],
            "gzip payload check 12345".getBytes(StandardCharsets.UTF_8),
            makeRepetitive(48 * 1024),
            makePrng(32 * 1024, 0x475A4950L)
        };
        String[] names = {"empty", "short", "repeat48k", "prng32k"};

        for (int i = 0; i < payloads.length; i++) {
            byte[] src = payloads[i];
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            try (GZIPOutputStream gz = new GZIPOutputStream(baos)) {
                gz.write(src);
            }
            byte[] restored;
            try (GZIPInputStream gis =
                    new GZIPInputStream(new ByteArrayInputStream(baos.toByteArray()), 64)) {
                restored = readAll(gis, 19);
            }
            System.out.println(
                    "gzip "
                            + names[i]
                            + " eq="
                            + Arrays.equals(src, restored)
                            + " len="
                            + restored.length
                            + " crc="
                            + crc32Of(restored));
        }

        // syncFlush=true with mid-stream flush.
        byte[] partA = "first-gzip-part;".getBytes(StandardCharsets.UTF_8);
        byte[] partB = "second-gzip-part!".getBytes(StandardCharsets.UTF_8);
        ByteArrayOutputStream syncBaos = new ByteArrayOutputStream();
        try (GZIPOutputStream gz = new GZIPOutputStream(syncBaos, 512, true)) {
            gz.write(partA);
            gz.flush();
            gz.write(partB);
        }
        byte[] syncRestored;
        try (GZIPInputStream gis =
                new GZIPInputStream(new ByteArrayInputStream(syncBaos.toByteArray()))) {
            syncRestored = readAll(gis, 13);
        }
        System.out.println(
                "gzip_sync=\""
                        + new String(syncRestored, StandardCharsets.UTF_8)
                        + "\" crc="
                        + crc32Of(syncRestored));

        try (GZIPInputStream known = new GZIPInputStream(new ByteArrayInputStream(GZIP_KNOWN))) {
            byte[] knownOut = readAll(known, 7);
            System.out.println(
                    "known_gzip=\""
                            + new String(knownOut, StandardCharsets.UTF_8)
                            + "\" len="
                            + knownOut.length
                            + " crc="
                            + crc32Of(knownOut));
        }
    }

    // ---- 9. Zip archive round-trip ----

    static void zipArchiveRoundTrip() throws IOException {
        byte[] emptyData = new byte[0];
        byte[] shortData = "hello zip archive entry!".getBytes(StandardCharsets.UTF_8);
        byte[] repeatData = makeRepetitive(48 * 1024);
        byte[] randomData = makePrng(32 * 1024, 0x5A495021L);
        byte[] storedData = "stored-entry-without-compression-0123456789".getBytes(
                StandardCharsets.UTF_8);

        String[] names = {
            "dir/",
            "dir/empty.txt",
            "hello.txt",
            "repeat.txt",
            "random.bin",
            "stored.dat"
        };
        byte[][] expected = {
            emptyData,
            emptyData,
            shortData,
            repeatData,
            randomData,
            storedData
        };

        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        try (ZipOutputStream zos = new ZipOutputStream(baos)) {
            ZipEntry dir = new ZipEntry("dir/");
            dir.setMethod(ZipEntry.STORED);
            dir.setSize(0);
            dir.setCompressedSize(0);
            dir.setCrc(0);
            zos.putNextEntry(dir);
            zos.closeEntry();

            for (int i = 1; i <= 4; i++) {
                ZipEntry e = new ZipEntry(names[i]);
                e.setMethod(ZipEntry.DEFLATED);
                zos.putNextEntry(e);
                zos.write(expected[i]);
                zos.closeEntry();
            }

            ZipEntry stored = new ZipEntry("stored.dat");
            stored.setMethod(ZipEntry.STORED);
            stored.setSize(storedData.length);
            stored.setCompressedSize(storedData.length);
            stored.setCrc(crc32Of(storedData));
            zos.putNextEntry(stored);
            zos.write(storedData);
            zos.closeEntry();
        }

        try (ZipInputStream zis =
                new ZipInputStream(new ByteArrayInputStream(baos.toByteArray()))) {
            int idx = 0;
            ZipEntry entry;
            while ((entry = zis.getNextEntry()) != null) {
                byte[] actual = readAll(zis, 17);
                zis.closeEntry();
                System.out.println(
                        "entry["
                                + idx
                                + "] name="
                                + entry.getName()
                                + " isDir="
                                + entry.isDirectory()
                                + " method="
                                + entry.getMethod()
                                + " size="
                                + entry.getSize()
                                + " crc="
                                + entry.getCrc()
                                + " eq="
                                + Arrays.equals(expected[idx], actual));
                idx++;
            }
            System.out.println("count=" + idx);
        }
    }

    // ---- 10. Corrupt / truncated input ----

    private static void printInflaterState(String label, Inflater inf, String status) {
        System.out.println(
                label
                        + " status="
                        + status
                        + " read="
                        + inf.getBytesRead()
                        + " written="
                        + inf.getBytesWritten()
                        + " rem="
                        + inf.getRemaining()
                        + " adler="
                        + ((long) inf.getAdler() & 0xffffffffL)
                        + " fin="
                        + inf.finished());
    }

    private static void runCorrupt(String label, boolean nowrap, byte[] badInput) {
        Inflater inf = new Inflater(nowrap);
        inf.setInput(badInput);
        byte[] out = new byte[64];
        try {
            while (true) {
                int n = inf.inflate(out);
                if (n == 0) {
                    if (inf.finished()) {
                        printInflaterState(label, inf, "finished");
                        break;
                    }
                    if (inf.needsInput()) {
                        printInflaterState(label, inf, "needsInput");
                        break;
                    }
                    if (inf.needsDictionary()) {
                        printInflaterState(label, inf, "needsDictionary");
                        break;
                    }
                }
            }
        } catch (DataFormatException e) {
            printInflaterState(label, inf, "DFE");
        } finally {
            inf.end();
        }
    }

    static void corruptInput() {
        // 1. Truncated zlib stream (only first 12 bytes of ZLIB_KNOWN).
        runCorrupt("truncated_zlib", false, Arrays.copyOf(ZLIB_KNOWN, 12));

        // 2. Invalid zlib CMF/FCHECK header.
        runCorrupt(
                "bad_header",
                false,
                new byte[] {0x78, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01});

        // 3. Wrong Adler-32 trailer (flip bit in last byte of ZLIB_KNOWN).
        byte[] badTrailer = Arrays.copyOf(ZLIB_KNOWN, ZLIB_KNOWN.length);
        badTrailer[badTrailer.length - 1] ^= 0x01;
        runCorrupt("bad_trailer", false, badTrailer);

        // 4. Invalid deflate block type (btype=11) inside valid zlib header.
        runCorrupt(
                "bad_zlib_block",
                false,
                new byte[] {0x78, (byte) 0x9c, 0x06, 0x00, 0x00, 0x00});

        // 5. Invalid dynamic Huffman code-length table inside valid zlib header.
        runCorrupt(
                "bad_dynamic_huffman",
                false,
                new byte[] {0x78, (byte) 0x9c, (byte) 0x85, (byte) 0xff, (byte) 0xff, 0x00});

        // 6. Corrupt raw stream (reserved block type 11, and invalid stored block complement).
        runCorrupt("bad_raw_block", true, new byte[] {0x06, 0x00});
        runCorrupt(
                "bad_raw_stored_len",
                true,
                new byte[] {0x01, 0x05, 0x00, 0x00, 0x00});

        // 7. Emit >0 valid bytes and hit a corrupt deflate token in the same inflate() call.
        byte[] partialThenCorruptZlib = {
            0x78,
            (byte) 0x9c,
            0x00,
            0x05,
            0x00,
            (byte) 0xfa,
            (byte) 0xff,
            0x68,
            0x65,
            0x6c,
            0x6c,
            0x6f,
            0x07,
            0x00
        };
        runCorrupt("partial_then_corrupt_zlib", false, partialThenCorruptZlib);

        byte[] partialThenCorruptRaw = {
            0x00, 0x05, 0x00, (byte) 0xfa, (byte) 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x07, 0x00
        };
        runCorrupt("partial_then_corrupt_raw", true, partialThenCorruptRaw);
    }

    // ---- 11. Closed state ----

    static void closedState() throws IOException, DataFormatException {
        Deflater def = new Deflater();
        def.end();
        def.end();
        System.out.println("def_double_end=ok");

        byte[] buf = new byte[16];
        for (int i = 0; i < 6; i++) {
            String op;
            try {
                if (i == 0) {
                    op = "deflate";
                    def.deflate(buf);
                } else if (i == 1) {
                    op = "deflateFlush";
                    def.deflate(buf, 0, buf.length, Deflater.NO_FLUSH);
                } else if (i == 2) {
                    op = "reset";
                    def.reset();
                } else if (i == 3) {
                    op = "getBytesRead";
                    def.getBytesRead();
                } else if (i == 4) {
                    op = "getBytesWritten";
                    def.getBytesWritten();
                } else {
                    op = "getAdler";
                    def.getAdler();
                }
                System.out.println("def_" + op + "=no_throw");
            } catch (IllegalStateException e) {
                System.out.println("def_op" + i + "=ISE");
            }
        }

        Inflater inf = new Inflater();
        inf.end();
        inf.end();
        System.out.println("inf_double_end=ok");

        for (int i = 0; i < 6; i++) {
            String op;
            try {
                if (i == 0) {
                    op = "inflate";
                    inf.inflate(buf);
                } else if (i == 1) {
                    op = "inflateSlice";
                    inf.inflate(buf, 0, buf.length);
                } else if (i == 2) {
                    op = "reset";
                    inf.reset();
                } else if (i == 3) {
                    op = "getBytesRead";
                    inf.getBytesRead();
                } else if (i == 4) {
                    op = "getBytesWritten";
                    inf.getBytesWritten();
                } else {
                    op = "getAdler";
                    inf.getAdler();
                }
                System.out.println("inf_" + op + "=no_throw");
            } catch (IllegalStateException e) {
                System.out.println("inf_op" + i + "=ISE");
            }
        }

        // Stream wrappers: idempotent close(), then post-close write/read throws IOException.
        ByteArrayOutputStream dosOut = new ByteArrayOutputStream();
        DeflaterOutputStream dos = new DeflaterOutputStream(dosOut);
        dos.write("abc".getBytes(StandardCharsets.UTF_8));
        dos.close();
        dos.close();
        try {
            dos.write('x');
            System.out.println("dos_write_closed=no_throw");
        } catch (IOException e) {
            System.out.println("dos_write_closed=IOE");
        }

        InflaterInputStream iis =
                new InflaterInputStream(new ByteArrayInputStream(dosOut.toByteArray()));
        iis.close();
        iis.close();
        try {
            iis.read();
            System.out.println("iis_read_closed=no_throw");
        } catch (IOException e) {
            System.out.println("iis_read_closed=IOE");
        }

        ByteArrayOutputStream gzOut = new ByteArrayOutputStream();
        GZIPOutputStream gz = new GZIPOutputStream(gzOut);
        gz.write("abc".getBytes(StandardCharsets.UTF_8));
        gz.close();
        gz.close();
        try {
            gz.write('x');
            System.out.println("gz_write_closed=no_throw");
        } catch (IOException e) {
            System.out.println("gz_write_closed=IOE");
        }

        GZIPInputStream gis = new GZIPInputStream(new ByteArrayInputStream(gzOut.toByteArray()));
        gis.close();
        gis.close();
        try {
            gis.read();
            System.out.println("gis_read_closed=no_throw");
        } catch (IOException e) {
            System.out.println("gis_read_closed=IOE");
        }

        ByteArrayOutputStream zosOut = new ByteArrayOutputStream();
        ZipOutputStream zos = new ZipOutputStream(zosOut);
        zos.putNextEntry(new ZipEntry("a.txt"));
        zos.write("abc".getBytes(StandardCharsets.UTF_8));
        zos.close();
        zos.close();
        try {
            zos.putNextEntry(new ZipEntry("b.txt"));
            System.out.println("zos_put_closed=no_throw");
        } catch (IOException e) {
            System.out.println("zos_put_closed=IOE");
        }

        ZipInputStream zis = new ZipInputStream(new ByteArrayInputStream(zosOut.toByteArray()));
        zis.close();
        zis.close();
        try {
            zis.getNextEntry();
            System.out.println("zis_next_closed=no_throw");
        } catch (IOException e) {
            System.out.println("zis_next_closed=IOE");
        }
    }
}
