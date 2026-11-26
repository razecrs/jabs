// JDK 11+ ByteBuffer overloads of Deflater and Inflater for tests/zip.rs (`ZipBufferCases
// <mechanism>`). Kept separate from Zip.java so Zip.java can stay on `--release 8` while this
// fixture compiles with `--release 21`. Never print implementation-dependent compressed sizes.
import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.ReadOnlyBufferException;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Random;
import java.util.zip.Adler32;
import java.util.zip.CRC32;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

class ZipBufferCases {
    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("directByteBuffers")) {
            directByteBuffers();
        } else {
            throw new IllegalArgumentException("no mechanism " + mechanism);
        }
    }

    private static long crc32Of(byte[] data) {
        CRC32 c = new CRC32();
        c.update(data);
        return c.getValue();
    }

    private static byte[] makeMixed(int size, long seed) {
        byte[] out = new byte[size];
        byte[] pat = "buffer-zip-pattern-0123456789\n".getBytes(StandardCharsets.UTF_8);
        int half = size / 2;
        for (int i = 0; i < half; i++) {
            out[i] = pat[i % pat.length];
        }
        byte[] rnd = new byte[size - half];
        new Random(seed).nextBytes(rnd);
        System.arraycopy(rnd, 0, out, half, rnd.length);
        return out;
    }

    private static ByteBuffer allocBuf(boolean direct, int cap) {
        return direct ? ByteBuffer.allocateDirect(cap) : ByteBuffer.allocate(cap);
    }

    // Returns a sliced buffer with arrayOffset > 0 (when heap), non-zero position, and
    // remaining == len, leaving guard bytes before and after the slice untouched.
    private static ByteBuffer sliceWithGuards(boolean direct, byte[] src, int off, int len) {
        int lead = 5;
        int innerLead = 3;
        int trail = 4;
        ByteBuffer base = allocBuf(direct, lead + innerLead + len + trail);
        for (int i = 0; i < base.capacity(); i++) {
            base.put(i, (byte) 0x5a);
        }
        base.position(lead);
        base.limit(lead + innerLead + len + 2);
        ByteBuffer sliced = base.slice();
        sliced.position(innerLead);
        for (int i = 0; i < len; i++) {
            sliced.put(innerLead + i, src[off + i]);
        }
        sliced.position(innerLead);
        sliced.limit(innerLead + len);
        return sliced;
    }

    private static byte[] roundTripBuffers(
            boolean nowrap,
            boolean defInDirect,
            boolean defOutDirect,
            boolean infInDirect,
            boolean infOutDirect,
            byte[] payload)
            throws DataFormatException {
        Deflater def = new Deflater(Deflater.DEFAULT_COMPRESSION, nowrap);
        ByteArrayOutputStream compressed = new ByteArrayOutputStream();
        int inSlice = 17;
        int outCap = 19;
        int off = 0;
        ByteBuffer defOut = sliceWithGuards(defOutDirect, new byte[outCap], 0, outCap);
        int outStart = defOut.position();
        int outLimit = defOut.limit();

        while (off < payload.length) {
            if (def.needsInput()) {
                int chunk = Math.min(inSlice, payload.length - off);
                ByteBuffer defIn = sliceWithGuards(defInDirect, payload, off, chunk);
                off += chunk;
                def.setInput(defIn);
            }
            defOut.position(outStart);
            defOut.limit(outLimit);
            int n = def.deflate(defOut);
            if (defOut.position() != outStart + n) {
                throw new AssertionError("defOut position mismatch");
            }
            for (int i = 0; i < n; i++) {
                compressed.write(defOut.get(outStart + i));
            }
        }
        while (!def.needsInput()) {
            defOut.position(outStart);
            defOut.limit(outLimit);
            int n = def.deflate(defOut);
            if (n == 0) break;
            for (int i = 0; i < n; i++) {
                compressed.write(defOut.get(outStart + i));
            }
        }
        def.finish();
        while (!def.finished()) {
            defOut.position(outStart);
            defOut.limit(outLimit);
            int n = def.deflate(defOut);
            if (defOut.position() != outStart + n) {
                throw new AssertionError("defOut finish position mismatch");
            }
            for (int i = 0; i < n; i++) {
                compressed.write(defOut.get(outStart + i));
            }
        }

        byte[] wire = compressed.toByteArray();
        if (def.getBytesRead() != payload.length || def.getBytesWritten() != wire.length) {
            throw new AssertionError("def counts mismatch");
        }
        int defAdler = def.getAdler();
        def.end();

        Inflater inf = new Inflater(nowrap);
        ByteArrayOutputStream plain = new ByteArrayOutputStream();
        int infOff = 0;
        ByteBuffer infOut = sliceWithGuards(infOutDirect, new byte[outCap], 0, outCap);
        int infOutStart = infOut.position();
        int infOutLimit = infOut.limit();

        while (!inf.finished()) {
            if (inf.needsInput() && infOff < wire.length) {
                int chunk = Math.min(inSlice, wire.length - infOff);
                ByteBuffer infIn = sliceWithGuards(infInDirect, wire, infOff, chunk);
                infOff += chunk;
                inf.setInput(infIn);
            }
            infOut.position(infOutStart);
            infOut.limit(infOutLimit);
            int n = inf.inflate(infOut);
            if (infOut.position() != infOutStart + n) {
                throw new AssertionError("infOut position mismatch");
            }
            if (n > 0) {
                for (int i = 0; i < n; i++) {
                    plain.write(infOut.get(infOutStart + i));
                }
            } else if (inf.needsInput() && !inf.finished() && infOff >= wire.length) {
                throw new IllegalStateException("inflater needs input at EOF");
            }
        }
        byte[] restored = plain.toByteArray();
        if (inf.getBytesRead() != wire.length || inf.getBytesWritten() != restored.length) {
            throw new AssertionError("inf counts mismatch");
        }
        if (!nowrap && inf.getAdler() != defAdler) {
            throw new AssertionError("adler mismatch");
        }
        inf.end();
        return restored;
    }

    static void directByteBuffers() throws DataFormatException {
        byte[] payload = makeMixed(8 * 1024 + 13, 0x42554646L);

        // Cover all 4 (heap/direct input x heap/direct output) combinations in zlib and raw modes.
        boolean[] modes = {false, true};
        for (boolean nowrap : modes) {
            for (boolean inDirect : modes) {
                for (boolean outDirect : modes) {
                    byte[] restored =
                            roundTripBuffers(
                                    nowrap, inDirect, outDirect, inDirect, outDirect, payload);
                    System.out.println(
                            "nowrap="
                                    + nowrap
                                    + " inDir="
                                    + inDirect
                                    + " outDir="
                                    + outDirect
                                    + " eq="
                                    + Arrays.equals(payload, restored)
                                    + " len="
                                    + restored.length
                                    + " crc="
                                    + crc32Of(restored));
                }
            }
        }

        // Cross-combination: direct Deflater -> heap Inflater, and flush overload with direct ByteBuffer.
        byte[] cross = roundTripBuffers(false, true, false, false, true, payload);
        System.out.println("cross eq=" + Arrays.equals(payload, cross));

        Deflater flushDef = new Deflater();
        byte[] p1 = "direct-flush-part1;".getBytes(StandardCharsets.UTF_8);
        byte[] p2 = "direct-flush-part2!".getBytes(StandardCharsets.UTF_8);
        ByteBuffer dIn1 = sliceWithGuards(true, p1, 0, p1.length);
        ByteBuffer dOut = ByteBuffer.allocateDirect(128);
        flushDef.setInput(dIn1);
        int fn1 = flushDef.deflate(dOut, Deflater.SYNC_FLUSH);
        boolean consumed1 = dIn1.remaining() == 0;
        dOut.flip();
        byte[] w1 = new byte[dOut.remaining()];
        dOut.get(w1);

        Inflater flushInf = new Inflater();
        ByteBuffer iIn1 = ByteBuffer.allocateDirect(w1.length);
        iIn1.put(w1).flip();
        flushInf.setInput(iIn1);
        ByteBuffer iOut1 = ByteBuffer.allocateDirect(64);
        int in1 = flushInf.inflate(iOut1);
        iOut1.flip();
        byte[] r1 = new byte[in1];
        iOut1.get(r1);
        System.out.println(
                "flush_direct consumed1="
                        + consumed1
                        + " fn1Positive="
                        + (fn1 > 0)
                        + " part1=\""
                        + new String(r1, StandardCharsets.UTF_8)
                        + "\" infFin="
                        + flushInf.finished());

        ByteBuffer dIn2 = sliceWithGuards(true, p2, 0, p2.length);
        dOut.clear();
        flushDef.setInput(dIn2);
        flushDef.finish();
        while (!flushDef.finished()) {
            flushDef.deflate(dOut);
        }
        dOut.flip();
        ByteBuffer iIn2 = ByteBuffer.allocateDirect(dOut.remaining());
        iIn2.put(dOut).flip();
        flushInf.setInput(iIn2);
        ByteBuffer iOut2 = ByteBuffer.allocateDirect(64);
        while (!flushInf.finished()) {
            flushInf.inflate(iOut2);
        }
        iOut2.flip();
        byte[] r2 = new byte[iOut2.remaining()];
        iOut2.get(r2);
        System.out.println(
                "flush_direct part2=\""
                        + new String(r2, StandardCharsets.UTF_8)
                        + "\" infFin="
                        + flushInf.finished());
        flushDef.end();
        flushInf.end();

        // Dictionary via ByteBuffer (direct and heap slice), including wrong-dict IAE.
        checkBufferDictionary(true);
        checkBufferDictionary(false);

        // Read-only buffers: allowed as input/dictionary, rejected as output.
        checkReadOnlyAndNull(true);
        checkReadOnlyAndNull(false);

        // Corrupt input via direct ByteBuffer.
        byte[] bad = {0x78, (byte) 0x9c, 0x00, 0x03, 0x00, (byte) 0xfc, (byte) 0xff, 0x61, 0x62, 0x63, 0x07, 0x00};
        ByteBuffer badDirect = ByteBuffer.allocateDirect(bad.length);
        badDirect.put(bad).flip();
        ByteBuffer outDirect = ByteBuffer.allocateDirect(32);
        Inflater badInf = new Inflater();
        badInf.setInput(badDirect);
        try {
            badInf.inflate(outDirect);
            System.out.println("corrupt_direct=no_throw");
        } catch (DataFormatException e) {
            System.out.println(
                    "corrupt_direct=DFE inPos="
                            + badDirect.position()
                            + " outPos="
                            + outDirect.position()
                            + " read="
                            + badInf.getBytesRead()
                            + " written="
                            + badInf.getBytesWritten()
                            + " rem="
                            + badInf.getRemaining()
                            + " adler="
                            + ((long) badInf.getAdler() & 0xffffffffL));
        }
        badInf.end();
    }

    private static void checkBufferDictionary(boolean direct) throws DataFormatException {
        byte[] dict = "DICT-KEY:shared-direct-buffer-terms:END".getBytes(StandardCharsets.UTF_8);
        byte[] wrong = "WRONG-DICT-CONTENT-FOR-BUFFER-TEST".getBytes(StandardCharsets.UTF_8);
        byte[] data =
                "DICT-KEY:shared-direct-buffer-terms:body;DICT-KEY:shared-direct-buffer-terms:END"
                        .getBytes(StandardCharsets.UTF_8);

        Deflater def = new Deflater();
        ByteBuffer dictBuf = sliceWithGuards(direct, dict, 0, dict.length);
        int dictStart = dictBuf.position();
        int dictLimit = dictBuf.limit();
        def.setDictionary(dictBuf);
        boolean defConsumedDict = dictBuf.position() == dictLimit;
        ByteBuffer inBuf = sliceWithGuards(direct, data, 0, data.length);
        ByteBuffer outBuf = allocBuf(direct, 256);
        def.setInput(inBuf);
        def.finish();
        while (!def.finished()) {
            def.deflate(outBuf);
        }
        outBuf.flip();
        byte[] wire = new byte[outBuf.remaining()];
        outBuf.get(wire);
        int defAdler = def.getAdler();
        def.end();

        Inflater inf = new Inflater();
        ByteBuffer infIn = sliceWithGuards(direct, wire, 0, wire.length);
        ByteBuffer infOut = allocBuf(direct, 256);
        inf.setInput(infIn);
        int first = inf.inflate(infOut);
        boolean needsDict = inf.needsDictionary();

        ByteBuffer wrongBuf = sliceWithGuards(direct, wrong, 0, wrong.length);
        String wrongRes;
        try {
            inf.setDictionary(wrongBuf);
            wrongRes = "no_throw";
        } catch (IllegalArgumentException e) {
            wrongRes = "IAE";
        }

        dictBuf.position(dictStart);
        inf.setDictionary(dictBuf);
        boolean infConsumedDict = dictBuf.position() == dictLimit;
        while (!inf.finished()) {
            inf.inflate(infOut);
        }
        infOut.flip();
        byte[] restored = new byte[infOut.remaining()];
        infOut.get(restored);

        Adler32 a = new Adler32();
        a.update(data);
        System.out.println(
                "dict direct="
                        + direct
                        + " defDictConsumed="
                        + defConsumedDict
                        + " infDictConsumed="
                        + infConsumedDict
                        + " first="
                        + first
                        + " needsDict="
                        + needsDict
                        + " wrong="
                        + wrongRes
                        + " eq="
                        + Arrays.equals(data, restored)
                        + " adlerMatch="
                        + (((long) defAdler & 0xffffffffL) == a.getValue()
                                && inf.getAdler() == defAdler));
        inf.end();
    }

    private static void checkReadOnlyAndNull(boolean direct) throws DataFormatException {
        byte[] src = "read-only-input-buffer-payload".getBytes(StandardCharsets.UTF_8);
        ByteBuffer roIn = sliceWithGuards(direct, src, 0, src.length).asReadOnlyBuffer();
        ByteBuffer out = allocBuf(direct, 128);

        Deflater def = new Deflater();
        def.setInput(roIn);
        def.finish();
        while (!def.finished()) {
            def.deflate(out);
        }
        out.flip();
        byte[] wire = new byte[out.remaining()];
        out.get(wire);

        // Read-only output buffer must throw ReadOnlyBufferException on deflate().
        def.reset();
        def.setInput(src);
        ByteBuffer roOut = allocBuf(direct, 32).asReadOnlyBuffer();
        String defRoOut;
        try {
            def.deflate(roOut);
            defRoOut = "no_throw";
        } catch (ReadOnlyBufferException e) {
            defRoOut = "ROBE";
        }

        // Zero-remaining output buffer returns 0.
        ByteBuffer emptyOut = allocBuf(direct, 8);
        emptyOut.limit(0);
        int zeroDef = def.deflate(emptyOut);
        def.end();

        // Read-only input allowed on Inflater; read-only output throws ReadOnlyBufferException.
        Inflater inf = new Inflater();
        ByteBuffer roWire = sliceWithGuards(direct, wire, 0, wire.length).asReadOnlyBuffer();
        inf.setInput(roWire);
        ByteBuffer plainBuf = allocBuf(direct, 128);
        while (!inf.finished()) {
            inf.inflate(plainBuf);
        }
        plainBuf.flip();
        byte[] restored = new byte[plainBuf.remaining()];
        plainBuf.get(restored);

        inf.reset();
        inf.setInput(wire);
        String infRoOut;
        try {
            inf.inflate(roOut);
            infRoOut = "no_throw";
        } catch (ReadOnlyBufferException e) {
            infRoOut = "ROBE";
        }
        int zeroInf = inf.inflate(emptyOut);
        inf.end();

        // Null ByteBuffer checks.
        Deflater nd = new Deflater();
        Inflater ni = new Inflater();
        String n1 = catchNpe(() -> nd.setInput((ByteBuffer) null));
        String n2 = catchNpe(() -> nd.deflate((ByteBuffer) null));
        String n3 = catchNpe(() -> nd.setDictionary((ByteBuffer) null));
        String n4 = catchNpe(() -> ni.setInput((ByteBuffer) null));
        String n5 = catchNpe(() -> ni.inflate((ByteBuffer) null));
        String n6 = catchNpe(() -> ni.setDictionary((ByteBuffer) null));
        nd.end();
        ni.end();

        System.out.println(
                "ro direct="
                        + direct
                        + " eq="
                        + Arrays.equals(src, restored)
                        + " defRo="
                        + defRoOut
                        + " infRo="
                        + infRoOut
                        + " zero="
                        + (zeroDef == 0 && zeroInf == 0)
                        + " npe="
                        + (n1 + n2 + n3 + n4 + n5 + n6));
    }

    private static String catchNpe(ThrowingRunnable r) {
        try {
            r.run();
            return "X";
        } catch (NullPointerException e) {
            return "N";
        } catch (Exception e) {
            return "E";
        }
    }

    private interface ThrowingRunnable {
        void run() throws Exception;
    }
}
